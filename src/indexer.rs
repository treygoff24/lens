use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::Instant;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};

use crate::budget::Budget;
use crate::error::LensError;
use crate::normalize::{NormalizeOutput, normalize_image};
use crate::providers::Spend;
use crate::providers::cerebras::{
    CerebrasClient, ChatOpts, ChatResponse, JsonSchema, Message, ResponseFormat, json_repair,
    parse_repaired_json,
};
use crate::store::{
    DESCRIPTION_MAX, FILENAME_MAX, IndexRecord, KIND_MAX, Store, TAG_COUNT_MAX, TAG_MAX,
    TEXT_CONTENT_MAX, sanitize_index_text,
};
use crate::walk::{WalkedFile, partition_with_stale_all, walk_library};

pub const CAPTION_WORST_CASE_COST: f64 = 0.008;
pub const DEFAULT_INDEX_CONCURRENCY: usize = 25;
const MAX_DECODE_CONCURRENCY: usize = 4;
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

fn skip_report(rel_path: String, reason: impl ToString) -> SkipReport {
    SkipReport {
        rel_path,
        reason: reason.to_string(),
    }
}

fn failure_report(rel_path: String, error: impl ToString) -> FailureReport {
    FailureReport {
        rel_path,
        error: error.to_string(),
    }
}

pub type BudgetReport = crate::envelope::Budget;

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
    /// `terse` bounds transcription on retries after the full prompt exhausts
    /// the completion cap mid-JSON. It leaves `promptVersion` unchanged because
    /// the primary prompt and stored rows are unchanged.
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
                // Terse re-rolls also drop temperature: default-temp decoding
                // degenerates into repetition loops (". . .", "\n\n\n") or
                // hallucinated marketing copy on some photos until the token
                // cap truncates the JSON. Measured live 2026-07-01: temp 0.4
                // cleared 3/3 attempts on an image that failed 3/3 at default.
                temperature: terse.then_some(0.4),
            },
        )
    }

    fn spend_snapshot(&self) -> Spend {
        CerebrasClient::spend_snapshot(self)
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

    // Partition first so a stale model still distinguishes recaption candidates
    // from files that truly vanished and must be pruned.
    let freshness = partition_with_stale_all(&walked, &loaded.records, loaded.stale_all);

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

    // Retain stale rows until recaption succeeds; last-wins replaces successes
    // without losing good captions when normalization or captioning fails.
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
    let decode_gate = Arc::new(DecodeGate::new(MAX_DECODE_CONCURRENCY));
    let queue = Arc::new(Mutex::new(VecDeque::from(work)));
    let (tx, rx) = mpsc::channel();
    let workers = concurrency.max(1);

    let mut state = thread::scope(|scope| {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let gate = Arc::clone(&gate);
            let decode_gate = Arc::clone(&decode_gate);
            let tx = tx.clone();
            scope.spawn(move || {
                loop {
                    let (file, reservation) = {
                        let _gate = gate.lock().unwrap();
                        if budget.hit().is_some() {
                            return;
                        }
                        let file = queue.lock().unwrap().pop_front();
                        let Some(file) = file else {
                            return;
                        };
                        let spend_snapshot = chat.spend_snapshot();
                        let Some(reservation) =
                            budget.reserve(&spend_snapshot, CAPTION_WORST_CASE_COST)
                        else {
                            tx.send(Some(WorkerOutput::Skipped(skip_report(
                                file.rel_path,
                                "budget_refused",
                            ))))
                            .expect("receiver lives until workers finish");
                            tx.send(None).expect("receiver lives until workers finish");
                            return;
                        };
                        (file, reservation)
                    };

                    let output = caption_one(file, chat, &decode_gate);
                    drop(reservation);
                    tx.send(Some(output))
                        .expect("receiver lives until workers finish");
                }
            });
        }
        drop(tx);
        collect_worker_outputs(rx, store)
    })?;

    // Workers stop silently on a budget hit, so account for every queued file.
    if state.budget_hit {
        let leftover = queue.lock().unwrap().drain(..).collect::<Vec<_>>();
        for file in leftover {
            state
                .skipped
                .push(skip_report(file.rel_path, "budget_refused"));
        }
    }

    Ok(state)
}

