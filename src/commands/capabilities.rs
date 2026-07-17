use std::collections::BTreeMap;

use serde::Serialize;

use crate::cli::GlobalArgs;
use crate::commands::common::{CommandSuccess, zero_envelope};
use crate::config::{DEFAULT_API_BASE, DEFAULT_MAX_CONCURRENCY, DEFAULT_MODEL};
use crate::error::LensError;
use crate::indexer::CAPTION_WORST_CASE_COST;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CapabilitiesData {
    name: &'static str,
    version: &'static str,
    schema: &'static str,
    commands: Vec<CommandCapability>,
    global_flags: Vec<GlobalFlag>,
    exit_codes: BTreeMap<&'static str, &'static str>,
    env_vars: Vec<EnvVar>,
    skip_reasons: [&'static str; 4],
    cost_expectations: CostExpectations,
    index_storage: &'static str,
    schemas: SchemaCommands,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandCapability {
    name: &'static str,
    usage: &'static str,
    read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    read_only_note: Option<&'static str>,
    destructive: bool,
    spends_money: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    spends_money_note: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    writes_index: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr_on_error: Option<&'static str>,
}

impl CommandCapability {
    fn free_read_only(name: &'static str, usage: &'static str) -> Self {
        Self {
            name,
            usage,
            read_only: true,
            read_only_note: None,
            destructive: false,
            spends_money: false,
            spends_money_note: None,
            writes_index: None,
            stdout: None,
            stderr_on_error: None,
        }
    }
}

#[derive(Serialize)]
struct GlobalFlag {
    name: &'static str,
    #[serde(rename = "type")]
    value_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    minimum: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    maximum: Option<u64>,
    default: Option<CapabilityDefault>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum CapabilityDefault {
    Bool(bool),
    Number(u64),
    String(&'static str),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EnvVar {
    name: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    required_for: Option<&'static [&'static str]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<CapabilityDefault>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CostExpectations {
    caption_measured_average_dollars: f64,
    caption_worst_case_dollars: f64,
    prototype_run: &'static str,
    find: &'static str,
}

#[derive(Serialize)]
struct SchemaCommands {
    response: &'static str,
    error: &'static str,
    all: &'static str,
}

pub fn run(_global: &GlobalArgs) -> Result<CommandSuccess, LensError> {
    let data = CapabilitiesData {
        name: "lens",
        version: env!("CARGO_PKG_VERSION"),
        schema: "lens.cli.capabilities.v1",
        commands: commands(),
        global_flags: global_flags(),
        exit_codes: [
            ("0", "ok"),
            ("1", "usage"),
            (
                "2",
                "auth; doctor emits its structured report on stdout even at exit 2",
            ),
            ("3", "config"),
            ("4", "network"),
            ("5", "upstream"),
            ("6", "rate-limit"),
            (
                "10",
                "partial/refused; stdout carries ok:true success envelope with budget.hit set",
            ),
            ("11", "no-input"),
        ]
        .into_iter()
        .collect(),
        env_vars: env_vars(),
        skip_reasons: [
            "unsupported_format",
            "corrupt_image",
            "too_large",
            "budget_refused",
        ],
        cost_expectations: CostExpectations {
            caption_measured_average_dollars: 0.00166,
            caption_worst_case_dollars: CAPTION_WORST_CASE_COST,
            prototype_run: "1100 images ≈ 93s ≈ $2.29 (measured 2026-07-02)",
            find: "pennies; projected from serialized index tokens",
        },
        index_storage: "${XDG_DATA_HOME:-~/.local/share}/lens/libraries/<sha256(canonical_path)[..16]>/",
        schemas: SchemaCommands {
            response: "lens schema response",
            error: "lens schema error",
            all: "lens schema all",
        },
    };
    Ok(CommandSuccess {
        envelope: zero_envelope("capabilities", data),
        exit_code: 0,
        hint: None,
    })
}

fn commands() -> Vec<CommandCapability> {
    vec![
        CommandCapability {
            name: "index",
            usage: "lens index [DIR]",
            read_only: false,
            read_only_note: None,
            destructive: false,
            spends_money: true,
            spends_money_note: None,
            writes_index: Some(true),
            stdout: Some("lens.cli.response.v1"),
            stderr_on_error: Some("lens.cli.error.v1"),
        },
        CommandCapability {
            name: "find",
            usage: "lens find <QUERY> [--dir DIR] [--top N] [--kind KIND] [--gallery PATH]",
            read_only: true,
            read_only_note: Some(
                "read-only on the library; --gallery writes the requested HTML file",
            ),
            destructive: false,
            spends_money: true,
            spends_money_note: None,
            writes_index: None,
            stdout: None,
            stderr_on_error: None,
        },
        CommandCapability::free_read_only("status", "lens status [--dir DIR]"),
        CommandCapability {
            name: "doctor",
            usage: "lens doctor [--online]",
            read_only: true,
            read_only_note: None,
            destructive: false,
            spends_money: true,
            spends_money_note: Some("only with --online"),
            writes_index: None,
            stdout: None,
            stderr_on_error: None,
        },
        CommandCapability::free_read_only("capabilities", "lens capabilities"),
        CommandCapability::free_read_only("schema", "lens schema [response|error|all]"),
    ]
}

fn global_flags() -> Vec<GlobalFlag> {
    vec![
        GlobalFlag {
            name: "--json",
            value_type: "bool",
            minimum: None,
            maximum: None,
            default: Some(CapabilityDefault::Bool(false)),
        },
        GlobalFlag {
            name: "--model",
            value_type: "string",
            minimum: None,
            maximum: None,
            default: Some(CapabilityDefault::String(DEFAULT_MODEL)),
        },
        GlobalFlag {
            name: "--max-dollars",
            value_type: "number",
            minimum: Some(0),
            maximum: None,
            default: None,
        },
        GlobalFlag {
            name: "--max-seconds",
            value_type: "integer",
            minimum: Some(1),
            maximum: None,
            default: None,
        },
        GlobalFlag {
            name: "--index-path",
            value_type: "path",
            minimum: None,
            maximum: None,
            default: Some(CapabilityDefault::String(
                "XDG data dir keyed by library path",
            )),
        },
        GlobalFlag {
            name: "--dry-run",
            value_type: "bool",
            minimum: None,
            maximum: None,
            default: Some(CapabilityDefault::Bool(false)),
        },
        GlobalFlag {
            name: "--concurrency",
            value_type: "integer",
            minimum: Some(1),
            maximum: Some(50),
            default: Some(CapabilityDefault::Number(25)),
        },
    ]
}

fn env_vars() -> Vec<EnvVar> {
    vec![
        EnvVar {
            name: "CEREBRAS_API_KEY",
            required_for: Some(&["index", "find", "doctor --online"]),
            secret: Some(true),
            default: None,
        },
        EnvVar {
            name: "LENS_API_BASE",
            required_for: None,
            secret: None,
            default: Some(CapabilityDefault::String(DEFAULT_API_BASE)),
        },
        EnvVar {
            name: "LENS_MODEL",
            required_for: None,
            secret: None,
            default: Some(CapabilityDefault::String(DEFAULT_MODEL)),
        },
        EnvVar {
            name: "LENS_MAX_CONCURRENCY",
            required_for: None,
            secret: None,
            default: Some(CapabilityDefault::Number(DEFAULT_MAX_CONCURRENCY as u64)),
        },
        EnvVar {
            name: "XDG_DATA_HOME",
            required_for: None,
            secret: None,
            default: Some(CapabilityDefault::String("~/.local/share")),
        },
    ]
}
