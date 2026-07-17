use std::time::Instant;

use serde::Serialize;
use serde_json::to_value;

use crate::cli::{GlobalArgs, StatusArgs};
use crate::commands::{CommandSuccess, model_from, open_store, resolve_library_path};
use crate::config::Config;
use crate::envelope::{Budget, CostDollars, Diagnostics, SuccessEnvelope};
use crate::error::LensError;
use crate::indexer::CAPTION_WORST_CASE_COST;
use crate::walk::{partition_freshness, walk_library};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusData {
    library_path: String,
    index_path: String,
    indexed: usize,
    fresh: usize,
    stale: usize,
    new: usize,
    vanished: usize,
    stale_all: bool,
    projected_cost_dollars: f64,
    projected_wall_seconds: f64,
    warnings: Vec<String>,
}

pub fn run(global: &GlobalArgs, args: &StatusArgs) -> Result<CommandSuccess, LensError> {
    let started = Instant::now();
    let library_path = resolve_library_path(&args.dir)?;
    let cfg = Config::load()?;
    let model = model_from(&global.model, &cfg);
    let store = open_store(&library_path, global.index_path.as_ref())?;
    let walked = walk_library(&library_path)?;
    let loaded = store.load(&model)?;
    let mut freshness = partition_freshness(&walked, &loaded.records);
    if loaded.stale_all {
        freshness.mark_all_stale();
    }
    let indexable = freshness.stale.len() + freshness.new.len();
    let projected = indexable as f64 * CAPTION_WORST_CASE_COST;
    let data = to_value(StatusData {
        library_path: library_path.to_string_lossy().into_owned(),
        index_path: store.dir().to_string_lossy().into_owned(),
        indexed: loaded.records.len(),
        fresh: freshness.fresh.len(),
        stale: freshness.stale.len(),
        new: freshness.new.len(),
        vanished: freshness.vanished.len(),
        stale_all: loaded.stale_all,
        projected_cost_dollars: projected,
        projected_wall_seconds: indexable as f64 / f64::from(global.concurrency) * 1.5,
        warnings: loaded.warnings,
    })
    .map_err(|err| LensError::upstream(format!("failed to serialize status data: {err}")))?;
    let envelope = SuccessEnvelope::new(
        "status",
        data,
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
    );
    Ok(CommandSuccess {
        envelope,
        exit_code: 0,
        hint: Some("run `lens index --dry-run --json` to see planned captioning work"),
    })
}
