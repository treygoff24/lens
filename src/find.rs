use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::budget::Budget;
use crate::error::{LensError, Provider};
use crate::providers::cerebras::{CerebrasClient, ChatOpts, ChatResponse, Message, json_repair};
use crate::providers::{SharedSpend, Spend};
use crate::store::IndexRecord;

const FIND_MAX_COMPLETION_TOKENS: u64 = 200;
/// Estimated-token budget per model call. `estimate_tokens` is deliberately
/// conservative (bytes/3 ≈ 1.3× real tokens for this line format, measured
/// live 2026-07-01: the 1,100-image corpus estimates 92K but is ~70K real),
/// so 100K estimated ≈ 75-77K real — comfortable margin under the 131K
/// context window. At 70K the same corpus was needlessly chunked: 2× cost,
/// no byte-stable prefix, no prompt-cache hit on repeat queries.
const CHUNK_TOKEN_CAP: usize = 100_000;
const GEMMA_INPUT_PER_MTOK: f64 = 2.15;
const GEMMA_OUTPUT_PER_MTOK: f64 = 2.70;
/// Maximum description chars in a serialized index line. Normal captions are
/// ≤~600 chars; this clamp defends against corrupted or hand-edited index rows
/// that could otherwise let a single record exceed the chunk token cap.
const DESCRIPTION_CLAMP: usize = 2_000;
/// Maximum text_content chars in a serialized index line.
const TEXT_CLAMP: usize = 120;
const FIND_PROMPT_HEAD: &str = "Photo library index, one image per line as 'id| kind | filename | description | tags | text':\n\n";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindData {
    pub query: String,
    pub hits: Vec<FindHit>,
    pub searched: usize,
    pub mode: String,
    pub chunks: usize,
    pub warnings: Vec<String>,
    #[serde(rename = "kindFilter", skip_serializing_if = "Vec::is_empty")]
    pub kind_filter: Vec<String>,
    /// Count of out-of-range ids dropped from the model's response. Serialized
    /// as `invalidIdsDropped` and skipped when zero (F7).
    #[serde(skip_serializing_if = "is_zero")]
    pub invalid_ids_dropped: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gallery_path: Option<String>,
}

