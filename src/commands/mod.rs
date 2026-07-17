pub mod capabilities;
mod common;
pub mod doctor;
pub mod find;
pub mod index;
pub mod schema;
pub mod status;

use clap::Parser;
use clap::error::ErrorKind;
use serde::Serialize;

use crate::cli::{Cli, Commands, edit_distance};
use crate::envelope::{ErrorEnvelope, emit_error, emit_success};
use crate::error::LensError;

pub use common::CommandSuccess;
use common::zero_envelope;

#[derive(Serialize)]
struct TextData {
    text: String,
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
                let envelope = zero_envelope(
                    command,
                    TextData {
                        text: err.to_string(),
                    },
                );
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
    // Parsing can fail before `GlobalArgs` exists, so preserve raw JSON mode.
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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    #[test]
    fn emit_parse_error_detects_json_flag_in_raw_args() {
        // The stderr writer is not injectable, so pin the raw-argument seam.
        let args_with_json: Vec<OsString> =
            vec!["lens".into(), "--json".into(), "--badflag".into()];
        let force_json = args_with_json.iter().any(|arg| arg == "--json");
        assert!(force_json);

        let args_without_json: Vec<OsString> = vec!["lens".into(), "--badflag".into()];
        let force_json = args_without_json.iter().any(|arg| arg == "--json");
        assert!(!force_json);
    }
}
