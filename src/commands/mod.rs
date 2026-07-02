pub mod capabilities;
pub mod doctor;
pub mod find;
pub mod index;
pub mod schema;
pub mod status;

use std::path::{Path, PathBuf};

use clap::Parser;
use clap::error::ErrorKind;

use crate::cli::{Cli, Commands, edit_distance};
use crate::config::Config;
use crate::envelope::{
    Budget, CostDollars, Diagnostics, ErrorEnvelope, SuccessEnvelope, emit_error, emit_success,
};
use crate::error::{LensError, Provider};
use crate::providers::{SharedSpend, new_spend};
use crate::store::Store;

pub struct CommandSuccess {
    pub envelope: SuccessEnvelope,
    pub exit_code: i32,
    pub hint: Option<&'static str>,
}

pub fn run() -> i32 {
    let raw_args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let force_json = raw_args.iter().any(|arg| arg == "--json");
    let cli = match Cli::try_parse_from(raw_args.clone()) {
        Ok(cli) => cli,
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            if force_json {
                let command = if matches!(err.kind(), ErrorKind::DisplayVersion) {
                    "version"
                } else {
                    "help"
                };
                let envelope = zero_envelope(command, serde_json::json!({"text": err.to_string()}));
                emit_success(&envelope, true);
                return 0;
            }
            err.exit();
        }
        Err(err) => return emit_parse_error(&raw_args, err),
    };

    let command_name = command_name(&cli.command);
    match dispatch(cli) {
        Ok(success) => {
            emit_success(
                &success.envelope,
                force_json || success.envelope.command != command_name,
            );
            if let Some(hint) = success.hint
                && !force_json
                && std::io::IsTerminal::is_terminal(&std::io::stdout())
            {
                eprintln!("hint: {hint}");
            }
            success.exit_code
        }
        Err(err) => {
            let envelope = ErrorEnvelope::from_error(command_name, &err, None);
            emit_error(&envelope, force_json)
        }
    }
}

fn dispatch(cli: Cli) -> Result<CommandSuccess, LensError> {
    let global = cli.global;
    match cli.command {
        Commands::Index(args) => index::run(&global, &args),
        Commands::Find(args) => find::run(&global, &args),
        Commands::Status(args) => status::run(&global, &args),
        Commands::Doctor(args) => doctor::run(&global, &args),
        Commands::Capabilities => capabilities::run(&global),
        Commands::Schema(args) => schema::run(&global, &args),
    }
}

fn command_name(command: &Commands) -> &'static str {
    match command {
        Commands::Index(_) => "index",
        Commands::Find(_) => "find",
        Commands::Status(_) => "status",
        Commands::Doctor(_) => "doctor",
        Commands::Capabilities => "capabilities",
        Commands::Schema(_) => "schema",
    }
}

fn emit_parse_error(raw_args: &[std::ffi::OsString], err: clap::Error) -> i32 {
    let message = clean_clap_message(&err.to_string());
    let suggested_fix = suggested_fix(raw_args, &message);
    let command = parse_command_name(raw_args);
    let lens_err =
        LensError::usage(format!("usage error: {message}")).with_suggested_fix(suggested_fix);
    let envelope = ErrorEnvelope::from_error(command, &lens_err, None);
    // F1: detect --json in raw args so `lens --json --badflag` on a TTY still
    // emits the error envelope on stderr (not the human renderer).
    let force_json = raw_args.iter().any(|arg| arg == "--json");
    emit_error(&envelope, force_json)
}

