use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Serialize;

use crate::config::Config;
use crate::envelope::{Budget, CostDollars, Diagnostics, SuccessEnvelope};
use crate::error::{LensError, Provider};
use crate::providers::{SharedSpend, new_spend};
use crate::store::Store;

pub struct CommandSuccess {
    pub envelope: SuccessEnvelope,
    pub exit_code: i32,
    pub hint: Option<&'static str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IndexStateData {
    pub library_path: String,
    pub index_path: String,
    pub indexed: usize,
    pub fresh: usize,
    pub stale: usize,
    pub new: usize,
    pub vanished: usize,
    pub stale_all: bool,
    pub projected_wall_seconds: f64,
}

pub(crate) fn zero_envelope(command: &str, data: impl Serialize) -> SuccessEnvelope {
    SuccessEnvelope::new(
        command,
        serde_json::to_value(data).expect("command data serializes"),
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

pub(crate) fn response_value(
    description: &str,
    data: impl Serialize,
) -> Result<serde_json::Value, LensError> {
    serde_json::to_value(data)
        .map_err(|err| LensError::upstream(format!("failed to serialize {description}: {err}")))
}

pub(crate) fn metered_envelope(
    command: &str,
    data: serde_json::Value,
    spend: &SharedSpend,
    budget_hit: Option<&str>,
    started: &Instant,
) -> SuccessEnvelope {
    SuccessEnvelope::new(
        command,
        data,
        cost_from_spend(spend),
        Budget {
            hit: budget_hit.map(str::to_string),
        },
        Diagnostics {
            duration_ms: started.elapsed().as_millis() as u64,
            retries: retries_from_spend(spend),
        },
        None,
    )
}

pub(crate) fn estimated_envelope(
    command: &str,
    description: &str,
    data: impl Serialize,
    projected_cost: f64,
) -> Result<SuccessEnvelope, LensError> {
    Ok(SuccessEnvelope::new(
        command,
        response_value(description, data)?,
        CostDollars {
            model: projected_cost,
            search: 0.0,
            total: projected_cost,
            estimated: true,
        },
        Budget { hit: None },
        Diagnostics {
            duration_ms: 0,
            retries: 0,
        },
        None,
    ))
}

pub(crate) fn zero_cost_success(
    command: &str,
    data: impl Serialize,
    started: &Instant,
    hint: &'static str,
) -> Result<CommandSuccess, LensError> {
    Ok(CommandSuccess {
        envelope: SuccessEnvelope::new(
            command,
            response_value(&format!("{command} data"), data)?,
            CostDollars {
                model: 0.0,
                search: 0.0,
                total: 0.0,
                estimated: false,
            },
            Budget { hit: None },
            Diagnostics {
                duration_ms: started.elapsed().as_millis() as u64,
                retries: 0,
            },
            None,
        ),
        exit_code: 0,
        hint: Some(hint),
    })
}

fn cost_from_spend(spend: &SharedSpend) -> CostDollars {
    let spend = spend.lock().unwrap();
    CostDollars {
        model: spend.dollars,
        search: 0.0,
        total: spend.total_dollars(),
        estimated: false,
    }
}

fn retries_from_spend(spend: &SharedSpend) -> u32 {
    spend.lock().unwrap().retries.min(u32::MAX as u64) as u32
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
