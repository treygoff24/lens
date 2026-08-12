# lens — agent setup and usage guide

You are probably an AI agent setting up `lens` for a human, or about to use it yourself. This file is the complete contract. Everything here is machine-verifiable against the binary: `lens capabilities --json` and `lens schema all --json` are the source of truth if this document and the binary ever disagree.

## What lens does

Natural-language search over a directory of images. `lens index DIR` captions every image once (Cerebras vision model, ~$0.002/image, resumable). `lens find QUERY` answers queries like "the hero shot of the beach club" in seconds by sending the whole caption index to the model in one call. No vector database, no embedding service, no daemon, no server to keep alive.

## Install

Pick the first path that works in your environment:

```sh
# Homebrew (macOS/Linux)
brew install treygoff24/tap/lens

# Shell installer (no toolchain needed; installs a prebuilt binary)
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/treygoff24/lens/releases/latest/download/lens-cli-installer.sh | sh

# Cargo (builds from source; needs Rust 1.85+)
cargo install lens-cli
```

Verify: `lens doctor --json` exits 0 and reports structured checks. It never needs a key and never prints secret values.

## Key setup (do this for your human)

`index` and `find` spend money via the Cerebras API. Your human needs a key from <https://cloud.cerebras.ai> (free tier exists; paid developer tier recommended — free keys have a 65K context window and much lower rate limits).

```sh
export CEREBRAS_API_KEY=...
```

Put it in their shell profile or secret manager — never commit it. `lens doctor --online --json` confirms the key works with a one-token probe (bad key exits 2 but still reports structured checks on stdout).

## The contract

- **stdout**: exactly one JSON success envelope (`lens.cli.response.v1`). Nothing else, ever.
- **stderr**: exactly one JSON error envelope on failure. Every error includes a paste-ready `suggestedFix`.
- **No prompts, no colors, no spinners, no implicit modes.** Safe to drive from any harness.
- **Exit codes are a stable dictionary**: 0 ok · 1 usage · 2 auth · 3 config · 4 network · 5 upstream · 6 rate limit · 10 partial/refused (budget hit — envelope still on stdout) · 11 no input.
- Self-description: `lens capabilities --json` (commands, flags, which commands spend money), `lens schema all --json` (JSON Schema for both envelopes).

## Cost safety — use these, always

Every paid command supports:

- `--dry-run` — zero network, no key needed, closed-form cost projection in the same envelope shape (`data.dryRun: true`, `costDollars.estimated: true`). **Run this first** and show your human the projected cost before spending.
- `--max-dollars N` / `--max-seconds N` — hard caps enforced by a reservation gate; parallel workers cannot overshoot. A budget hit exits 10 with `budget.hit` set. `index` progress is durable (resumable); a `find` refusal returns `data.outcome: "refused"` and empty hits.

Real-world scale: 1,100 images indexed in 93s for $2.29; each `find` on that index costs ~$0.20 and takes 2–6s.

## Typical session

```sh
lens --json index ./photos --dry-run          # project the cost, show the human
lens --json index ./photos --max-dollars 5    # caption + index, capped
lens --json find "sunset over the marina" --dir ./photos --top 5
lens --json find "the team offsite group shot" --dir ./photos --kind photo
lens --json status --dir ./photos             # free, offline: staleness + cost to bring current
```

`--kind photo,screenshot` filters before anything is sent to the model (better precision, fewer tokens). `find --gallery out.html` writes an HTML contact sheet when the human wants to look with their eyes.

## Things that will save you a debugging loop

- The index lives *outside* the library at `${XDG_DATA_HOME:-~/.local/share}/lens/libraries/<hash>/` — libraries can be read-only. `--index-path` overrides.
- `index` takes a per-library advisory lock; a concurrent run exits 3 naming the lock path. Locks older than 30 minutes are stolen with a warning.
- A killed `index` run resumes exactly where it stopped — just rerun it.
- Rate-limit 429s are absorbed by internal retry/backoff; you don't need your own retry loop around `lens`.
- Linux: HEIC files are skipped as `unsupported_format` (conversion needs macOS `sips`). Skips are reported inside the success envelope and never fail a run.
- Windows is unsupported.

## If you're working on lens itself, not just using it

- The gate, all of it, before you claim done: `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test && cargo build --release`.
- Integration tests drive the real binary against a local mock Cerebras server — no key, no network, same as CI. To debug captioning against the live API: `CEREBRAS_API_KEY=... cargo run --release --example debug_caption -- path/to/image.jpg`.
- The contract above is written down in four places that must move together: this file, the README exit-code table, `src/commands/capabilities.rs`, and `LensError::exit_code` in `src/error.rs` (pinned by the `exit_codes_match_contract` test).
- Module map: `cli.rs` clap surface · `walk.rs` deterministic walk and `(relPath, size, mtime)` freshness key · `normalize.rs` in-memory ≤1600px JPEG · `indexer.rs` caption fan-out · `find.rs` single-shot vs chunked plus rerank · `store.rs` index directory, advisory lock, torn-trailing-line tolerance · `budget.rs` reservation gate · `providers/` Cerebras client and retry/backoff · `envelope.rs` and `error.rs` the contract itself.
- `main` carries work past the v0.1.2 tag that is neither released nor pushed. Run `git log --oneline v0.1.2..main` before assuming an installed `lens` behaves like this tree.
- Design doc, including the adversarial review's accepted and rejected findings: `docs/plans/2026-07-01-lens-cli-design.md`. Deliberate non-goals and the v2 seam: `docs/BACKLOG.md`.

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:970c3bf2 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   bd dolt push
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->

<!-- BEGIN BEADS CODEX SETUP: generated by bd setup codex -->
## Beads Issue Tracker

Use Beads (`bd`) for durable task tracking in repositories that include it. Use the `beads` skill at `.agents/skills/beads/SKILL.md` (project install) or `~/.agents/skills/beads/SKILL.md` (global install) for Beads workflow guidance, then use the `bd` CLI for issue operations.

### Quick Reference

```bash
bd ready                # Find available work
bd show <id>            # View issue details
bd update <id> --claim  # Claim work
bd close <id>           # Complete work
bd prime                # Refresh Beads context
```

### Rules

- Use `bd` for all task tracking; do not create markdown TODO lists.
- Run `bd prime` when Beads context is missing or stale. Codex 0.129.0+ can load Beads context automatically through native hooks; use `/hooks` to inspect or toggle them.
- Keep persistent project memory in Beads via `bd remember`; do not create ad hoc memory files.

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.
<!-- END BEADS CODEX SETUP -->
