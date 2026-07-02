use serde_json::json;

use crate::cli::GlobalArgs;
use crate::commands::{CommandSuccess, zero_envelope};
use crate::config::{DEFAULT_API_BASE, DEFAULT_MAX_CONCURRENCY, DEFAULT_MODEL};
use crate::error::LensError;
use crate::indexer::CAPTION_WORST_CASE_COST;

pub fn run(_global: &GlobalArgs) -> Result<CommandSuccess, LensError> {
    let data = json!({
        "name": "lens",
        "version": env!("CARGO_PKG_VERSION"),
        "schema": "lens.cli.capabilities.v1",
        "commands": [
            {"name": "index", "usage": "lens index [DIR]", "readOnly": false, "destructive": false, "spendsMoney": true, "writesIndex": true, "stdout": "lens.cli.response.v1", "stderrOnError": "lens.cli.error.v1"},
            {"name": "find", "usage": "lens find <QUERY> [--dir DIR] [--top N] [--gallery PATH]", "readOnly": true, "readOnlyNote": "read-only on the library; --gallery writes the requested HTML file", "destructive": false, "spendsMoney": true},
            {"name": "status", "usage": "lens status [--dir DIR]", "readOnly": true, "destructive": false, "spendsMoney": false},
            {"name": "doctor", "usage": "lens doctor [--online]", "readOnly": true, "destructive": false, "spendsMoney": true, "spendsMoneyNote": "only with --online"},
            {"name": "capabilities", "usage": "lens capabilities", "readOnly": true, "destructive": false, "spendsMoney": false},
            {"name": "schema", "usage": "lens schema [response|error|all]", "readOnly": true, "destructive": false, "spendsMoney": false}
        ],
        "globalFlags": [
            {"name": "--json", "type": "bool", "default": false},
            {"name": "--model", "type": "string", "default": DEFAULT_MODEL},
            {"name": "--max-dollars", "type": "number", "minimum": 0, "default": null},
            {"name": "--max-seconds", "type": "integer", "minimum": 1, "default": null},
            {"name": "--index-path", "type": "path", "default": "XDG data dir keyed by library path"},
            {"name": "--dry-run", "type": "bool", "default": false},
            {"name": "--concurrency", "type": "integer", "minimum": 1, "maximum": 50, "default": 25}
        ],
        "exitCodes": {
            "0": "ok",
            "1": "usage",
            "2": "auth; doctor emits its structured report on stdout even at exit 2",
            "3": "config",
            "4": "network",
            "5": "upstream",
            "6": "rate-limit",
            "10": "partial/refused; stdout carries ok:true success envelope with budget.hit set",
            "11": "no-input"
        },
        "envVars": [
            {"name": "CEREBRAS_API_KEY", "requiredFor": ["index", "find", "doctor --online"], "secret": true},
            {"name": "LENS_API_BASE", "default": DEFAULT_API_BASE},
            {"name": "LENS_MODEL", "default": DEFAULT_MODEL},
            {"name": "LENS_MAX_CONCURRENCY", "default": DEFAULT_MAX_CONCURRENCY},
            {"name": "XDG_DATA_HOME", "default": "~/.local/share"}
        ],
        "skipReasons": ["unsupported_format", "corrupt_image", "too_large", "budget_refused"],
        "costExpectations": {
            "captionMeasuredAverageDollars": 0.0017,
            "captionWorstCaseDollars": CAPTION_WORST_CASE_COST,
            "prototypeRun": "1128 images ≈ 45s ≈ $1.87",
            "find": "pennies; projected from serialized index tokens"
        },
        "indexStorage": "${XDG_DATA_HOME:-~/.local/share}/lens/libraries/<sha256(canonical_path)[..16]>/",
        "schemas": {"response": "lens schema response", "error": "lens schema error", "all": "lens schema all"}
    });
    Ok(CommandSuccess {
        envelope: zero_envelope("capabilities", data),
        exit_code: 0,
        hint: None,
    })
}
