use std::time::Instant;

use serde_json::json;

use crate::cli::{GlobalArgs, StatusArgs};
use crate::commands::{CommandSuccess, model_from, open_store, resolve_library_path};
use crate::config::Config;
use crate::envelope::{Budget, CostDollars, Diagnostics, SuccessEnvelope};
use crate::error::LensError;
use crate::indexer::CAPTION_WORST_CASE_COST;
use crate::walk::{partition_freshness, walk_library};

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
        let mut recaption = std::mem::take(&mut freshness.fresh);
        recaption.append(&mut freshness.new);
        freshness.stale.extend(recaption);
        freshness.new = Vec::new();
        freshness.fresh = Vec::new();
    }
    let indexable = freshness.stale.len() + freshness.new.len();
    let projected = indexable as f64 * CAPTION_WORST_CASE_COST;
    let data = json!({
        "libraryPath": library_path.to_string_lossy(),
        "indexPath": store.dir().to_string_lossy(),
        "indexed": loaded.records.len(),
        "fresh": freshness.fresh.len(),
        "stale": freshness.stale.len(),
        "new": freshness.new.len(),
        "vanished": freshness.vanished.len(),
        "staleAll": loaded.stale_all,
        "projectedCostDollars": projected,
        "projectedWallSeconds": indexable as f64 / f64::from(global.concurrency) * 1.5,
        "warnings": loaded.warnings,
    });
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