fn is_zero(v: &usize) -> bool {
    *v == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindHit {
    pub path: String,
    pub rel_path: String,
    pub filename: String,
    pub description: String,
    pub tags: Vec<String>,
    pub kind: String,
    pub rank: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindPlan {
    pub mode: String,
    pub chunks: usize,
    pub estimated_tokens: usize,
    pub projected_cost_dollars: f64,
    pub searched: usize,
}

pub trait FindChat: Sync {
    fn find_chat(&self, messages: &[Message], opts: ChatOpts) -> Result<ChatResponse, LensError>;
    fn spend_snapshot(&self) -> Spend;
}

impl FindChat for CerebrasClient {
    fn find_chat(&self, messages: &[Message], opts: ChatOpts) -> Result<ChatResponse, LensError> {
        self.chat(messages, opts)
    }

    fn spend_snapshot(&self) -> Spend {
        self.spend()
            .lock()
            .map(|spend| spend.clone())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct FindOptions {
    pub concurrency: usize,
}

impl Default for FindOptions {
    fn default() -> Self {
        Self { concurrency: 25 }
    }
}

#[derive(Clone, Copy)]
pub struct FindContext<'a> {
    pub chat: &'a (dyn FindChat + Sync),
    pub budget: &'a Budget,
    pub spend: &'a SharedSpend,
}

/// Tracks the number of invalid (out-of-range) ids dropped across all
/// `call_for_ids` invocations for a single find run. This is accumulated
/// alongside `warnings` so that `FindData` can carry the structured
/// `invalidIdsDropped` field (F7) while `warnings` no longer carries the
/// free-form "invalidIdsDropped: N" string.
struct FindStats {
    invalid_ids_dropped: usize,
}

pub fn find(
    query: &str,
    records: &[IndexRecord],
    top: usize,
    chat: &(dyn FindChat + Sync),
    budget: &Budget,
    spend: &SharedSpend,
) -> Result<FindData, LensError> {
    find_with_options(
        query,
        Path::new(""),
        records,
        top,
        FindContext {
            chat,
            budget,
            spend,
        },
        &FindOptions::default(),
    )
}

pub fn find_with_options(
    query: &str,
    library_path: &Path,
    records: &[IndexRecord],
    top: usize,
    ctx: FindContext<'_>,
    options: &FindOptions,
) -> Result<FindData, LensError> {
    let snapshot = make_snapshot(records);
    if snapshot.is_empty() {
        return Ok(FindData {
            query: query.to_string(),
            hits: Vec::new(),
            searched: 0,
            mode: "single_shot".to_string(),
            chunks: 0,
            warnings: vec!["index is empty; run lens index".to_string()],
            kind_filter: Vec::new(),
            invalid_ids_dropped: 0,
            gallery_path: None,
        });
    }

    let plan = plan(query, records, top);
    let warnings = Vec::new();
    let mut stats = FindStats {
        invalid_ids_dropped: 0,
    };
    let selected: Vec<IndexRecord> = if plan.mode == "single_shot" {
        call_for_ids(query, &snapshot, top, ctx, &mut stats)?
    } else {
        chunked_ids(
            query,
            &snapshot,
            top,
            ctx,
            options.concurrency.max(1),
            &mut stats,
        )?
    };

    Ok(data_from_records(
        query,
        library_path,
        &snapshot,
        selected,
        plan.mode,
        plan.chunks,
        warnings,
        stats.invalid_ids_dropped,
    ))
}

pub fn plan(query: &str, records: &[IndexRecord], top: usize) -> FindPlan {
    let snapshot = make_snapshot(records);
    if snapshot.is_empty() {
        return FindPlan {
            mode: "single_shot".into(),
            chunks: 0,
            estimated_tokens: 0,
            projected_cost_dollars: 0.0,
            searched: 0,
        };
    }
    let full_prompt = prompt(query, &lines_for(&snapshot), top);
    let estimated_tokens = estimate_tokens(full_prompt.len());
    if estimated_tokens <= CHUNK_TOKEN_CAP {
        return FindPlan {
            mode: "single_shot".into(),
            chunks: 1,
            estimated_tokens,
            projected_cost_dollars: projected_call_cost(estimated_tokens),
            searched: snapshot.len(),
        };
    }

    let chunks = chunks_for(query, &snapshot, top);
    let chunk_top = chunk_top(top);
    let mut projected = 0.0;
    for chunk in &chunks {
        projected += projected_call_cost(estimate_tokens(
            prompt(query, &lines_for(chunk), chunk_top).len(),
        ));
    }
    projected += projected_call_cost(estimate_tokens(
        prompt(query, &lines_for(&snapshot), top)
            .len()
            .min(CHUNK_TOKEN_CAP * 3),
    ));

    FindPlan {
        mode: "chunked".into(),
        chunks: chunks.len(),
        estimated_tokens,
        projected_cost_dollars: projected,
        searched: snapshot.len(),
    }
}

/// Chunked find: per-chunk top-N → union → rerank. Returns the selected
/// `IndexRecord`s in rank order (B1: resolves ids against the correct
/// snapshot so no positional cross-snapshot confusion is possible).
fn chunked_ids(
    query: &str,
    snapshot: &[SnapshotRecord],
    top: usize,
    ctx: FindContext<'_>,
    concurrency: usize,
    stats: &mut FindStats,
) -> Result<Vec<IndexRecord>, LensError> {
    let chunks = chunks_for(query, snapshot, top);
    let chunk_top = chunk_top(top);
    let queue = Arc::new(Mutex::new(VecDeque::from(
        chunks.into_iter().enumerate().collect::<Vec<_>>(),
    )));
    let (tx, rx) = mpsc::channel();

    // Thread-local stats are accumulated into the parent after the scope
    // completes, because `stats` is not `Sync` (it lives behind a &mut).
    // We pass the per-worker invalid-id counts through the channel alongside
    // the results.
    let collected: Vec<IndexRecord> = thread::scope(|scope| {
        for _ in 0..concurrency {
            let queue = Arc::clone(&queue);
            let tx = tx.clone();
            scope.spawn(move || {
                loop {
                    let Some((chunk_idx, chunk)) = queue
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .pop_front()
                    else {
                        return;
                    };
                    let mut local_stats = FindStats {
                        invalid_ids_dropped: 0,
                    };
                    // B1: call_for_ids resolves ids against the chunk snapshot
                    // it was called with, returning IndexRecords directly.
                    let result = call_for_ids(query, &chunk, chunk_top, ctx, &mut local_stats);
                    let _ = tx.send((chunk_idx, result, local_stats));
                }
            });
        }
        drop(tx);

        let mut collected = Vec::new();
        for (_, result, local_stats) in rx {
            stats.invalid_ids_dropped += local_stats.invalid_ids_dropped;
            collected.extend(result?);
        }
        Ok::<Vec<IndexRecord>, LensError>(collected)
    })?;

    // Dedup by rel_path (first wins, preserving rank order from chunks).
    let mut seen = HashSet::new();
    let mut union = Vec::new();
    for record in collected {
        if seen.insert(record.rel_path.clone()) {
            union.push(record);
        }
    }
    let union_snapshot = make_snapshot(&union);
    // B1: the rerank call resolves its ids against the UNION snapshot it was
    // called with, returning IndexRecords directly — no positional confusion
    // with the full snapshot.
    call_for_ids(query, &union_snapshot, top, ctx, stats)
}

/// Makes a single find model call (with re-roll policies) and resolves the
/// returned ids against the *same* snapshot the call was made against.
/// Returns the selected `IndexRecord`s in rank order (B1).
///
/// Re-roll policies:
/// - F6: all-invalid-id reroll — if every returned id is out of range, reserve
///   budget for one more call and retry. Refusal → same budget-partial error.
/// - F9: parse-failure reroll — if the JSON fails to parse on the first call,
///   reserve budget for one more call and retry. Second failure → upstream
///   error.
fn call_for_ids(
    query: &str,
    snapshot: &[SnapshotRecord],
    top: usize,
    ctx: FindContext<'_>,
    stats: &mut FindStats,
) -> Result<Vec<IndexRecord>, LensError> {
    let prompt = prompt(query, &lines_for(snapshot), top);
    let projected = projected_call_cost(estimate_tokens(prompt.len()));

    // Initial call.
    let reservation = ctx
        .budget
        .reserve(&ctx.chat.spend_snapshot(), projected)
        .ok_or_else(|| LensError::partial("budget refused find call"))?;
    let response = ctx.chat.find_chat(
        &[Message::user(prompt.clone())],
        ChatOpts {
            max_completion_tokens: Some(FIND_MAX_COMPLETION_TOKENS),
            response_format: Some(ids_response_format()),
            ..ChatOpts::default()
        },
    );
    reservation.settle(0.0);
    let response = response?;

    // F9: parse failure gets one re-roll (budget-reserved like F6).
    let parsed = match parse_ids(&response.content) {
        Ok(payload) => payload,
        Err(parse_err) => {
            // Reserve budget for the re-roll.
            let reroll_reservation = ctx
                .budget
                .reserve(&ctx.chat.spend_snapshot(), projected)
                .ok_or_else(|| LensError::partial("budget refused find call"))?;
            let reroll = ctx.chat.find_chat(
                &[Message::user(prompt.clone())],
                ChatOpts {
                    max_completion_tokens: Some(FIND_MAX_COMPLETION_TOKENS),
                    response_format: Some(ids_response_format()),
                    ..ChatOpts::default()
                },
            );
            reroll_reservation.settle(0.0);
            let reroll = reroll?;
            parse_ids(&reroll.content).map_err(|_| parse_err)?
        }
    };

    let filtered = filter_ids(&parsed.ids, snapshot.len(), stats);
    if filtered.is_empty() && !parsed.ids.is_empty() {
        // F6: all-invalid-id reroll — reserve budget before the retry.
        let reroll_reservation = ctx
            .budget
            .reserve(&ctx.chat.spend_snapshot(), projected)
            .ok_or_else(|| LensError::partial("budget refused find call"))?;
        let reroll = ctx.chat.find_chat(
            &[Message::user(prompt)],
            ChatOpts {
                max_completion_tokens: Some(FIND_MAX_COMPLETION_TOKENS),
                response_format: Some(ids_response_format()),
                ..ChatOpts::default()
            },
        );
        reroll_reservation.settle(0.0);
        let reroll = reroll?;
        let parsed = parse_ids(&reroll.content)?;
        let filtered = filter_ids(&parsed.ids, snapshot.len(), stats);
        if filtered.is_empty() && !parsed.ids.is_empty() {
            return Err(
                LensError::upstream("model returned only out-of-range find ids")
                    .with_provider(Provider::Cerebras),
            );
        }
        // Resolve filtered ids to records against the same snapshot.
        return Ok(resolve_ids(&filtered, snapshot));
    }
    Ok(resolve_ids(&filtered, snapshot))
}

/// Maps validated position ids to `IndexRecord`s in rank order.
fn resolve_ids(ids: &[usize], snapshot: &[SnapshotRecord]) -> Vec<IndexRecord> {
    ids.iter()
        .filter_map(|&id| snapshot.get(id).map(|item| item.record.clone()))
        .collect()
}

#[derive(Debug, Deserialize)]
struct IdsPayload {
    ids: Vec<i64>,
}

fn parse_ids(content: &str) -> Result<IdsPayload, LensError> {
    let repaired = json_repair(content);
    serde_json::from_str(&repaired).map_err(|err| {
        LensError::upstream(format!("failed to parse find response JSON: {err}"))
            .with_provider(Provider::Cerebras)
    })
}

/// Filters raw i64 ids to valid usize positions, counting dropped ids into
/// `stats` (F7: no longer pushes a warning string).
fn filter_ids(ids: &[i64], len: usize, stats: &mut FindStats) -> Vec<usize> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for id in ids {
        if *id >= 0 && (*id as usize) < len {
            let id = *id as usize;
            if seen.insert(id) {
                out.push(id);
            }
        } else {
            stats.invalid_ids_dropped += 1;
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn data_from_records(
    query: &str,
    library_path: &Path,
    snapshot: &[SnapshotRecord],
    selected: Vec<IndexRecord>,
    mode: String,
    chunks: usize,
    warnings: Vec<String>,
    invalid_ids_dropped: usize,
) -> FindData {
    let hits = selected
        .into_iter()
        .enumerate()
        .map(|(rank, record)| {
            let path = library_path.join(&record.rel_path);
            FindHit {
                path: path.to_string_lossy().to_string(),
                rel_path: record.rel_path.clone(),
                filename: record.filename.clone(),
                description: record.description.clone(),
                tags: record.tags.clone(),
                kind: record.kind.clone(),
                rank: rank + 1,
            }
        })
        .collect();
    // `searched` is always the FULL snapshot length (B1).
    FindData {
        query: query.to_string(),
        hits,
        searched: snapshot.len(),
        mode,
        chunks,
        warnings,
        kind_filter: Vec::new(),
        invalid_ids_dropped,
        gallery_path: None,
    }
}

#[derive(Debug, Clone)]
struct SnapshotRecord {
    record: IndexRecord,
}

fn make_snapshot(records: &[IndexRecord]) -> Vec<SnapshotRecord> {
    let mut by_path: HashMap<&str, &IndexRecord> = HashMap::new();
    for record in records {
        by_path.insert(record.rel_path.as_str(), record);
    }
    let mut out = by_path
        .into_values()
        .cloned()
        .map(|record| SnapshotRecord { record })
        .collect::<Vec<_>>();
    out.sort_by(|a, b| a.record.rel_path.cmp(&b.record.rel_path));
    out
}

fn lines_for(records: &[SnapshotRecord]) -> Vec<String> {
    records
        .iter()
        .enumerate()
        .map(|(id, item)| line_for(id, &item.record))
        .collect()
}

fn line_for(id: usize, record: &IndexRecord) -> String {
    // F5: clamp description to 2,000 chars (defense against corrupted/hand-edited
    // index rows; normal captions are ≤~600 chars). Keep the existing 120-char
    // text clamp.
    let description: String = record.description.chars().take(DESCRIPTION_CLAMP).collect();
    let mut line = format!(
        "{id}| {} | {} | {} | tags: {}",
        record.kind,
        record.filename,
        description,
        record.tags.join(",")
    );
    if !record.text_content.is_empty() {
        line.push_str(" | text: ");
        line.push_str(
            &record
                .text_content
                .chars()
                .take(TEXT_CLAMP)
                .collect::<String>(),
        );
    }
    line
}

fn prompt(query: &str, lines: &[String], top: usize) -> String {
    format!(
        "{FIND_PROMPT_HEAD}{}\n\nQuery: {query}\n\nReturn the ids of the {top} best-matching images, best first.",
        lines.join("\n")
    )
}

fn estimate_tokens(bytes: usize) -> usize {
    bytes.div_ceil(3)
}

fn projected_call_cost(prompt_tokens: usize) -> f64 {
    prompt_tokens as f64 / 1_000_000.0 * GEMMA_INPUT_PER_MTOK
        + FIND_MAX_COMPLETION_TOKENS as f64 / 1_000_000.0 * GEMMA_OUTPUT_PER_MTOK
}

fn chunk_top(top: usize) -> usize {
    (top * 3).max(20)
}

fn chunks_for(query: &str, snapshot: &[SnapshotRecord], top: usize) -> Vec<Vec<SnapshotRecord>> {
    // F4: size with chunk_top(top), the same `top` value the per-chunk workers
    // actually send. The old code used `top` (the final rerank top) which
    // mis-estimated chunk sizes.
    let ct = chunk_top(top);
    let mut chunks: Vec<Vec<SnapshotRecord>> = Vec::new();
    let mut current = Vec::new();
    for record in snapshot {
        let mut trial = current.clone();
        trial.push(record.clone());
        let tokens = estimate_tokens(prompt(query, &lines_for(&trial), ct).len());
        if tokens > CHUNK_TOKEN_CAP && !current.is_empty() {
            chunks.push(current);
            current = vec![record.clone()];
        } else {
            current = trial;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    // Invariant (F5): with the DESCRIPTION_CLAMP (2,000 chars) and TEXT_CLAMP
    // (120 chars) on line_for, a single record's serialized line cannot
    // exceed CHUNK_TOKEN_CAP. This guarantees the `else` branch above always
    // makes progress — a single record always fits in one chunk.
    debug_assert!(
        chunks.iter().all(|chunk| !chunk.is_empty()),
        "chunks must be non-empty"
    );
    chunks
}

fn ids_response_format() -> Value {
    json!({
        "type": "json_schema",
        "json_schema": {
            "name": "hits",
            "strict": true,
            "schema": {
                "type": "object",
                "properties": {"ids": {"type": "array", "items": {"type": "integer"}}},
                "required": ["ids"],
                "additionalProperties": false
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn rec(path: &str, description: &str) -> IndexRecord {
        IndexRecord {
            rel_path: path.into(),
            size: 1,
            mtime_ns: 1,
            description: description.into(),
            filename: path.replace('.', "-"),
            tags: vec!["tag".into()],
            text_content: "abcdefghijklmnopqrstuvwxyz".repeat(6),
            kind: "photo".into(),
        }
    }

    struct ScriptedFindChat {
        responses: Mutex<VecDeque<String>>,
        calls: AtomicUsize,
    }

    impl ScriptedFindChat {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl FindChat for ScriptedFindChat {
        fn find_chat(
            &self,
            _messages: &[Message],
            _opts: ChatOpts,
        ) -> Result<ChatResponse, LensError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let content = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| json!({"ids":[0]}).to_string());
            Ok(ChatResponse {
                content,
                tool_calls: Vec::new(),
                usage: Default::default(),
                wall_time_ms: 1,
            })
        }

        fn spend_snapshot(&self) -> Spend {
            Spend::default()
        }
    }

    #[test]
    fn snapshot_is_last_wins_sorted_and_lines_are_stable() {
        let records = vec![rec("b.jpg", "old"), rec("a.jpg", "a"), rec("b.jpg", "new")];
        let snapshot = make_snapshot(&records);
        assert_eq!(snapshot[0].record.rel_path, "a.jpg");
        assert_eq!(snapshot[1].record.description, "new");
        assert_eq!(
            line_for(0, &snapshot[0].record),
            "0| photo | a-jpg | a | tags: tag | text: abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnop"
        );
    }

    #[test]
    fn dry_plan_reports_single_shot() {
        let plan = plan("beach", &[rec("a.jpg", "a")], 8);
        assert_eq!(plan.mode, "single_shot");
        assert_eq!(plan.chunks, 1);
        assert!(plan.projected_cost_dollars > 0.0);
    }

    #[test]
    fn filter_ids_counts_dropped_into_stats() {
        // F7: invalid ids are counted in stats, not in warnings.
        let mut stats = FindStats {
            invalid_ids_dropped: 0,
        };
        let ids = filter_ids(&[99, 0, -1, 0], 1, &mut stats);
        assert_eq!(ids, vec![0]);
        assert_eq!(stats.invalid_ids_dropped, 2);
    }

    #[test]
    fn large_indexes_use_chunked_rerank_path() {
        // With the F5 description clamp (2,000 chars), each line is ~2,170
        // bytes → ~723 tokens. Need >97 records to exceed 70K tokens.
        let records = (0..150)
            .map(|i| rec(&format!("{i:03}.jpg"), &"large description ".repeat(300)))
            .collect::<Vec<_>>();
        let plan = plan("large", &records, 3);
        assert_eq!(plan.mode, "chunked");
        assert!(plan.chunks > 1);

        let chat = ScriptedFindChat::new(vec![json!({"ids":[0]}).to_string(); plan.chunks + 1]);
        let spend = crate::providers::new_spend();
        let data = find_with_options(
            "large",
            Path::new("/library"),
            &records,
            3,
            FindContext {
                chat: &chat,
                budget: &Budget::new(None, None),
                spend: &spend,
            },
            &FindOptions { concurrency: 4 },
        )
        .unwrap();

        assert_eq!(data.mode, "chunked");
        assert_eq!(data.chunks, plan.chunks);
        assert!(chat.calls.load(Ordering::SeqCst) > plan.chunks);
    }

    // B1 regression test: 3 chunks where per-chunk responses pick DISTINCT
    // non-zero ids, and the rerank returns non-zero ids. The returned hit
    // relPaths must be the exact union records at those union positions,
    // computed independently in the test. The old code (all calls return [0])
    // could not catch this because id 0 maps to the same record in both
    // snapshots.
    #[test]
    fn chunked_find_resolves_rerank_ids_against_union_snapshot() {
        // Build records large enough to force 3+ chunks.
        // With the F5 description clamp (2,000 chars), each line is ~2,170
        // bytes → ~723 estimated tokens. Need >277 records for 3 chunks
        // (each ≤100K estimated tokens). Use 450 for at least 3 chunks.
        let records: Vec<IndexRecord> = (0..450)
            .map(|i| {
                rec(
                    &format!("file_{i:03}.jpg"),
                    &"large description ".repeat(300),
                )
            })
            .collect();
        let plan = plan("large", &records, 3);
        assert_eq!(plan.mode, "chunked");
        let num_chunks = plan.chunks;
        assert!(num_chunks >= 3, "need at least 3 chunks, got {num_chunks}");

        // Build the snapshot and chunks exactly as find_with_options does, to
        // compute the expected union records independently.
        let snapshot = make_snapshot(&records);
        let chunks = chunks_for("large", &snapshot, 3);

        // Script per-chunk responses: each chunk returns a DISTINCT non-zero id.
        // chunk 0 → id 2, chunk 1 → id 1, chunk 2 → id 3 (if chunk has 4+ records).
        // Remaining chunks → id 0.
        let mut scripted_responses: Vec<String> = Vec::new();
        let per_chunk_ids: Vec<usize> = (0..num_chunks)
            .map(|i| match i {
                0 => 2,
                1 => 1,
                2 => 3,
                _ => 0,
            })
            .collect();
        for &id in &per_chunk_ids {
            scripted_responses.push(json!({"ids":[id]}).to_string());
        }

        // Compute the union: collect the records selected from each chunk.
        let mut union_records: Vec<IndexRecord> = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let id = per_chunk_ids[i];
            if id < chunk.len() {
                union_records.push(chunk[id].record.clone());
            }
        }
        // Dedup by rel_path (first wins).
        let mut seen = HashSet::new();
        let mut union_deduped: Vec<IndexRecord> = Vec::new();
        for record in union_records {
            if seen.insert(record.rel_path.clone()) {
                union_deduped.push(record);
            }
        }
        let union_snapshot = make_snapshot(&union_deduped);

        // The rerank call returns ids [2, 0] — non-zero id 2 is key.
        // This maps to union_snapshot[2] and union_snapshot[0].
        let rerank_ids = vec![2, 0];
        scripted_responses.push(json!({"ids": rerank_ids}).to_string());

        // Compute expected hit relPaths independently.
        let mut expected_rel_paths: Vec<String> = Vec::new();
        for &id in &rerank_ids {
            if id < union_snapshot.len() {
                expected_rel_paths.push(union_snapshot[id].record.rel_path.clone());
            }
        }

        let chat = ScriptedFindChat::new(scripted_responses);
        let spend = crate::providers::new_spend();
        let data = find_with_options(
            "large",
            Path::new("/library"),
            &records,
            3,
            FindContext {
                chat: &chat,
                budget: &Budget::new(None, None),
                spend: &spend,
            },
            &FindOptions { concurrency: 1 }, // deterministic ordering
        )
        .unwrap();

        assert_eq!(data.mode, "chunked");
        // The hits must match the expected relPaths computed from the union.
        let actual_rel_paths: Vec<String> = data.hits.iter().map(|h| h.rel_path.clone()).collect();
        assert_eq!(
            actual_rel_paths, expected_rel_paths,
            "B1 regression: chunked find resolved rerank ids against the wrong snapshot"
        );
    }

    // F9: parse failure on the first call gets one re-roll, then succeeds.
    #[test]
    fn parse_failure_rerolls_once_then_succeeds() {
        let records = vec![rec("a.jpg", "desc"), rec("b.jpg", "desc2")];
        // First call returns bad JSON, second call returns valid ids.
        let chat = ScriptedFindChat::new(vec![
            "not valid json".to_string(),
            json!({"ids": [0]}).to_string(),
        ]);
        let spend = crate::providers::new_spend();
        let data = find_with_options(
            "query",
            Path::new("/lib"),
            &records,
            3,
            FindContext {
                chat: &chat,
                budget: &Budget::new(None, None),
                spend: &spend,
            },
            &FindOptions::default(),
        )
        .unwrap();

        assert_eq!(data.mode, "single_shot");
        assert_eq!(data.hits.len(), 1);
        assert_eq!(data.hits[0].rel_path, "a.jpg");
        assert_eq!(chat.calls.load(Ordering::SeqCst), 2);
    }

    // F9: parse failure on both calls → upstream error.
    #[test]
    fn double_parse_failure_yields_upstream_error() {
        let records = vec![rec("a.jpg", "desc"), rec("b.jpg", "desc2")];
        let chat = ScriptedFindChat::new(vec![
            "not valid json".to_string(),
            "still not json".to_string(),
        ]);
        let spend = crate::providers::new_spend();
        let result = find_with_options(
            "query",
            Path::new("/lib"),
            &records,
            3,
            FindContext {
                chat: &chat,
                budget: &Budget::new(None, None),
                spend: &spend,
            },
            &FindOptions::default(),
        );

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().exit_code(), 5);
    }

    // F6: all-invalid ids → reroll → still all invalid → upstream error.
    #[test]
    fn all_invalid_ids_reroll_then_error() {
        let records = vec![rec("a.jpg", "desc")];
        // Both calls return id 999 (out of range for a 1-record snapshot).
        let chat = ScriptedFindChat::new(vec![
            json!({"ids": [999]}).to_string(),
            json!({"ids": [999]}).to_string(),
        ]);
        let spend = crate::providers::new_spend();
        let result = find_with_options(
            "query",
            Path::new("/lib"),
            &records,
            3,
            FindContext {
                chat: &chat,
                budget: &Budget::new(None, None),
                spend: &spend,
            },
            &FindOptions::default(),
        );

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().exit_code(), 5);
    }

    // F5: description is clamped to 2,000 chars.
    #[test]
    fn description_is_clamped_to_2000_chars() {
        let long_desc = "x".repeat(5_000);
        let record = rec("a.jpg", &long_desc);
        let line = line_for(0, &record);
        // The description portion should be clamped, not the full 5,000 chars.
        // The line format is "0| photo | a-jpg | <desc> | tags: tag | text: ..."
        // We just verify the line doesn't contain the full 5,000-char description.
        assert!(!line.contains(&"x".repeat(5_000)));
        // And it does contain at least 2,000 x's (the clamp limit).
        let x_count = line.matches('x').count();
        // 2000 from description + 156 from text_content (6*26=156) = 2156
        assert!(
            x_count >= 2_000,
            "description should have at least 2000 chars, got {x_count}"
        );
        assert!(
            x_count <= 2_200,
            "description should be clamped, got {x_count} chars"
        );
    }

    // F7: invalidIdsDropped is a structured field, not a warning string.
    #[test]
    fn invalid_ids_dropped_is_structured_field_not_warning() {
        let records = vec![rec("a.jpg", "desc"), rec("b.jpg", "desc2")];
        // Return [99, 0] — 99 is out of range, 0 is valid.
        let chat = ScriptedFindChat::new(vec![json!({"ids": [99, 0]}).to_string()]);
        let spend = crate::providers::new_spend();
        let data = find_with_options(
            "query",
            Path::new("/lib"),
            &records,
            3,
            FindContext {
                chat: &chat,
                budget: &Budget::new(None, None),
                spend: &spend,
            },
            &FindOptions::default(),
        )
        .unwrap();

        assert_eq!(data.invalid_ids_dropped, 1);
        // The warnings should NOT contain the old "invalidIdsDropped: 1" string.
        assert!(
            !data
                .warnings
                .iter()
                .any(|w| w.contains("invalidIdsDropped"))
        );
    }

    // F10: settle_spend_seen is gone — verify no lock-and-drop no-op.
    // This is implicitly tested by the fact that the code compiles without it.
}
