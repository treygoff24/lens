# lens — agent-first image library CLI

**Status:** design, reviewed (Codex adversarial review 2026-07-01, 13 findings; amendments in the final section). **Provenance:** graduation of `volley/librarian.py` (validated 2026-07-01: 1,128 images indexed in ~45s for $1.87; search 8/8 relevant in ~1s via whole-index prompt with cached prefix). Sibling of `recon` — same envelope contract, same exit-code dictionary, same foundation code, same consumer: coding agents.

## Thesis

An agent working on a real project needs to *find media*: "the hero shot of the beach club," "the signed page of the lease," "every screenshot of the billing dashboard." Filenames don't carry that. lens indexes a directory of images once (fast vision captioning fan-out on Cerebras gemma-4-31b), then answers natural-language queries in ~1 second by shoving the whole index into a prompt-cached prefix. No vector DB, no embedding service, no daemon. Index is a JSONL file; search is one model call.

## Contract (inherited from recon, deliberately)

- Envelopes `lens.cli.response.v1` / `lens.cli.error.v1`, camelCase, stdout=data / stderr=errors, TTY detection with `--json` force.
- Exit codes: 0 ok / 1 usage / 2 auth / 3 config / 4 network / 5 upstream / 6 rate-limit / 10 partial / 11 no-input. Same deliberate deviation from the rust-agent-cli skill's sysexits table — same consumer population as recon/exa-agent.
- Errors carry `suggestedFix` with paste-ready remediation. Human TTY mode ends with a next-step hint on stderr.
- `capabilities` / `schema` self-description; `spendsMoney: true` on index/find; blast-radius annotations.
- Budget: `--max-dollars` / `--max-seconds`, pre-launch projection via `may_launch` serialized through a gate mutex; budget-hit partial = SUCCESS envelope + exit 10 (index is resumable, so a partial index is durable progress, not loss).
- ONE shared Spend meter across all clients and run params.

## Commands

### `lens index <DIR>`
Recursive walk → normalize → caption → append to index. Resumable and incremental.

- **Walk:** regular files only, skip hidden dirs/files, skip non-image extensions (allow: jpg/jpeg/png/webp/gif/bmp/tiff/heic). Follow no symlinks. Deterministic (sorted) order.
- **Freshness key:** `(relative_path, size, mtime_ms)`. Prototype keyed on path only — lens re-captions when a file changes and drops index rows whose file vanished (`--prune`, default on; prunes at index time only).
- **Normalize:** target ≤1600px longest side, JPEG q80, ≤3MB encoded. Pure-Rust via `image` crate for jpg/png/webp/gif/bmp/tiff. HEIC and decode failures: shell out to `sips` (macOS) when available; otherwise record the file as `skipped: unsupported_format` in the run report — never a hard failure. Normalized bytes are transient (encode in memory, send, drop) — no imgcache dir; the index stores metadata only.
- **Caption call:** 1 image/request (Cerebras: base64 data URI only, ≤10MB payload), structured schema `{description, filename, tags[], text_content, kind}` (proven schema from librarian.py verbatim), `strict: true`, max 1200 completion tokens. json_repair + one re-roll on parse failure (model-output boundary only — the recon lesson). `invalid_image_data` from upstream → one forced re-encode retry (extension-lies-about-format case, measured in prototype).
- **Fan-out:** scoped threads, concurrency 25 (client cap; server allows 500 rpm). Budget gate per caption unit before launch. Results append to index JSONL as they land (crash-safe: a killed run resumes where it stopped).
- **Report (envelope data):** indexed/skipped/failed/pruned counts, per-failure reasons, wall time, spend. Failures don't fail the run; exit 0 with failures listed, exit 10 only on budget hit.

