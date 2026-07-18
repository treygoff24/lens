use std::time::Instant;

use serde::Serialize;

use crate::cli::{GlobalArgs, StatusArgs};
use crate::commands::common::{
    CommandSuccess, IndexStateData, load_index_state, load_library, zero_cost_success,
};
use crate::error::LensError;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusData {
    #[serde(flatten)]
    state: IndexStateData,
    projected_cost_dollars: f64,
    warnings: Vec<String>,
}

pub fn run(
    global: &GlobalArgs,
    args: &StatusArgs,
) -> Result<CommandSuccess<impl Serialize>, LensError> {
    let started = Instant::now();
    let (library_path, _, model, store) = load_library(global, &args.dir)?;
    let state = load_index_state(&library_path, &store, &model, global.concurrency)?;
    Ok(zero_cost_success(
        "status",
        StatusData {
            state: state.data,
            projected_cost_dollars: state.projected_cost_dollars,
            warnings: state.warnings,
        },
        &started,
        "run `lens index --dry-run --json` to see planned captioning work",
    ))
}
