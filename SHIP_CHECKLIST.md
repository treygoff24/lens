# lens — ship checklist

Goal: launch `lens` as a public, easy-to-understand, easy-to-install open-source project on GitHub.
Scope basis: rust-agent-cli audit (15 pass / 1 review), live acceptance on a 1,100-image corpus (2026-07-02),
and a coordinator ship review. Items are checked off as they land; this file is deleted or archived to
`docs/` once every box is ticked and the repo is public.

Legend: `[x]` done · `[ ]` open · `[~]` open, decision recorded below

---

## 1. Naming & distribution

- [x] **Decision: GitHub-first distribution.** The crates.io name `lens` is taken (an unrelated
  "unified lens query language" crate). Rather than rename, v0.1.0 installs via
  `cargo install --git https://github.com/treygoff24/lens` (or clone + `cargo install --path .`).
  Publishing to crates.io as `lens-cli` (with `[[bin]] name = "lens"`) stays a documented option —
  see §8. Owner can overrule at any time; nothing else depends on this.

## 2. Code wins (pre-ship features & tests)

- [x] `find --kind <KIND>` filter — client-side snapshot filter before serialization. Fixes the
  observed failure mode (asked for a photo, got renders), improves accuracy, and cuts input tokens.
  Repeatable / comma-separable, case-insensitive, echoed in the envelope as `kindFilter`,
  zero-match returns exit 0 + a warning naming the kinds present. Dry-run reflects the filtered estimate.
- [x] Parser test suite (`Cli::try_parse_from`) — the one `review` finding from the
  rust-agent-cli audit script: flag ranges, conflicts, defaults, empty-value rejections.

## 3. Packaging & repo hygiene

- [x] `Cargo.toml` publish metadata: `description`, `license`, `repository`, `readme`,
  `keywords`, `categories`, `rust-version` (edition 2024 ⇒ MSRV 1.85).
- [x] `.gitignore` covers `.delegate/` (delegate-lane run logs) and `.claude/` (machine-local
  skill symlinks).
- [x] Untrack `.claude/skills/*` — they are symlinks into the owner's home directory and break
  for every cloner. (Kept on disk for local delegate lanes; just untracked.)
- [x] `LICENSE` — Apache-2.0, already committed.
- [x] `examples/debug_caption.rs` — **keep.** It is the caption-pipeline diagnosis harness
  (prints raw model output per attempt); document it in the README's development section.
- [x] GitHub Actions CI: fmt --check, clippy `-D warnings`, full test suite, release build,
  on `macos-latest` + `ubuntu-latest`. Integration tests run against a local mock server —
  no secrets required in CI.

## 4. README (the front door)

- [x] Quickstart: install → `CEREBRAS_API_KEY` (link to where to get one) → `index` → `find`,
  with real envelope output shown.
- [x] **Sizing honesty:** replace "single-shot ≈ 1,500 images" with measured guidance —
  caption-rich corpora serialize at ~140 est. tokens/image (measured: 1,100 real-estate photos
  → 154K est. tokens → 2 chunks), terse corpora at ~65. Single-shot ceiling is therefore
  ~700–1,500 images depending on caption richness.
- [x] **Cost-claim verification** (see §5): correct or soften the "pennies when cached" claim
  to match what Cerebras actually documents; state measured per-query cost ($0.20 on the
  1,100-image corpus) rather than only the best case.
- [x] **Linux honesty:** works on Linux, but `sips` is macOS-only, so HEIC files and
  decode-fallback cases are skipped as `unsupported_format` / `corrupt_image` instead of
  converted. Say it before a Linux user discovers it.
- [x] Document `--kind` once it lands.
- [x] Development section: gate commands, mock-server test layout, `debug_caption` example.
- [x] Attribution note: built almost entirely by Claude (owner's request — he's telling the world).

## 5. Cost-claim verification (research, not vibes)

- [x] gemma-4-31b input $2.15/MTok and output $2.70/MTok confirmed against Cerebras's
  published pricing (constants live in `src/find.rs` and the indexer cost model).
- [x] 131K context window confirmed — paid tier only; free tier is 65K (footnoted in README).
- [x] Prompt caching: VERIFIED latency-only — Cerebras bills cached input tokens at the full
  input rate ("no additional fee... billed at the standard input token rate"). "Pennies when
  cached" removed from README; replaced with measured latency win. Consistent with the live
  measurement (2026-07-02): two find queries on the same 2-chunk index cost $0.198 and $0.196 —
  no discount — while latency dropped 5.9s → 1.8s.
- [x] 10MB payload cap confirmed (paid tier; free trial is 4MB/2 images). Rate limit CORRECTED:
  docs say 300 rpm / 500K TPM on the developer tier, not the 500 rpm the design assumed. Measured
  behavior: the 1,100-image run attempted ~710 rpm and completed with 50 retries (4.5%) — the
  existing backoff absorbs the ceiling. README now states 300 rpm; concurrency default unchanged.

## 6. Quality gate (coordinator-run, not delegate-claimed)

- [x] `cargo fmt --check`
- [x] `cargo clippy --all-targets --all-features -- -D warnings`
- [x] `cargo test --all-features` — **five consecutive runs**, zero intermittent failures
- [x] `cargo build --release`
- [x] Live verification: `lens find --kind photo` against the real 1,100-image index returns
  only photos; dry-run token estimate drops vs. unfiltered.

## 7. Launch

- [x] Independent review lane (different model family) over the full uncommitted diff;
  every finding triaged in writing; fixes verified landed.
- [x] Commit(s) with review provenance in the message.
- [x] Create public repo `treygoff24/lens`, push `main`.
- [x] Confirm CI green on the public repo.
- [x] Repo description + topics (`cli`, `rust`, `image-search`, `ai-agents`, `cerebras`).
- [x] Tag `v0.1.0`.

## 8. Post-launch backlog (documented, deliberately not v1)

- Binary releases (cargo-dist) so non-Rust users skip the toolchain.
- crates.io publication as `lens-cli` if `cargo install lens-cli` demand shows up.
- Cached-token cost accounting if/when Cerebras documents a caching discount.
- Local lexical pre-filter stage for 30K+ image libraries (the economic ceiling of
  whole-index-in-prompt; design doc non-goal, real v2 seam).
- `rename` command riding the already-indexed `filename` field.
- Non-Cerebras providers; Windows support (no `sips` equivalent).
- README demo GIF / asciinema of `index` → `find`.
