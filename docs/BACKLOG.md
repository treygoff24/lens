# lens — post-launch backlog

Shipped and public through v0.1.3 (2026-10-22). What follows are the deliberate
non-goals from the v1 design and the ship review — recorded so they are not
re-litigated, not scheduled as work.

- **Local lexical pre-filter** ahead of the whole-index-in-prompt call. This is
  the real v2 seam: query cost grows linearly with index size, so libraries
  around 30K images are the economic ceiling of the current design.
- **`rename`** command riding the already-indexed `filename` field.
- **Cached-token cost accounting**, only if Cerebras ever documents a caching
  discount. Verified 2026-07-02: cached input bills at the full input rate, so
  prompt caching is a latency win (measured 5.9s → 1.8s) and never a price one.
- **Non-Cerebras providers** and **Windows support** (no `sips` equivalent).
- **README demo GIF or asciinema** of `index` → `find`.

Everything else from the pre-launch checklist landed: the `--kind` filter, the
parser test suite, CI on macOS and Linux, cargo-dist binaries with a Homebrew tap
and crates.io publication as `lens-cli` (both v0.1.1), and the cost and
rate-limit claims verified against Cerebras's published docs — the corrected
numbers live in the README's cost section.
