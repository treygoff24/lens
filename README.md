# lens

`lens` is an agent-first Rust CLI for image-library search. It indexes images with Cerebras vision captions, stores a JSONL index under the user data dir, then answers natural-language image queries from that index. Stdout is reserved for success envelopes. Stderr is reserved for error envelopes. There are no prompts, colors, spinners, or implicit query mode.

## Contract

Commands are explicit:

```sh
lens index [DIR]
lens find <QUERY> [--dir DIR] [--top N] [--gallery PATH]
lens status [--dir DIR]
lens doctor [--online]
lens capabilities
lens schema [response|error|all]
```

Global flags are `--json`, `--model`, `--max-dollars`, `--max-seconds`, `--index-path`, `--dry-run`, and `--concurrency N`. `--concurrency` is capped at 50 and is used by indexing and chunked find.

## Install

```sh
cargo install --path .
```

## Environment

| Variable | Required for | Default |
| --- | --- | --- |
| `CEREBRAS_API_KEY` | `index`, `find`, `doctor --online` | none |
| `LENS_API_BASE` | Cerebras-compatible API base | `https://api.cerebras.ai/v1` |
| `LENS_MODEL` | model default | `gemma-4-31b` |
| `LENS_MAX_CONCURRENCY` | chunk/search worker default | `25` |
| `XDG_DATA_HOME` | index storage root | `~/.local/share` |

## Canonical use

```sh
lens --json index ./photos
lens --json find "beach club hero shot" --dir ./photos --top 3
```

Example index envelope:

```json
{
  "schema": "lens.cli.response.v1",
  "ok": true,
  "command": "index",
  "requestId": "00000000-0000-0000-0000-000000000000",
  "data": {
    "libraryPath": "/tmp/photos",
    "indexed": 2,
    "skipped": [],
    "failed": [],
    "pruned": 0,
    "budget": { "hit": null },
    "durationMs": 120,
    "totalFiles": 2,
    "fresh": 0,
    "stale": 0,
    "new": 2,
    "vanished": 0,
    "outcome": "complete"
  },
  "costDollars": { "model": 0.0097, "search": 0.0, "total": 0.0097, "estimated": false },
  "budget": { "hit": null },
  "diagnostics": { "durationMs": 120, "retries": 0 }
}
```

> **Note:** `IndexReport` duplicates `budget` and `durationMs` inside `data` (from the report struct) alongside the envelope-level `budget` and `diagnostics.durationMs`. Both are kept for backward compatibility; the envelope-level fields are the canonical ones.

Example find envelope:

```json
{
  "schema": "lens.cli.response.v1",
  "ok": true,
  "command": "find",
  "requestId": "00000000-0000-0000-0000-000000000000",
  "data": {
    "query": "beach club hero shot",
    "hits": [
      {
        "path": "/tmp/photos/1.png",
        "relPath": "1.png",
        "filename": "mock-image",
        "description": "mock image caption",
        "tags": ["mock", "fixture"],
        "kind": "photo",
        "rank": 1
      }
    ],
    "searched": 2,
    "mode": "single_shot",
    "chunks": 1,
    "warnings": [],
    "invalidIdsDropped": 0,
    "outcome": "answered"
  },
  "costDollars": { "model": 0.00485, "search": 0.0, "total": 0.00485, "estimated": false },
  "budget": { "hit": null },
  "diagnostics": { "durationMs": 25, "retries": 0 }
}
```

`--dry-run` returns the same envelope family with `data.dryRun: true` and `costDollars.estimated: true`. It makes no provider requests and does not require `CEREBRAS_API_KEY`.

## Exit codes

| Code | Meaning | Channel and shape |
| ---: | --- | --- |
| 0 | ok | success envelope on stdout |
| 1 | usage | error envelope on stderr, stdout empty |
| 2 | auth | error envelope on stderr, stdout empty, except `doctor --online` reports structured checks on stdout |
| 3 | config | error envelope on stderr, stdout empty |
| 4 | network | error envelope on stderr, stdout empty |
| 5 | upstream | error envelope on stderr, stdout empty |
| 6 | rate limit | error envelope on stderr, stdout empty |
| 10 | partial or refused | success envelope on stdout with `ok: true` and `budget.hit` set |
| 11 | no input | error envelope on stderr, stdout empty |

Exit 10 is deliberate. `index` is resumable, so a budget-hit partial index is durable progress. `find` has no useful partial result, so a budget refusal returns `data.outcome: "refused"`, empty `hits`, `budget.hit`, and exit 10.

## Skip reasons

| Reason | Meaning |
| --- | --- |
| `unsupported_format` | format is not handled by the Rust decoder or `sips` fallback |
| `corrupt_image` | decode or fallback conversion failed |
| `too_large` | image exceeds decode or payload limits |
| `budget_refused` | budget gate refused to launch another model call |

Skips and per-file failures are reported in the success envelope. They do not make `index` fail.

## Index storage and locking

Default storage is:

```text
${XDG_DATA_HOME:-~/.local/share}/lens/libraries/<sha256(canonical_path)[..16]>/
  meta.json
  index.jsonl
  index.lock
```

`--index-path PATH` overrides the store directory. Libraries can be read-only; the index is outside the library by default. `lens index` takes a per-library advisory `index.lock` with `create_new`. A lock older than 30 minutes is treated as stale and may be stolen. Fresh lock conflicts exit 3 with a suggested fix naming the lock path.

## Cost expectations

Prototype measurements: 1,128 images indexed in about 45 seconds for about $1.87. Search was about 1 second with a cached whole-index prompt prefix. The indexer uses a worst-case projection of `$0.008` per captioned image. Find cost is projected from serialized index bytes at the Gemma input price plus 200 output tokens, so larger indexes cost more.

## Doctor and self-description

```sh
lens doctor --json
lens doctor --online --json
lens capabilities --json
lens schema all --json
lens schema response --json
lens schema error --json
```

Offline doctor checks config parsing, key presence, `sips` availability, and index data-dir writability. It never prints secret values. `--online` adds a one-token Cerebras chat probe. Bad or missing online credentials exit 2, but doctor still emits its report on stdout.

`capabilities` returns version, commands, read-only/destructive/spend annotations, global flags, exit codes, env vars, skip reasons, and cost expectations. `schema` returns JSON Schema for success and error envelopes.
