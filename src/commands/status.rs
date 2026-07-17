use std::time::Instant;

use serde::Serialize;

use crate::cli::{GlobalArgs, StatusArgs};
use crate::commands::common::{
    CommandSuccess, IndexStateData, model_from, open_store, resolve_library_path, zero_cost_success,
};
use crate::config::Config;
use crate::error::LensError;
use crate::indexer::CAPTION_WORST_CASE_COST;
use crate::walk::{partition_with_stale_all, walk_library};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusData {
    #[serde(flatten)]
    state: IndexStateData,
    projected_cost_dollars: f64,
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
    let freshness = partition_with_stale_all(&walked, &loaded.records, loaded.stale_all);
    let indexable = freshness.stale.len() + freshness.new.len();
    let projected = indexable as f64 * CAPTION_WORST_CASE_COST;
    zero_cost_success(
        "status",
        StatusData {
            state: IndexStateData {
                library_path: library_path.to_string_lossy().into_owned(),
                index_path: store.dir().to_string_lossy().into_owned(),
                indexed: loaded.records.len(),
                fresh: freshness.fresh.len(),
                stale: freshness.stale.len(),
                new: freshness.new.len(),
                vanished: freshness.vanished.len(),
                stale_all: loaded.stale_all,
                projected_wall_seconds: indexable as f64 / f64::from(global.concurrency) * 1.5,
            },
            projected_cost_dollars: projected,
            warnings: loaded.warnings,
        },
        &started,
        "run `lens index --dry-run --json` to see planned captioning work",
    )
}
