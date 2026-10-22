# Changelog

All notable changes to `lens` are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [0.1.3] — 2026-10-22

### Security

- Updated the TLS trust-store and handshake crates that `lens` uses for every
  HTTPS call to the captioning provider: `rustls-webpki` 0.103.13 → 0.103.16,
  `webpki-roots` 1.0.8 → 1.0.10 and `rustls` 0.23.41 → 0.23.46. The refreshed
  `webpki-roots` carries the current CA trust list (including the eMudhra CA
  incident changes), and `rustls-webpki` 0.103.16 fixes certificate-validation
  advisories RUSTSEC-2026-0346 through RUSTSEC-2026-0349 and
  RUSTSEC-2026-0381 through RUSTSEC-2026-0383. Upgrade if you run `lens`
  against the live API.

### Changed

- Also ships the hardening and cleanup work merged since 0.1.2: stricter
  config, index-store, image-normalization and provider-client handling, a
  reworked `capabilities` output, and a leaner image decoder feature set
  (`jpeg`, `png`, `webp`, `gif`, `bmp`, `tiff`).

## [0.1.2] — 2026-07-06

### Fixed

- Wired exit code 11 (`no_input`): a blank `find` query now returns a structured
  error instead of a success envelope with zero hits. Found by an automated QA
  sweep.

## [0.1.1] — 2026-07-03

### Added

- Binary releases via `cargo-dist`: prebuilt tarballs for macOS and Linux
  (x86_64 and aarch64), a curl shell installer, and a Homebrew formula
  published to `treygoff24/homebrew-tap` (`brew install treygoff24/tap/lens`).
- Published to crates.io as `lens-cli` (`cargo install lens-cli`); the
  installed binary is still named `lens`.
- `AGENTS.md` — the machine-facing contract for agents setting up or driving
  `lens`: install paths, key setup, envelope schemas, exit-code dictionary,
  and cost-safety rails.
- README front-porch rewrite: cover image, three-way install instructions,
  and a pointer to `AGENTS.md` for agent readers.

## [0.1.0] — 2026-07-02

Initial public release.

### Added

- `lens index [DIR]` — captions every image once via Cerebras vision
  (`gemma-4-31b`), storing results in a durable JSONL index outside the
  library. Resumable; skips unchanged files via a `(relPath, size, mtime)`
  freshness key.
- `lens find <QUERY>` — natural-language search over an indexed library.
  Small indexes go to the model in a single call; larger ones are chunked
  and reranked. Supports `--kind` filtering, `--top N`, and `--gallery` for
  an HTML contact sheet.
- `lens status`, `lens doctor`, `lens capabilities`, `lens schema` —
  offline introspection: staleness, config/key checks, self-description,
  and JSON Schema for both envelope shapes.
- Agent-first contract: one JSON success envelope on stdout, one JSON error
  envelope on stderr, a stable exit-code dictionary, and paste-ready
  `suggestedFix` on every error.
- Cost safety: `--dry-run` (zero-network cost projection) and
  `--max-dollars` / `--max-seconds` hard caps enforced by a budget
  reservation gate.
- macOS HEIC support via `sips` fallback; Linux support with HEIC and
  decode failures reported as skips rather than failures.
