use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, to_value};

use crate::budget::Budget as RunBudget;
use crate::cli::{GlobalArgs, IndexArgs};
use crate::commands::{
    CommandSuccess, cost_from_spend, model_from, new_shared_spend, open_store, require_key,
    resolve_library_path, retries_from_spend,
};
use crate::config::Config;
use crate::envelope::{Budget, Diagnostics, SuccessEnvelope};
use crate::error::LensError;
use crate::indexer::{CAPTION_WORST_CASE_COST, IndexOptions, IndexOutcome, index_library};
use crate::providers::cerebras::CerebrasClient;
use crate::walk::{partition_freshness, walk_library};

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
    // F2+F3: in human mode, print warnings to stderr. In JSON mode they ride
    // the envelope.
    if !global.json {
        for warning in &report.warnings {
            eprintln!("warning: {warning}");
        }
    }
    let mut data = to_value(report)
        .map_err(|err| LensError::upstream(format!("failed to serialize index report: {err}")))?;
    data["outcome"] = json!(match outcome {
        IndexOutcome::Complete(_) => "complete",
        IndexOutcome::Partial(_) => "partial",
    });
    let exit_code = if matches!(outcome, IndexOutcome::Partial(_)) {
        10
    } else {
        0
    };
    let envelope = SuccessEnvelope::new(
        "index",
        data,
        cost_from_spend(&spend, false)?,
        Budget {
            hit: budget.hit().map(str::to_string),
        },
        Diagnostics {
            duration_ms: started.elapsed().as_millis() as u64,
            retries: retries_from_spend(&spend)?,
        },
        None,
    );
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
        "outcome": "planned",
        "dryRun": true,
        "plannedWork": indexable,
        "indexed": loaded.records.len(),
        "fresh": freshness.fresh.len(),
        "stale": freshness.stale.len(),
        "new": freshness.new.len(),
        "vanished": freshness.vanished.len(),
        "staleAll": loaded.stale_all,
        "projectedWorstCaseCost": projected,
        "projectedWallSeconds": indexable as f64 / f64::from(global.concurrency) * 1.5,
    });
    let envelope = SuccessEnvelope::new(
        "index",
        data,
        crate::envelope::CostDollars {
            model: projected,
            search: 0.0,
            total: projected,
            estimated: true,
        },
        Budget { hit: None },
        Diagnostics {
            duration_ms: 0,
            retries: 0,
        },
        None,
    );
    Ok(CommandSuccess {
        envelope,
        exit_code: 0,
        hint: Some("remove --dry-run to caption and write the index"),
    })
}
