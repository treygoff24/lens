use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde_json::to_value;

use crate::cli::{DoctorArgs, GlobalArgs};
use crate::commands::{
    CommandSuccess, cost_from_spend, model_from, new_shared_spend, require_key, retries_from_spend,
};
use crate::config::Config;
use crate::envelope::{Budget, Diagnostics, SuccessEnvelope};
use crate::error::{LensError, Provider};
use crate::providers::cerebras::{CerebrasClient, ChatOpts, Message};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorReport {
    schema_version: &'static str,
    status: &'static str,
    summary: DoctorSummary,
    checks: Vec<DoctorCheck>,
    run_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct DoctorSummary {
    total: usize,
    ok: usize,
    warn: usize,
    error: usize,
    fixable: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorCheck {
    id: &'static str,
    category: &'static str,
    severity: &'static str,
    ok: bool,
    detail: String,
    location: Option<String>,
    fix_available: bool,
    remediation: Option<Remediation>,
}

#[derive(Debug, Serialize)]
struct Remediation {
    summary: String,
    command: String,
    reversible: bool,
}

pub fn run(global: &GlobalArgs, args: &DoctorArgs) -> Result<CommandSuccess, LensError> {
    let started = Instant::now();
    let cfg = Config::load()?;
    let spend = new_shared_spend();
    let mut checks = offline_checks(&cfg);

    if args.online {
        match require_key(cfg.cerebras_api_key.as_deref()) {
            Ok(key) => {
                let model = model_from(&global.model, &cfg);
                let chat = CerebrasClient::new(key, cfg.api_base.clone(), model)
                    .with_spend(Arc::clone(&spend));
                checks.push(probe_cerebras(&chat));
            }
            Err(err) => checks.push(auth_error_check("online.cerebras", err)),
        }
    }

    let exit_code = if args.online
        && checks
            .iter()
            .any(|check| !check.ok && check.category == "auth")
    {
        2
    } else {
        0
    };
    let report = report(checks);
    let envelope = SuccessEnvelope::new(
        "doctor",
        to_value(report).map_err(|err| {
            LensError::upstream(format!("failed to serialize doctor report: {err}"))
        })?,
        cost_from_spend(&spend, false)?,
        Budget { hit: None },
        Diagnostics {
            duration_ms: started.elapsed().as_millis() as u64,
            retries: retries_from_spend(&spend)?,
        },
        None,
    );
    Ok(CommandSuccess {
        envelope,
        exit_code,
        hint: Some("run `lens doctor --online --json` to probe the Cerebras key"),
    })
}

fn offline_checks(cfg: &Config) -> Vec<DoctorCheck> {
    vec![
        ok_check(
            "config.parse",
            "config",
            "config parsed; defaults and environment overrides resolved",
        ),
        key_presence_check(cfg.cerebras_api_key.as_deref()),
        sips_check(),
        data_dir_check(),
        ok_check(
            "config.resolved",
            "config",
            &format!(
                "model={}, apiBase={}, maxConcurrency={}",
                cfg.model, cfg.api_base, cfg.max_concurrency
            ),
        ),
    ]
}

fn key_presence_check(key: Option<&str>) -> DoctorCheck {
    if key.is_some_and(|value| !value.trim().is_empty()) {
        ok_check(
            "auth.cerebras_key",
            "auth",
            "Cerebras key present via CEREBRAS_API_KEY or config",
        )
    } else {
        DoctorCheck {
            id: "auth.cerebras_key",
            category: "auth",
            severity: "warn",
            ok: false,
            detail: "missing Cerebras API key; set CEREBRAS_API_KEY before lens index/find".into(),
            location: Some("CEREBRAS_API_KEY".into()),
            fix_available: false,
            remediation: Some(Remediation {
                summary: "set CEREBRAS_API_KEY".into(),
                command: "export CEREBRAS_API_KEY=...".into(),
                reversible: false,
            }),
        }
    }
}

fn sips_check() -> DoctorCheck {
    let ok = Command::new("sips")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();
    if ok {
        ok_check(
            "deps.sips",
            "deps",
            "sips is available for HEIC/decode fallback on macOS",
        )
    } else {
        DoctorCheck {
            id: "deps.sips",
            category: "deps",
            severity: "warn",
            ok: false,
            detail:
                "sips is unavailable; HEIC and some corrupt-extension fallbacks will be skipped"
                    .into(),
            location: None,
            fix_available: false,
            remediation: Some(Remediation {
                summary: "run on macOS with sips for HEIC fallback".into(),
                command: "xcrun sips --help".into(),
                reversible: false,
            }),
        }
    }
}

fn data_dir_check() -> DoctorCheck {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/share"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let dir = base.join("lens");
    match std::fs::create_dir_all(&dir).and_then(|_| {
        let probe = dir.join(".doctor-write-test");
        std::fs::write(&probe, b"ok")?;
        std::fs::remove_file(probe)
    }) {
        Ok(()) => ok_check(
            "state.data_dir",
            "state",
            &format!("index data dir is writable: {}", dir.display()),
        ),
        Err(err) => DoctorCheck {
            id: "state.data_dir",
            category: "state",
            severity: "error",
            ok: false,
            detail: format!("index data dir is not writable: {}: {err}", dir.display()),
            location: Some(dir.to_string_lossy().to_string()),
            fix_available: false,
            remediation: Some(Remediation {
                summary: "choose a writable XDG_DATA_HOME".into(),
                command: "export XDG_DATA_HOME=/path/to/writable/data".into(),
                reversible: false,
            }),
        },
    }
}

fn probe_cerebras(client: &CerebrasClient) -> DoctorCheck {
    match client.chat(
        &[Message::user("Reply with ok.")],
        ChatOpts {
            max_completion_tokens: Some(1),
            ..ChatOpts::default()
        },
    ) {
        Ok(_) => ok_check(
            "online.cerebras",
            "auth",
            "Cerebras chat-completions probe succeeded",
        ),
        Err(err) => auth_error_check("online.cerebras", err),
    }
}

fn auth_error_check(id: &'static str, err: LensError) -> DoctorCheck {
    DoctorCheck {
        id,
        category: "auth",
        severity: "error",
        ok: false,
        detail: format!("Cerebras probe failed; check CEREBRAS_API_KEY: {err}"),
        location: Some("CEREBRAS_API_KEY".to_string()),
        fix_available: false,
        remediation: Some(Remediation {
            summary: "set a valid Cerebras API key".into(),
            command: "export CEREBRAS_API_KEY=...".into(),
            reversible: false,
        }),
    }
}

fn ok_check(id: &'static str, category: &'static str, detail: &str) -> DoctorCheck {
    DoctorCheck {
        id,
        category,
        severity: "info",
        ok: true,
        detail: detail.to_string(),
        location: None,
        fix_available: false,
        remediation: None,
    }
}

fn report(checks: Vec<DoctorCheck>) -> DoctorReport {
    let summary = DoctorSummary {
        total: checks.len(),
        ok: checks.iter().filter(|check| check.ok).count(),
        warn: checks
            .iter()
            .filter(|check| !check.ok && check.severity == "warn")
            .count(),
        error: checks
            .iter()
            .filter(|check| !check.ok && check.severity == "error")
            .count(),
        fixable: checks.iter().filter(|check| check.fix_available).count(),
    };
    let status = if summary.error > 0 {
        "broken"
    } else if summary.warn > 0 {
        "degraded"
    } else {
        "healthy"
    };
    DoctorReport {
        schema_version: "1.0",
        status,
        summary,
        checks,
        run_id: None,
    }
}

#[allow(dead_code)]
fn _provider_marker() -> Provider {
    Provider::Cerebras
}
