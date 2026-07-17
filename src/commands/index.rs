use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use crate::budget::Budget as RunBudget;
use crate::cli::{GlobalArgs, IndexArgs};
use crate::commands::common::{
    CommandSuccess, IndexStateData, estimated_envelope, metered_envelope, model_from,
    new_shared_spend, open_store, require_key, resolve_library_path, response_value,
};
use crate::config::Config;
use crate::error::LensError;
use crate::indexer::{
    CAPTION_WORST_CASE_COST, IndexOptions, IndexOutcome, IndexReport, index_library,
};
use crate::providers::cerebras::CerebrasClient;
use crate::walk::{partition_with_stale_all, walk_library};

#[derive(Serialize)]
struct IndexData<'a> {
    #[serde(flatten)]
    report: &'a IndexReport,
    outcome: &'static str,
}

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

pub fn run(global: &GlobalArgs, args: &IndexArgs) -> Result<CommandSuccess, LensError> {
    let started = Instant::now();
    let library_path = resolve_library_path(&args.dir)?;
    let cfg = Config::load()?;
    let model = model_from(&global.model, &cfg);
    let store = open_store(&library_path, global.index_path.as_ref())?;

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
    let report = outcome.report();
    if !global.json {
        for warning in &report.warnings {
            eprintln!("warning: {warning}");
        }
    }
    let outcome_name = match outcome {
        IndexOutcome::Complete(_) => "complete",
        IndexOutcome::Partial(_) => "partial",
    };
    let data = response_value(
        "index report",
        IndexData {
            report,
            outcome: outcome_name,
        },
    )?;
    let exit_code = if matches!(outcome, IndexOutcome::Partial(_)) {
        10
    } else {
        0
    };
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
) -> Result<CommandSuccess, LensError> {
    let walked = walk_library(library_path)?;
    let loaded = store.load(model)?;
    let freshness = partition_with_stale_all(&walked, &loaded.records, loaded.stale_all);
    let indexable = freshness.stale.len() + freshness.new.len();
    let projected = indexable as f64 * CAPTION_WORST_CASE_COST;
    let envelope = estimated_envelope(
        "index",
        "index plan",
        IndexDryRunData {
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
            outcome: "planned",
            dry_run: true,
            planned_work: indexable,
            projected_worst_case_cost: projected,
        },
        projected,
    )?;
    Ok(CommandSuccess {
        envelope,
        exit_code: 0,
        hint: Some("remove --dry-run to caption and write the index"),
    })
}
