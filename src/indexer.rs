use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Instant;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::budget::Budget;
use crate::error::LensError;
use crate::normalize::{NormalizeOutput, normalize_image};
use crate::providers::Spend;
use crate::providers::cerebras::{CerebrasClient, ChatOpts, ChatResponse, Message, json_repair};
use crate::store::{IndexRecord, Store};
use crate::walk::{WalkedFile, partition_freshness, walk_library};

pub const CAPTION_WORST_CASE_COST: f64 = 0.008;
pub const DEFAULT_INDEX_CONCURRENCY: usize = 25;
const PROMPT: &str = "Index this image for a searchable photo library. Transcribe any legible text verbatim into text_content (truncate past ~100 words).";
const TERSE_PROMPT: &str = "Index this image for a searchable photo library. This image is text-dense: put only the first ~50 words of legible text in text_content and keep description to two sentences. The JSON must be complete.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexReport {
    pub library_path: String,
    pub indexed: usize,
    pub skipped: Vec<SkipReport>,
    pub failed: Vec<FailureReport>,
    pub pruned: usize,
    pub budget: BudgetReport,
    pub duration_ms: u64,
    pub total_files: usize,
    pub fresh: usize,
    pub stale: usize,
    pub new: usize,
    pub vanished: usize,
    /// Warnings collected during the run (store-load warnings, stale_all
    /// notice, lock-steal warning). In JSON mode these ride the envelope
    /// instead of raw stderr (F2+F3).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkipReport {
    pub rel_path: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureReport {
    pub rel_path: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BudgetReport {
    pub hit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexOutcome {
    Complete(IndexReport),
    Partial(IndexReport),
}

impl IndexOutcome {
    pub fn report(&self) -> &IndexReport {
        match self {
            IndexOutcome::Complete(report) | IndexOutcome::Partial(report) => report,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub model: String,
    pub concurrency: usize,
}

impl IndexOptions {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            concurrency: DEFAULT_INDEX_CONCURRENCY,
        }
    }
}

pub trait CaptionChat: Sync {
    /// `terse` selects the bounded-transcription prompt used on re-rolls:
    /// text-dense images (brochures, infographics) can blow the completion cap
    /// mid-JSON on the full prompt, which parses as truncated JSON every time.
    /// Measured live 2026-07-01: 29/1,100 corpus images failed deterministically
    /// until the terse re-roll was added. Does not bump promptVersion — the
    /// primary prompt is unchanged and terse mode only runs for files that
    /// previously produced no index row at all.
    fn caption_chat(&self, jpeg_bytes: &[u8], terse: bool) -> Result<ChatResponse, LensError>;
    fn spend_snapshot(&self) -> Spend;
}

impl CaptionChat for CerebrasClient {
    fn caption_chat(&self, jpeg_bytes: &[u8], terse: bool) -> Result<ChatResponse, LensError> {
        let data_uri = format!("data:image/jpeg;base64,{}", STANDARD.encode(jpeg_bytes));
        let prompt = if terse { TERSE_PROMPT } else { PROMPT };
        self.chat(
            &[Message::user_with_image(prompt, data_uri)],
            ChatOpts {
                max_completion_tokens: Some(1200),
                response_format: Some(caption_response_format()),
                ..ChatOpts::default()
            },
        )
    }

    fn spend_snapshot(&self) -> Spend {
        self.spend()
            .lock()
            .map(|spend| spend.clone())
            .unwrap_or_default()
    }
}

pub fn index_library(
    library_path: &Path,
    store: &Store,
    chat: &(dyn CaptionChat + Sync),
    budget: &Budget,
    options: &IndexOptions,
) -> Result<IndexOutcome, LensError> {
    let start = Instant::now();
    let (_lock, lock_warning) = store.lock()?;

    // F2+F3: collect warnings into a Vec<String> instead of eprintln. The
    // commands layer decides whether to print them to stderr (human mode only).
    let mut warnings = Vec::new();
    if let Some(w) = lock_warning {
        warnings.push(w);
    }

    let walked = walk_library(library_path)?;
    let loaded = store.load(&options.model)?;
    warnings.extend(loaded.warnings);
    if loaded.stale_all {
        warnings.push("index metadata is stale; recaptioning all files".to_string());
    }

    // Always partition so we know which files truly vanished from the library
    // (F2: stale_all must still prune vanished files, not fabricate an empty
    // vanished list). When the whole index is stale, move everything that still
    // exists into the stale bucket so it gets recaptioned, while keeping the
    // real vanished list.
    let mut freshness = partition_freshness(&walked, &loaded.records);
    if loaded.stale_all {
        let mut recaption = std::mem::take(&mut freshness.fresh);
        recaption.append(&mut freshness.new);
        freshness.stale.extend(recaption);
        freshness.new = Vec::new();
        freshness.fresh = Vec::new();
    }

    let mut work = freshness
        .stale
        .iter()
        .chain(freshness.new.iter())
        .cloned()
        .collect::<Vec<_>>();
    work.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

    let vanished_paths = freshness
        .vanished
        .iter()
        .map(|record| record.rel_path.clone())
        .collect::<HashSet<_>>();

    let run = run_caption_work(work, chat, budget, options.concurrency, store)?;

    // F1: keep old records for stale paths (a recaption that skipped/failed
    // must not lose its previous good caption). Only vanished paths get
    // filtered. New records are appended after old ones so rewrite_last_wins
    // resolves replacements with the new record winning.
    let mut records = loaded
        .records
        .into_iter()
        .filter(|record| !vanished_paths.contains(&record.rel_path))
        .collect::<Vec<_>>();
    records.extend(run.indexed_records);
    let before = records.len();
    let after = store.rewrite_last_wins(records)?;
    store.ensure_meta(library_path, &options.model)?;
    let duplicate_pruned = before.saturating_sub(after);

    let report = IndexReport {
        library_path: library_path.to_string_lossy().to_string(),
        indexed: run.indexed,
        skipped: run.skipped,
        failed: run.failed,
        pruned: freshness.vanished.len() + duplicate_pruned,
        budget: BudgetReport {
            hit: budget.hit().map(ToString::to_string),
        },
        duration_ms: start.elapsed().as_millis() as u64,
        total_files: walked.len(),
        fresh: freshness.fresh.len(),
        stale: freshness.stale.len(),
        new: freshness.new.len(),
        vanished: freshness.vanished.len(),
        warnings,
    };

    if run.budget_hit {
        Ok(IndexOutcome::Partial(report))
    } else {
        Ok(IndexOutcome::Complete(report))
    }
}

#[derive(Debug, Default)]
struct RunState {
    indexed: usize,
    indexed_records: Vec<IndexRecord>,
    skipped: Vec<SkipReport>,
    failed: Vec<FailureReport>,
    budget_hit: bool,
}

#[derive(Debug)]
enum WorkerOutput {
    Indexed(IndexRecord),
    Skipped(SkipReport),
    Failed(FailureReport),
}

fn run_caption_work(
    work: Vec<WalkedFile>,
    chat: &(dyn CaptionChat + Sync),
    budget: &Budget,
    concurrency: usize,
    store: &Store,
) -> Result<RunState, LensError> {
    let gate = Arc::new(Mutex::new(()));
    let queue = Arc::new(Mutex::new(VecDeque::from(work)));
    let (tx, rx) = mpsc::channel();
    let workers = concurrency.max(1);

    let mut state = thread::scope(|scope| {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let gate = Arc::clone(&gate);
            let tx = tx.clone();
            scope.spawn(move || {
                loop {
                    let (file, reservation) = {
                        let _gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        if budget.hit().is_some() {
                            return;
                        }
                        let file = queue
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .pop_front();
                        let Some(file) = file else {
                            return;
                        };
                        let spend_snapshot = chat.spend_snapshot();
                        let Some(reservation) =
                            budget.reserve(&spend_snapshot, CAPTION_WORST_CASE_COST)
                        else {
                            let _ = tx.send(Some(WorkerOutput::Skipped(SkipReport {
                                rel_path: file.rel_path,
                                reason: "budget_refused".to_string(),
                            })));
                            let _ = tx.send(None);
                            return;
                        };
                        (file, reservation)
                    };

                    let output = caption_one(file, chat);
                    reservation.settle(0.0);
                    let _ = tx.send(Some(output));
                }
            });
        }
        drop(tx);
        collect_worker_outputs(rx, store)
    })?;

    // F3: workers that see budget.hit() return silently, leaving the rest of
    // the queue unreported. Drain the leftover queue and emit a budget_refused
    // skip for every file that was never picked up.
    if state.budget_hit {
        let leftover = queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .collect::<Vec<_>>();
        for file in leftover {
            state.skipped.push(SkipReport {
                rel_path: file.rel_path,
                reason: "budget_refused".to_string(),
            });
        }
    }

    Ok(state)
}

fn collect_worker_outputs(
    rx: mpsc::Receiver<Option<WorkerOutput>>,
    store: &Store,
) -> Result<RunState, LensError> {
    let mut state = RunState::default();
    for message in rx {
        match message {
            None => state.budget_hit = true,
            Some(WorkerOutput::Indexed(record)) => {
                store.append(&record)?;
                state.indexed += 1;
                state.indexed_records.push(record);
            }
            Some(WorkerOutput::Skipped(skip)) => state.skipped.push(skip),
            Some(WorkerOutput::Failed(failure)) => state.failed.push(failure),
        }
    }
    Ok(state)
}

fn caption_one(file: WalkedFile, chat: &dyn CaptionChat) -> WorkerOutput {
    let ext = Path::new(&file.rel_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_string();

    let normalized = match normalize_image(&file.abs_path, &ext, false) {
        NormalizeOutput::Normalized(normalized) => normalized,
        NormalizeOutput::Skip { reason, .. } => {
            return WorkerOutput::Skipped(SkipReport {
                rel_path: file.rel_path,
                reason: reason.to_string(),
            });
        }
    };

    match caption_normalized(&normalized.jpeg_bytes, chat) {
        Ok(caption) => WorkerOutput::Indexed(record_from_caption(file, caption)),
        Err(CaptionFailure::InvalidImageData) => {
            match normalize_image(&file.abs_path, &ext, true) {
                NormalizeOutput::Normalized(reencoded) => {
                    match caption_normalized(&reencoded.jpeg_bytes, chat) {
                        Ok(caption) => WorkerOutput::Indexed(record_from_caption(file, caption)),
                        Err(err) => WorkerOutput::Failed(FailureReport {
                            rel_path: file.rel_path,
                            error: err.to_string(),
                        }),
                    }
                }
                NormalizeOutput::Skip { reason, .. } => WorkerOutput::Skipped(SkipReport {
                    rel_path: file.rel_path,
                    reason: reason.to_string(),
                }),
            }
        }
        Err(err) => WorkerOutput::Failed(FailureReport {
            rel_path: file.rel_path,
            error: err.to_string(),
        }),
    }
}

#[derive(Debug, Clone, Deserialize)]
struct CaptionPayload {
    description: String,
    filename: String,
    tags: Vec<String>,
    text_content: String,
    kind: String,
}

#[derive(Debug)]
enum CaptionFailure {
    Parse,
    InvalidImageData,
    Upstream(String),
}

impl std::fmt::Display for CaptionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptionFailure::Parse => f.write_str("failed to parse caption JSON after retry"),
            CaptionFailure::InvalidImageData => f.write_str("invalid_image_data"),
            CaptionFailure::Upstream(message) => f.write_str(message),
        }
    }
}