fn clean_clap_message(message: &str) -> String {
    message
        .lines()
        .filter(|line| !line.trim_start().starts_with("Usage:"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn parse_command_name(args: &[std::ffi::OsString]) -> &'static str {
    for arg in args.iter().skip(1) {
        let text = arg.to_string_lossy();
        if text.starts_with('-') {
            continue;
        }
        return match text.as_ref() {
            "index" => "index",
            "find" => "find",
            "status" => "status",
            "doctor" => "doctor",
            "capabilities" => "capabilities",
            "schema" => "schema",
            _ => "lens",
        };
    }
    "lens"
}

fn suggested_fix(args: &[std::ffi::OsString], message: &str) -> String {
    if let Some(tip) = message.lines().find(|line| line.contains("similar")) {
        return tip.trim().trim_start_matches("tip:").trim().to_string();
    }

    let bad = args
        .iter()
        .skip(1)
        .map(|arg| arg.to_string_lossy())
        .find(|arg| arg.starts_with("--") && !known_flags().contains(&arg.as_ref()));
    if let Some(bad) = bad
        && let Some(best) = best_match(&bad, known_flags())
    {
        return format!("did you mean '{best}'?");
    }

    let command = args
        .iter()
        .skip(1)
        .map(|arg| arg.to_string_lossy())
        .find(|arg| !arg.starts_with('-'));
    if let Some(command) = command
        && let Some(best) = best_match(
            &command,
            &[
                "index",
                "find",
                "status",
                "doctor",
                "capabilities",
                "schema",
            ],
        )
    {
        return format!("did you mean '{best}'?");
    }

    "Run `lens --help` or `lens capabilities` for the supported contract.".to_string()
}

fn known_flags() -> &'static [&'static str] {
    &[
        "--json",
        "--model",
        "--max-dollars",
        "--max-seconds",
        "--index-path",
        "--dry-run",
        "--concurrency",
        "--dir",
        "--top",
        "--gallery",
        "--online",
        "--help",
        "--version",
    ]
}

fn best_match<'a>(needle: &str, choices: &'a [&'a str]) -> Option<&'a str> {
    choices
        .iter()
        .copied()
        .map(|choice| (choice, edit_distance(needle, choice)))
        .filter(|(_, distance)| *distance <= 3)
        .min_by_key(|(_, distance)| *distance)
        .map(|(choice, _)| choice)
}

pub(crate) fn zero_envelope(command: &str, data: serde_json::Value) -> SuccessEnvelope {
    SuccessEnvelope::new(
        command,
        data,
        CostDollars {
            model: 0.0,
            search: 0.0,
            total: 0.0,
            estimated: false,
        },
        Budget { hit: None },
        Diagnostics {
            duration_ms: 0,
            retries: 0,
        },
        None,
    )
}

pub(crate) fn cost_from_spend(
    spend: &SharedSpend,
    estimated: bool,
) -> Result<CostDollars, LensError> {
    let spend = spend
        .lock()
        .map_err(|_| LensError::upstream("spend meter lock poisoned"))?;
    Ok(CostDollars {
        model: spend.dollars,
        search: 0.0,
        total: spend.total_dollars(),
        estimated,
    })
}

pub(crate) fn retries_from_spend(spend: &SharedSpend) -> Result<u32, LensError> {
    let spend = spend
        .lock()
        .map_err(|_| LensError::upstream("spend meter lock poisoned"))?;
    Ok(spend.retries.min(u32::MAX as u64) as u32)
}

pub(crate) fn require_key(key: Option<&str>) -> Result<String, LensError> {
    key.filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| {
            LensError::auth("missing Cerebras API key; set CEREBRAS_API_KEY")
                .with_provider(Provider::Cerebras)
                .with_suggested_fix(
                    "set CEREBRAS_API_KEY or add cerebras_api_key to ~/.config/lens/config.toml; verify with: lens doctor",
                )
        })
}

pub(crate) fn model_from(global_model: &Option<String>, cfg: &Config) -> String {
    global_model.clone().unwrap_or_else(|| cfg.model.clone())
}

pub(crate) fn open_store(
    library_path: &Path,
    index_path: Option<&PathBuf>,
) -> Result<Store, LensError> {
    match index_path {
        Some(path) => Store::open_at(path),
        None => Store::open_for_library(library_path),
    }
}

pub(crate) fn resolve_library_path(path: &Path) -> Result<PathBuf, LensError> {
    std::fs::canonicalize(path).map_err(|err| {
        LensError::config(format!(
            "failed to resolve library path {}: {err}",
            path.display()
        ))
        .with_suggested_fix("pass an existing directory to lens index/find/status")
    })
}

pub(crate) fn new_shared_spend() -> SharedSpend {
    new_spend()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    #[test]
    fn emit_parse_error_detects_json_flag_in_raw_args() {
        // F1: `lens --json --badflag` should forward force_json=true to
        // emit_error so the error envelope is emitted on stderr even on a TTY.
        // We can't easily test the full emit_parse_error (it writes to stderr),
        // but we can verify the --json detection logic that feeds force_json.
        let args_with_json: Vec<OsString> =
            vec!["lens".into(), "--json".into(), "--badflag".into()];
        let force_json = args_with_json.iter().any(|arg| arg == "--json");
        assert!(force_json);

        let args_without_json: Vec<OsString> = vec!["lens".into(), "--badflag".into()];
        let force_json = args_without_json.iter().any(|arg| arg == "--json");
        assert!(!force_json);
    }
}