#[derive(Debug)]
struct DecodeGate {
    available: Mutex<usize>,
    wake: Condvar,
}

impl DecodeGate {
    fn new(limit: usize) -> Self {
        Self {
            available: Mutex::new(limit.max(1)),
            wake: Condvar::new(),
        }
    }

    fn acquire(&self) -> DecodePermit<'_> {
        let mut available = self.available.lock().unwrap();
        while *available == 0 {
            available = self.wake.wait(available).unwrap();
        }
        *available -= 1;
        DecodePermit { gate: self }
    }
}

struct DecodePermit<'a> {
    gate: &'a DecodeGate,
}

impl Drop for DecodePermit<'_> {
    fn drop(&mut self) {
        *self.gate.available.lock().unwrap() += 1;
        self.gate.wake.notify_one();
    }
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

fn caption_one(file: WalkedFile, chat: &dyn CaptionChat, decode_gate: &DecodeGate) -> WorkerOutput {
    let ext = Path::new(&file.rel_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .expect("walked image has a UTF-8 extension")
        .to_string();

    let image = match file.open() {
        Ok(image) => image,
        Err(_) => {
            return WorkerOutput::Skipped(skip_report(
                file.rel_path,
                crate::normalize::NormalizeSkipReason::CorruptImage,
            ));
        }
    };
    let normalized = {
        let _permit = decode_gate.acquire();
        normalize_image(&image, &ext, false)
    };
    let normalized = match normalized {
        NormalizeOutput::Normalized(normalized) => normalized,
        NormalizeOutput::Skip { reason, .. } => {
            return WorkerOutput::Skipped(skip_report(file.rel_path, reason));
        }
    };

    match caption_normalized(&normalized.jpeg_bytes, chat) {
        Ok(caption) => WorkerOutput::Indexed(record_from_caption(file, caption)),
        Err(CaptionFailure::InvalidImageData) => {
            let reencoded = {
                let _permit = decode_gate.acquire();
                normalize_image(&image, &ext, true)
            };
            match reencoded {
                NormalizeOutput::Normalized(reencoded) => {
                    match caption_normalized(&reencoded.jpeg_bytes, chat) {
                        Ok(caption) => WorkerOutput::Indexed(record_from_caption(file, caption)),
                        Err(err) => WorkerOutput::Failed(failure_report(file.rel_path, err)),
                    }
                }
                NormalizeOutput::Skip { reason, .. } => {
                    WorkerOutput::Skipped(skip_report(file.rel_path, reason))
                }
            }
        }
        Err(err) => WorkerOutput::Failed(failure_report(file.rel_path, err)),
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[doc(hidden)]
pub struct CaptionPayload {
    pub description: String,
    pub filename: String,
    pub tags: Vec<String>,
    pub text_content: String,
    pub kind: String,
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
        Ok(response) => match parse_caption(&response.content) {
            Ok(caption) => Ok(caption),
            Err(_) => {
                // Parse failure is dominated by cap-truncated output: either a
                // text-dense image transcribed past the cap, or a degeneration
                // loop (model hallucinates repeating text in photo texture).
                // Bounded transcription and low temperature avoid both failure modes.
                let reroll = chat
                    .caption_chat(jpeg_bytes, true)
                    .map_err(map_caption_error)?;
                parse_caption(&reroll.content).or_else(|err| {
                    // Last resort for images that loop on every attempt: both
                    // responses reliably open with a good description string
                    // before degenerating. Salvage it into a degraded row —
                    // an honest thin record beats a permanent failure.
                    salvage_caption(&reroll.content)
                        .or_else(|| salvage_caption(&response.content))
                        .ok_or(err)
                })
            }
        },
        Err(err) => Err(map_caption_error(err)),
    }
}

/// Extracts a usable record from a cap-truncated caption response. Requires a
/// complete-enough `"description": "..."` opening; everything else defaults.
fn salvage_caption(content: &str) -> Option<CaptionPayload> {
    let repaired = json_repair(content);
    let start = repaired.find("\"description\"")?;
    let after_key = &repaired[start + "\"description\"".len()..];
    let quote = after_key.find('"')?;
    let body = &after_key[quote + 1..];
    let mut end = body.len();
    let mut chars = body.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                let _ = chars.next();
            }
            '"' => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    let mut description: String = body[..end].chars().take(500).collect();
    while description.ends_with('\\') {
        description.pop();
    }
    let description = description.trim().to_string();
    if description.len() < 20 {
        return None;
    }
    let filename = description
        .split_whitespace()
        .take(6)
        .map(|word| {
            word.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    Some(CaptionPayload {
        description,
        filename,
        tags: Vec::new(),
        text_content: String::new(),
        kind: "photo".to_string(),
    })
}

fn map_caption_error(err: LensError) -> CaptionFailure {
    if err.to_string().contains("invalid_image_data") {
        CaptionFailure::InvalidImageData
    } else {
        CaptionFailure::Upstream(err.to_string())
    }
}

fn parse_caption(content: &str) -> Result<CaptionPayload, CaptionFailure> {
    parse_repaired_json(content).map_err(|_| CaptionFailure::Parse)
}

fn record_from_caption(file: WalkedFile, caption: CaptionPayload) -> IndexRecord {
    IndexRecord {
        rel_path: file.rel_path,
        size: file.size,
        mtime_ns: file.mtime_ns,
        description: sanitize_index_text(&caption.description, DESCRIPTION_MAX),
        filename: sanitize_index_text(&caption.filename, FILENAME_MAX),
        tags: caption
            .tags
            .into_iter()
            .take(TAG_COUNT_MAX)
            .map(|tag| sanitize_index_text(&tag, TAG_MAX))
            .collect(),
        text_content: sanitize_index_text(&caption.text_content, TEXT_CONTENT_MAX),
        kind: sanitize_index_text(&caption.kind, KIND_MAX),
    }
}

fn caption_response_format() -> ResponseFormat {
    ResponseFormat::json_schema(
        "image_index",
        JsonSchema::object(
            [
                (
                    "description",
                    JsonSchema::string().described("2-3 sentence description"),
                ),
                (
                    "filename",
                    JsonSchema::string().described(
                        "short descriptive filename, lowercase-hyphenated, no extension",
                    ),
                ),
                (
                    "tags",
                    JsonSchema::array(JsonSchema::string()).described("3-6 short tags"),
                ),
                (
                    "text_content",
                    JsonSchema::string()
                        .described("any legible text in the image, verbatim, empty string if none"),
                ),
                (
                    "kind",
                    JsonSchema::string().one_of([
                        "photo",
                        "screenshot",
                        "document",
                        "diagram",
                        "graphic",
                        "map",
                        "other",
                    ]),
                ),
            ],
            ["description", "filename", "tags", "text_content", "kind"],
        ),
    )
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
                .expect("scripted caption response");
            next.map(|content| ChatResponse {
                content,
                usage: Default::default(),
                wall_time_ms: 1,
            })
        }

        fn spend_snapshot(&self) -> Spend {
            self.spend.lock().unwrap().clone()
        }
    }

    fn caption_json(name: &str) -> String {
        serde_json::to_string(&CaptionPayload {
            description: format!("description {name}"),
            filename: name.to_string(),
            tags: vec!["tag".into()],
            text_content: String::new(),
            kind: "photo".into(),
        })
        .unwrap()
    }

    #[test]
    fn caption_fields_are_sanitized_before_storage() {
        let dir = fixture_library(1);
        let file = crate::walk::walk_library(dir.path()).unwrap().remove(0);
        let record = record_from_caption(
            file,
            CaptionPayload {
                description: format!("description\n{}", "x".repeat(DESCRIPTION_MAX * 2)),
                filename: "name\0".repeat(FILENAME_MAX),
                tags: (0..TAG_COUNT_MAX * 2)
                    .map(|_| "tag\r".repeat(TAG_MAX))
                    .collect(),
                text_content: "text\n".repeat(TEXT_CONTENT_MAX),
                kind: "photo\u{1b}".repeat(KIND_MAX),
            },
        );

        assert!(record.description.chars().count() <= DESCRIPTION_MAX);
        assert!(record.filename.chars().count() <= FILENAME_MAX);
        assert!(record.text_content.chars().count() <= TEXT_CONTENT_MAX);
        assert!(record.kind.chars().count() <= KIND_MAX);
        assert_eq!(record.tags.len(), TAG_COUNT_MAX);
        assert!(record.tags.iter().all(|tag| tag.chars().count() <= TAG_MAX));
        assert!(
            record
                .description
                .chars()
                .chain(record.filename.chars())
                .chain(record.text_content.chars())
                .chain(record.kind.chars())
                .all(|ch| !ch.is_control())
        );
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
        assert_eq!(chat.terse_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn salvage_extracts_description_from_truncated_response() {
        let truncated = r#"{"description": "High-angle aerial view of a luxury resort with pools", "filename": "resort", "tags": ["a"], "text_content": "LUXURY RESORT LUXURY RESORT LUXURY RES"#;
        let payload = salvage_caption(truncated).unwrap();
        assert_eq!(
            payload.description,
            "High-angle aerial view of a luxury resort with pools"
        );
        assert_eq!(payload.filename, "highangle-aerial-view-of-a-luxury");
        assert_eq!(payload.kind, "photo");
        assert!(payload.tags.is_empty());
        assert!(payload.text_content.is_empty());

        assert!(salvage_caption("not json at all").is_none());
        assert!(salvage_caption(r#"{"description": "too short"#).is_none());
    }

    #[test]
    fn double_parse_fail_with_salvageable_description_indexes_degraded_row() {
        let dir = fixture_library(1);
        let truncated = r#"{"description": "An aerial photo of winding blue pools beside the ocean", "text_content": ". . . . . . . ."#;
        let chat = ScriptedChat::new(vec![Ok("not json".into()), Ok(truncated.into())]);
        let outcome = run(dir.path(), &chat, &Budget::new(None, None));

        assert_eq!(outcome.report().indexed, 1);
        assert_eq!(outcome.report().failed.len(), 0);
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
        // Binary floating point puts the second reservation just above 0.009.
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
        let dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(store_dir.path()).unwrap();

        let good_path = dir.path().join("a.png");
        let img = ImageBuffer::from_pixel(2, 2, Rgba([10u8, 0, 0, 255]));
        img.save(&good_path).unwrap();
        let walked = crate::walk::walk_library(dir.path()).unwrap();
        let file = &walked[0];
        store.append(&record_for(file, "original caption")).unwrap();
        store.ensure_meta(dir.path(), "m").unwrap();

        // An immediate rewrite may preserve mtime and hide the stale file.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&good_path, b"\x13\x99\x00not an image\xff\x00").unwrap();

        let chat = ScriptedChat::new(vec![]);
        let mut opts = IndexOptions::new("m");
        opts.concurrency = 1;
        let outcome =
            index_library(dir.path(), &store, &chat, &Budget::new(None, None), &opts).unwrap();

        let loaded = store.load("m").unwrap();
        let found = loaded
            .records
            .iter()
            .find(|r| r.rel_path == "a.png")
            .unwrap();
        assert_eq!(found.description, "original caption");
        assert_eq!(outcome.report().indexed, 0);
    }

    #[test]
    fn f1_stale_file_successful_recaption_new_record_wins() {
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

        // An immediate rewrite may preserve mtime and hide the stale file.
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
        let dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(store_dir.path()).unwrap();

        let keep_path = dir.path().join("keep.png");
        let img = ImageBuffer::from_pixel(2, 2, Rgba([10u8, 0, 0, 255]));
        img.save(&keep_path).unwrap();

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
        store.ensure_meta(dir.path(), "old-model").unwrap();

        let chat = ScriptedChat::new(vec![Ok(caption_json("keep"))]);
        let mut opts = IndexOptions::new("new-model");
        opts.concurrency = 1;
        let outcome =
            index_library(dir.path(), &store, &chat, &Budget::new(None, None), &opts).unwrap();

        assert_eq!(outcome.report().vanished, 1);
        assert_eq!(outcome.report().pruned, 1);
        let loaded = store.load("new-model").unwrap();
        assert!(loaded.records.iter().all(|r| r.rel_path != "gone.png"));
    }
}