fn caption_normalized(
    jpeg_bytes: &[u8],
    chat: &dyn CaptionChat,
) -> Result<CaptionPayload, CaptionFailure> {
    match chat.caption_chat(jpeg_bytes, false) {
        Ok(response) => parse_caption(&response.content).or_else(|_| {
            // Parse failure is dominated by cap-truncated transcriptions of
            // text-dense images; the terse prompt bounds the output so the
            // re-roll can complete.
            chat.caption_chat(jpeg_bytes, true)
                .map_err(map_caption_error)
                .and_then(|reroll| parse_caption(&reroll.content))
        }),
        Err(err) if err.to_string().contains("invalid_image_data") => {
            Err(CaptionFailure::InvalidImageData)
        }
        Err(err) => Err(map_caption_error(err)),
    }
}

fn map_caption_error(err: LensError) -> CaptionFailure {
    if err.to_string().contains("invalid_image_data") {
        CaptionFailure::InvalidImageData
    } else {
        CaptionFailure::Upstream(err.to_string())
    }
}

fn parse_caption(content: &str) -> Result<CaptionPayload, CaptionFailure> {
    let repaired = json_repair(content);
    serde_json::from_str(&repaired).map_err(|_| CaptionFailure::Parse)
}

fn record_from_caption(file: WalkedFile, caption: CaptionPayload) -> IndexRecord {
    IndexRecord {
        rel_path: file.rel_path,
        size: file.size,
        mtime_ns: file.mtime_ns,
        description: caption.description,
        filename: caption.filename,
        tags: caption.tags,
        text_content: caption.text_content,
        kind: caption.kind,
    }
}

