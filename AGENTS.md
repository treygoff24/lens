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