### `lens find <QUERY> [--dir DIR] [--top N]`
- Load index for the library. Build the same line format as the prototype (`id| kind | filename | description | tags | text`).
- **Single-shot path** (index ≤ ~100K estimated tokens ≈ ~1,500 images): one call, index lines as a stable prefix (byte-identical across queries → Cerebras prompt cache hit → ~1s answers), query appended, structured `{ids: [int]}` response.
- **Chunked path** (bigger libraries): split into ~90K-token chunks → parallel top-N per chunk → one rerank call over the union. Two model rounds, still seconds.
- Hits returned best-first: `{path (absolute), filename, description, tags, kind, rank}`. Out-of-range ids from the model are dropped silently (measured behavior: rare hallucinated ids).
- `--gallery [PATH]`: also write an HTML contact sheet (the prototype's dark-mode grid) for human eyes; path echoed in envelope data. Never the default in JSON mode.

### `lens status [--dir DIR]`
The agent's discovery affordance, zero network: library path, index path, indexed count, unindexed count (fresh walk vs index), stale count (mtime/size drift), vanished count, projected cost + wall estimate to bring current (`indexable × CAPTION_WORST_CASE_COST`). Exit 0 always (status of an unindexed dir is a valid answer, not an error).

### `lens doctor [--online]` / `lens capabilities` / `lens schema [response|error|all]`
Recon-shaped. Doctor offline: config parse, key presence (never values), sips availability + platform note, index dir writability. `--online`: 1-token chat probe. Bad key → exit 2 naming CEREBRAS_API_KEY.

### Global flags
`--json`, `--model`, `--max-dollars`, `--max-seconds`, `--dry-run` (index: walk + count + closed-form cost projection, zero network; find: token estimate + path chosen [single-shot vs chunked] + projected cost).

## Index storage

Libraries may be read-only (our own test corpus is). Index lives under the XDG data dir, keyed by the canonicalized library path:

```
~/.local/share/lens/libraries/<sha256(canonical_path)[..16]>/
  meta.json      # {libraryPath, model, schemaVersion, createdAt, updatedAt}
  index.jsonl    # one record per image: {relPath, size, mtimeMs, description, filename, tags, textContent, kind}
```

`--index-path` overrides for portable/CI use. `--dir` defaults to cwd. `meta.json.schemaVersion` gates future format migrations (v1 refuses newer versions with exit 3 + suggestedFix "reindex").

## Costs (measured basis, worst-case constants)

- `CAPTION_WORST_CASE_COST = $0.004`/image (measured avg $0.00166; 2.4× headroom for text-heavy documents).
- Find single-shot: `est_tokens × $2.15/MTok` + 200 output tokens — projected per-call, not a constant (index size varies).
- 759-image corpus full index ≈ **$1.30 projected**; find ≈ $0.05 uncached, pennies cached.
- Budget context: ~$36 of the session's $50 remains; a full acceptance run costs < $2.

## Dependencies

clap, serde/serde_json, thiserror, ureq (rustls), toml, base64, sha2, **image** (the one new dep — no stdlib decode; HEIC excluded from its feature set, handled via sips fallback). Dev: assert_cmd, predicates, tempfile. No async runtime, std::thread scoped fan-out — recon's run_chunked pattern.

## Testing

- Unit: walker (hidden/ext/symlink rules), freshness key, normalize decision matrix (pass-through vs re-encode vs sips vs skip), index round-trip, chunking arithmetic, budget gates, json_repair (ported tests).
- Integration (mock server, recon's tests/common pattern): fixture images *generated in the test* (tiny PNGs via `image`), canned caption + find responses; assert envelope shape, exact spend arithmetic, resume-skips-indexed, stale-recaption, exit 10 partial, dry-run zero-network, unknown flag / missing key / missing arg exits.
- Live acceptance (the corpus): full index of `volley/corpus/photos` (759 images) — wall time, spend vs projection, failure count; 3 known-answer queries (hero-image query from Session 1 replays at 8/8 relevance); `status` after partial index; budget kill-test mid-index then resume to completion.

## Non-goals (v1)

Video files, PDF pages, RAW formats; watch/daemon mode; near-duplicate detection; `rename` command (the `filename` field already in the index is the v2 seam); non-Cerebras providers; Windows sips-equivalent.

## Design-review amendments (Codex, 2026-07-01 — accepted findings)

1. **Single-writer index (was the blocker).** `lens index` takes a per-library advisory lock: `index.lock` created with `create_new`, containing PID + timestamp; a lock older than 30 minutes is stale and stolen with a warning. Second concurrent writer → exit 3 with suggestedFix naming the lock path. Appends are one complete line per write syscall; prune rewrites via temp file + atomic rename; index load tolerates (and truncates) one torn trailing line.
2. **Deterministic search snapshot.** `find` never trusts file order: it loads records, dedupes by relPath (last wins), sorts by NFC-normalized relPath, and assigns line IDs at query time. The serialized prefix is therefore byte-identical for an unchanged index regardless of append/resume/prune history. Any index mutation makes the next search cold — documented, accepted.
3. **Budget reservation ledger.** `may_launch` alone allows N-workers × unit overshoot. Budget gains reserve/settle: inside the gate mutex, `reserve(projected)` counts toward the cap; on completion the reservation converts to actual spend (or releases on failure). Overshoot bound drops to actual-vs-projected error only. `CAPTION_WORST_CASE_COST = $0.008` (1,200 completion tokens ≈ $0.0032 + image + prompt tokens + one re-roll headroom; measured avg $0.00166).
4. **Freshness key**: `(relPath NFC-normalized, size, mtime_ns)` — nanoseconds, not ms. Content hashing rejected: reads the whole library per status check. Case-only renames fall out naturally as new+vanished.
5. **Moved libraries**: meta.json stores the display path; if it no longer resolves to the same canonical path, exit 3 with suggestedFix (`--index-path` or reindex). Volume IDs rejected as overkill.
6. **Chunked recall**: per-chunk take = `max(top × 3, 20)`, dedupe by relPath before rerank. Adaptive second pass rejected (v2).
7. **Token estimate**: from exact serialized prompt bytes at a conservative 3 bytes/token, chunk cap 70K tokens including query + schema overhead; `find --dry-run` reports the chosen path and chunk count.
8. **EXIF orientation**: read and `apply_orientation()` before resize for JPEG/WebP/TIFF; EXIF-rotated fixture in tests.
9. **Normalization matrix pinned**: animated GIF/WebP → first frame; decode failure on a supported extension → sips fallback → `corrupt_image` skip reason (distinct from `unsupported_format`); decode pixel limits enforced.
10. **Version-aware staleness**: meta.json carries `model`, `promptVersion`, `normalizerVersion`; mismatch marks the whole index stale (status reports it; index recaptions with a warning). Per-row versions rejected.
11. **suggestedFix in human mode**: the ported human-mode error renderer must print suggestedFix (recon's doesn't) — golden test required.
12. **Invalid find IDs**: dropped IDs surface in `warnings.invalidIdsDropped`; if ALL returned IDs are invalid → one re-roll, then upstream error.
13. **Gallery kept** (explicit `--gallery PATH` only, never default, all model-derived text HTML-escaped). Cutting it rejected — it's the human-verification affordance.

Rejected: content-hash freshness (F4-part), volume/file IDs (F5-part), adaptive second search pass (F6-part), gallery removal (F13-part).

**Wave-2 review waivers (Cursor round, 2026-07-01):** NFC normalization of relPaths waived — indexes are XDG-keyed by canonical library path and never travel between machines, so walk/store self-consistency is sufficient; revisit if `--index-path` portability becomes a real workflow. Normalize ceiling clarified: re-encode path allows raw JPEG ≤6.5MB (×1.33 base64 = 8.6MB, inside Cerebras's 10MB payload cap); the 3MB figure applies to the pass-through gate only. Lock staleness is mtime-based (30min) without PID liveness — a >30min hung indexer can be stolen from; documented limit, not defended (PID probes need libc).

## Waves

1. **Foundation port** — scaffold + error/envelope/config/budget/providers(cerebras + image content part, no exa) adapted from recon; golden envelope tests. (Codex)
2. **Indexer** — walk/normalize/freshness/caption fan-out/store/resume/prune. (Codex → Cursor review → GLM fix)
3. **Find + surface** — find (both paths), status, CLI, doctor/capabilities/schema, integration tests, README. (Codex → Cursor review → GLM fix → live acceptance)