fn caption_response_format() -> Value {
    json!({
        "type": "json_schema",
        "json_schema": {
            "name": "image_index",
            "strict": true,
            "schema": {
                "type": "object",
                "properties": {
                    "description": {"type": "string", "description": "2-3 sentence description"},
                    "filename": {"type": "string", "description": "short descriptive filename, lowercase-hyphenated, no extension"},
                    "tags": {"type": "array", "items": {"type": "string"}, "description": "3-6 short tags"},
                    "text_content": {"type": "string", "description": "any legible text in the image, verbatim, empty string if none"},
                    "kind": {"type": "string", "enum": ["photo", "screenshot", "document", "diagram", "graphic", "map", "other"]},
                },
                "required": ["description", "filename", "tags", "text_content", "kind"],
                "additionalProperties": false,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Provider;
    use image::{ImageBuffer, Rgba};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct ScriptedChat {
        responses: Mutex<VecDeque<Result<String, LensError>>>,
        calls: AtomicUsize,
        terse_calls: AtomicUsize,
        spend: Mutex<Spend>,
    }

    impl ScriptedChat {
        fn new(responses: Vec<Result<String, LensError>>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
                calls: AtomicUsize::new(0),
                terse_calls: AtomicUsize::new(0),
                spend: Mutex::new(Spend::default()),
            }
        }
    }

    impl CaptionChat for ScriptedChat {
        fn caption_chat(&self, _jpeg_bytes: &[u8], terse: bool) -> Result<ChatResponse, LensError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if terse {
                self.terse_calls.fetch_add(1, Ordering::SeqCst);
            }
            let mut spend = self.spend.lock().unwrap();
            spend.call_count += 1;
            spend.prompt_tokens += 10;
            spend.completion_tokens += 5;
            spend.dollars += 0.001;
            drop(spend);

            let next = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(caption_json("fallback")));
            next.map(|content| ChatResponse {
                content,
                tool_calls: Vec::new(),
                usage: Default::default(),
                wall_time_ms: 1,
            })
        }

        fn spend_snapshot(&self) -> Spend {
            self.spend.lock().unwrap().clone()
        }
    }

    fn caption_json(name: &str) -> String {
        json!({
            "description": format!("description {name}"),
            "filename": name,
            "tags": ["tag"],
            "text_content": "",
            "kind": "photo",
        })
        .to_string()
    }

    fn fixture_library(count: usize) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..count {
            let path = dir.path().join(format!("{i}.png"));
            let img = ImageBuffer::from_pixel(2, 2, Rgba([i as u8, 0, 0, 255]));
            img.save(path).unwrap();
        }
        dir
    }

    fn run(dir: &Path, chat: &ScriptedChat, budget: &Budget) -> IndexOutcome {
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(store_dir.path()).unwrap();
        let mut opts = IndexOptions::new("m");
        opts.concurrency = 1; // deterministic scripted response ordering.
        index_library(dir, &store, chat, budget, &opts).unwrap()
    }

    #[test]
    fn happy_path_indexes_n_files() {
        let dir = fixture_library(3);
        let chat = ScriptedChat::new(vec![
            Ok(caption_json("a")),
            Ok(caption_json("b")),
            Ok(caption_json("c")),
        ]);
        let outcome = run(dir.path(), &chat, &Budget::new(None, None));

        assert!(matches!(outcome, IndexOutcome::Complete(_)));
        assert_eq!(outcome.report().indexed, 3);
        assert_eq!(outcome.report().failed.len(), 0);
    }

    #[test]
    fn parse_fail_rerolls_then_succeeds() {
        let dir = fixture_library(1);
        let chat = ScriptedChat::new(vec![Ok("not json".into()), Ok(caption_json("rerolled"))]);
        let outcome = run(dir.path(), &chat, &Budget::new(None, None));

        assert_eq!(outcome.report().indexed, 1);
        assert_eq!(chat.calls.load(Ordering::SeqCst), 2);
        // The re-roll must use the terse prompt (cap-truncation defense).
        assert_eq!(chat.terse_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn double_parse_fail_records_failure() {
        let dir = fixture_library(1);
        let chat = ScriptedChat::new(vec![Ok("not json".into()), Ok("still bad".into())]);
        let outcome = run(dir.path(), &chat, &Budget::new(None, None));

        assert_eq!(outcome.report().indexed, 0);
        assert_eq!(outcome.report().failed.len(), 1);
    }

    #[test]
    fn invalid_image_data_retries_force_reencode_path() {
        let dir = fixture_library(1);
        let chat = ScriptedChat::new(vec![
            Err(LensError::upstream("invalid_image_data").with_provider(Provider::Cerebras)),
            Ok(caption_json("retry")),
        ]);
        let outcome = run(dir.path(), &chat, &Budget::new(None, None));

        assert_eq!(outcome.report().indexed, 1);
        assert_eq!(chat.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn budget_refusal_mid_run_returns_partial_and_stops_launches() {
        let dir = fixture_library(3);
        let chat = ScriptedChat::new(vec![Ok(caption_json("one")), Ok(caption_json("two"))]);
        let outcome = run(dir.path(), &chat, &Budget::new(Some(0.009), None));

        assert!(matches!(outcome, IndexOutcome::Partial(_)));
        // With the epsilon fix (F4), the f64 representation error no longer
        // false-trips on the second reservation: spend 0.001 + projected 0.008
        // = 0.009 which is within cap 0.009 + 1e-9. The third reservation
        // (spend 0.002 + 0.008 = 0.010) hits the cap, so exactly 2 are indexed.
        assert_eq!(outcome.report().indexed, 2);
        assert_eq!(outcome.report().budget.hit.as_deref(), Some("dollars"));
    }

    #[test]
    fn budget_hit_drains_remaining_queue_as_refused() {
        let dir = fixture_library(5);
        let chat = ScriptedChat::new(vec![Ok(caption_json("one"))]);
        let outcome = run(dir.path(), &chat, &Budget::new(Some(0.008), None));

        assert!(matches!(outcome, IndexOutcome::Partial(_)));
        assert_eq!(outcome.report().indexed, 1);
        let refused = outcome
            .report()
            .skipped
            .iter()
            .filter(|s| s.reason == "budget_refused")
            .count();
        assert_eq!(refused, 4);
        assert_eq!(outcome.report().budget.hit.as_deref(), Some("dollars"));
    }

    #[test]
    fn spend_arithmetic_comes_from_mock_usage() {
        let dir = fixture_library(2);
        let chat = ScriptedChat::new(vec![Ok(caption_json("a")), Ok(caption_json("b"))]);
        let outcome = run(dir.path(), &chat, &Budget::new(None, None));

        assert_eq!(outcome.report().indexed, 2);
        let spend = chat.spend_snapshot();
        assert_eq!(spend.call_count, 2);
        assert!((spend.dollars - 0.002).abs() < f64::EPSILON);
    }

    fn record_for(file: &WalkedFile, description: &str) -> IndexRecord {
        IndexRecord {
            rel_path: file.rel_path.clone(),
            size: file.size,
            mtime_ns: file.mtime_ns,
            description: description.to_string(),
            filename: "name".to_string(),
            tags: vec!["tag".to_string()],
            text_content: String::new(),
            kind: "photo".to_string(),
        }
    }

    #[test]
    fn f1_stale_file_skip_keeps_old_caption() {
        // A stale file whose recaption is skipped (corrupt fixture) must keep
        // its previous good caption in the store.
        let dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(store_dir.path()).unwrap();

        // Create a good image and index it first.
        let good_path = dir.path().join("a.png");
        let img = ImageBuffer::from_pixel(2, 2, Rgba([10u8, 0, 0, 255]));
        img.save(&good_path).unwrap();
        let walked = crate::walk::walk_library(dir.path()).unwrap();
        let file = &walked[0];
        store.append(&record_for(file, "original caption")).unwrap();
        store.ensure_meta(dir.path(), "m").unwrap();

        // Now corrupt the file (change bytes + mtime) so normalize skips it.
        // Sleep to ensure the mtime changes from the original walk.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&good_path, b"\x13\x99\x00not an image\xff\x00").unwrap();

        let chat = ScriptedChat::new(vec![]);
        let mut opts = IndexOptions::new("m");
        opts.concurrency = 1;
        let outcome =
            index_library(dir.path(), &store, &chat, &Budget::new(None, None), &opts).unwrap();

        // The skip should not lose the old caption.
        let loaded = store.load("m").unwrap();
        let found = loaded
            .records
            .iter()
            .find(|r| r.rel_path == "a.png")
            .unwrap();
        assert_eq!(found.description, "original caption");
        // It was skipped, not indexed.
        assert_eq!(outcome.report().indexed, 0);
    }

    #[test]
    fn f1_stale_file_successful_recaption_new_record_wins() {
        // A stale file that gets successfully recaptioned should have the new
        // caption replace the old one.
        let dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(store_dir.path()).unwrap();

        let good_path = dir.path().join("a.png");
        let img = ImageBuffer::from_pixel(2, 2, Rgba([10u8, 0, 0, 255]));
        img.save(&good_path).unwrap();
        let walked = crate::walk::walk_library(dir.path()).unwrap();
        let file = &walked[0];
        store.append(&record_for(file, "old caption")).unwrap();
        store.ensure_meta(dir.path(), "m").unwrap();

        // Touch the file to make it stale (sleep ensures mtime differs).
        std::thread::sleep(std::time::Duration::from_millis(20));
        let img2 = ImageBuffer::from_pixel(2, 2, Rgba([20u8, 0, 0, 255]));
        img2.save(&good_path).unwrap();

        let chat = ScriptedChat::new(vec![Ok(caption_json("new caption"))]);
        let mut opts = IndexOptions::new("m");
        opts.concurrency = 1;
        let _outcome =
            index_library(dir.path(), &store, &chat, &Budget::new(None, None), &opts).unwrap();

        let loaded = store.load("m").unwrap();
        let found = loaded
            .records
            .iter()
            .find(|r| r.rel_path == "a.png")
            .unwrap();
        assert_eq!(found.description, "description new caption");
    }

    #[test]
    fn f2_stale_all_prunes_vanished_files() {
        // When the whole index is stale (model mismatch), files that were
        // deleted from the library should still be pruned, not kept.
        let dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(store_dir.path()).unwrap();

        // Create one file on disk.
        let keep_path = dir.path().join("keep.png");
        let img = ImageBuffer::from_pixel(2, 2, Rgba([10u8, 0, 0, 255]));
        img.save(&keep_path).unwrap();

        // Store has a record for a file that no longer exists (vanished).
        let vanished_record = IndexRecord {
            rel_path: "gone.png".to_string(),
            size: 100,
            mtime_ns: 1,
            description: "should be pruned".to_string(),
            filename: "name".to_string(),
            tags: vec!["tag".to_string()],
            text_content: String::new(),
            kind: "photo".to_string(),
        };
        store.append(&vanished_record).unwrap();
        // Use a different model to trigger stale_all.
        store.ensure_meta(dir.path(), "old-model").unwrap();

        let chat = ScriptedChat::new(vec![Ok(caption_json("keep"))]);
        let mut opts = IndexOptions::new("new-model");
        opts.concurrency = 1;
        let outcome =
            index_library(dir.path(), &store, &chat, &Budget::new(None, None), &opts).unwrap();

        // The vanished file should be pruned.
        assert_eq!(outcome.report().vanished, 1);
        assert_eq!(outcome.report().pruned, 1);
        let loaded = store.load("new-model").unwrap();
        assert!(loaded.records.iter().all(|r| r.rel_path != "gone.png"));
    }
}
