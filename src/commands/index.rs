use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use crate::budget::Budget as RunBudget;
use crate::cli::{GlobalArgs, IndexArgs};
use crate::commands::common::{
    CommandSuccess, IndexStateData, OutcomeData, estimated_envelope, load_index_state,
    load_library, metered_envelope, new_shared_spend, require_key,
};
use crate::error::LensError;
use crate::indexer::{IndexOptions, IndexOutcome, index_library};
use crate::providers::cerebras::CerebrasClient;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexDryRunData {
    #[serde(flatten)]
    state: IndexStateData,
    outcome: &'static str,
    dry_run: bool,
    planned_work: usize,
    projected_worst_case_cost: f64,
}

#[derive(Serialize)]
#[serde(untagged)]
enum IndexResponseData {
    DryRun(IndexDryRunData),
    Report(OutcomeData<crate::indexer::IndexReport>),
}

pub fn run(
    global: &GlobalArgs,
    args: &IndexArgs,
) -> Result<CommandSuccess<impl Serialize>, LensError> {
    let started = Instant::now();
    let (library_path, cfg, model, store) = load_library(global, &args.dir)?;

    if global.dry_run {
        return dry_run(global, &library_path, &store, &model);
    }

    let api_key = require_key(cfg.cerebras_api_key.as_deref())?;
    let spend = new_shared_spend();
    let chat =
        CerebrasClient::new(api_key, cfg.api_base, model.clone()).with_spend(Arc::clone(&spend));
    let budget = RunBudget::new(global.max_dollars, global.max_seconds);
    let mut options = IndexOptions::new(model);
    options.concurrency = usize::from(global.concurrency);

    let outcome = index_library(&library_path, &store, &chat, &budget, &options)?;
    let is_partial = matches!(outcome, IndexOutcome::Partial(_));
    let (report, outcome_name) = match outcome {
        IndexOutcome::Complete(report) => (report, "complete"),
        IndexOutcome::Partial(report) => (report, "partial"),
    };
    if !global.json {
        for warning in &report.warnings {
            eprintln!("warning: {warning}");
        }
    }
    let data = IndexResponseData::Report(OutcomeData {
        data: report,
        outcome: Some(outcome_name),
    });
    let exit_code = if is_partial { 10 } else { 0 };
    let envelope = metered_envelope("index", data, &spend, budget.hit(), &started);
    Ok(CommandSuccess {
        envelope,
        exit_code,
        hint: Some("run `lens status --json` to inspect remaining stale or new images"),
    })
}

fn dry_run(
    global: &GlobalArgs,
    library_path: &std::path::Path,
    store: &crate::store::Store,
    model: &str,
) -> Result<CommandSuccess<IndexResponseData>, LensError> {
    let state = load_index_state(library_path, store, model, global.concurrency)?;
    let envelope = estimated_envelope(
        "index",
        IndexResponseData::DryRun(IndexDryRunData {
            state: state.data,
            outcome: "planned",
            dry_run: true,
            planned_work: state.planned_work,
            projected_worst_case_cost: state.projected_cost_dollars,
        }),
        state.projected_cost_dollars,
    );
    Ok(CommandSuccess {
        envelope,
        exit_code: 0,
        hint: Some("remove --dry-run to caption and write the index"),
    })
}
