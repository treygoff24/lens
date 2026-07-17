use std::time::Instant;

use serde::Serialize;

use crate::cli::{GlobalArgs, StatusArgs};
use crate::commands::common::{
    CommandSuccess, IndexStateData, load_index_state, model_from, open_store, resolve_library_path,
    zero_cost_success,
};
use crate::config::Config;
use crate::error::LensError;

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
    let state = load_index_state(&library_path, &store, &model, global.concurrency)?;
    zero_cost_success(
        "status",
        StatusData {
            state: state.data,
            projected_cost_dollars: state.projected_cost_dollars,
            warnings: state.warnings,
        },
        &started,
        "run `lens index --dry-run --json` to see planned captioning work",
    )
}
