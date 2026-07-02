use std::fs;
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, to_value};

use crate::budget::Budget as RunBudget;
use crate::cli::{FindArgs, GlobalArgs};
use crate::commands::{
    CommandSuccess, cost_from_spend, model_from, new_shared_spend, open_store, require_key,
    resolve_library_path, retries_from_spend,
};
use crate::config::Config;
use crate::envelope::{Budget, Diagnostics, SuccessEnvelope};
use crate::error::LensError;
use crate::find::{FindContext, FindOptions, find_with_options, plan};
use crate::providers::cerebras::CerebrasClient;

pub fn run(global: &GlobalArgs, args: &FindArgs) -> Result<CommandSuccess, LensError> {
    let started = Instant::now();
    let library_path = resolve_library_path(&args.dir)?;
    let cfg = Config::load()?;
    let model = model_from(&global.model, &cfg);
    let store = open_store(&library_path, global.index_path.as_ref())?;
    let loaded = store.load(&model)?;

    if global.dry_run {
        let plan = plan(&args.query, &loaded.records, args.top);
        let data = json!({
            "query": args.query,
            "dryRun": true,
            "searched": plan.searched,
            "mode": plan.mode,
            "chunks": plan.chunks,
            "estimatedTokens": plan.estimated_tokens,
            "projectedCostDollars": plan.projected_cost_dollars,
            "warnings": loaded.warnings,
        });
        let envelope = SuccessEnvelope::new(
            "find",
            data,
            crate::envelope::CostDollars {
                model: plan.projected_cost_dollars,
                search: 0.0,
                total: plan.projected_cost_dollars,
                estimated: true,
            },
            Budget { hit: None },
            Diagnostics {
                duration_ms: 0,
                retries: 0,
            },
            None,
        );
        return Ok(CommandSuccess {
            envelope,
            exit_code: 0,
            hint: Some("remove --dry-run to spend one model call on search"),
        });
    }

    if loaded.records.is_empty() {
        let data = json!({
            "query": args.query,
            "hits": [],
            "searched": 0,
            "mode": "single_shot",
            "chunks": 0,
            "warnings": ["index is empty; run lens index"]
        });
        let envelope = SuccessEnvelope::new(
            "find",
            data,
            crate::envelope::CostDollars {
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
        return Ok(CommandSuccess {
            envelope,
            exit_code: 0,
            hint: Some("run `lens index --json` before searching this library"),
        });
    }

    let api_key = require_key(cfg.cerebras_api_key.as_deref())?;
    let spend = new_shared_spend();
    let chat = CerebrasClient::new(api_key, cfg.api_base, model).with_spend(Arc::clone(&spend));
    let budget = RunBudget::new(global.max_dollars, global.max_seconds);
    // F8: wire --concurrency into find. Use the global flag when set (non-
    // default), otherwise fall back to config.
    let concurrency = if global.concurrency != 25 {
        usize::from(global.concurrency)
    } else {
        cfg.max_concurrency as usize
    };
    let options = FindOptions { concurrency };
    let mut data = match find_with_options(
        &args.query,
        &library_path,
        &loaded.records,
        args.top,
        FindContext {
            chat: &chat,
            budget: &budget,
            spend: &spend,
        },
        &options,
    ) {
        Ok(mut data) => {
            data.warnings.extend(loaded.warnings);
            if loaded.stale_all {
                data.warnings
                    .push("index metadata is stale; run lens index".into());
            }
            if let Some(path) = &args.gallery {
                write_gallery(path, &data)?;
                data.gallery_path = Some(path.to_string_lossy().to_string());
            }
            to_value(data).map_err(|err| {
                LensError::upstream(format!("failed to serialize find data: {err}"))
            })?
        }
        Err(err) if err.exit_code() == 10 && budget.hit().is_some() => json!({
            "query": args.query,
            "outcome": "refused",
            "hits": [],
            "searched": loaded.records.len(),
            "mode": plan(&args.query, &loaded.records, args.top).mode,
            "chunks": plan(&args.query, &loaded.records, args.top).chunks,
            "warnings": ["budget refused find call"]
        }),
        Err(err) => return Err(err),
    };
    if data.get("outcome").is_none() {
        data["outcome"] = json!("answered");
    }
    let envelope = SuccessEnvelope::new(
        "find",
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
    let exit_code = if budget.hit().is_some() { 10 } else { 0 };
    Ok(CommandSuccess {
        envelope,
        exit_code,
        hint: Some("use --gallery PATH to write a contact sheet for human review"),
    })
}

fn write_gallery(path: &std::path::Path, data: &crate::find::FindData) -> Result<(), LensError> {
    let mut cards = String::new();
    for hit in &data.hits {
        cards.push_str(&format!(
            "<figure><img src=\"file://{}\" loading=\"lazy\"><figcaption><b>{}</b><br>{}</figcaption></figure>",
            escape_html(&hit.path),
            escape_html(&hit.filename),
            escape_html(&hit.description)
        ));
    }
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{}</title>\n<style>body{{font:14px/1.4 -apple-system,sans-serif;background:#111;color:#ddd;margin:2rem}}\nh1{{font-weight:600}} .g{{display:grid;grid-template-columns:repeat(auto-fill,minmax(320px,1fr));gap:1rem}}\nfigure{{margin:0;background:#1c1c1e;border-radius:10px;overflow:hidden}}\nimg{{width:100%;height:220px;object-fit:cover;display:block}}\nfigcaption{{padding:.6rem .8rem;color:#aaa}} figcaption b{{color:#eee}}</style>\n<h1>“{}”</h1><div class=\"g\">{cards}</div>",
        escape_html(&data.query),
        escape_html(&data.query)
    );
    fs::write(path, html).map_err(|err| {
        LensError::config(format!("failed to write gallery {}: {err}", path.display()))
    })
}

fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::escape_html;

    #[test]
    fn html_escape_covers_model_text_boundaries() {
        assert_eq!(escape_html("<&>\"'"), "&lt;&amp;&gt;&quot;&#39;");
    }
}
