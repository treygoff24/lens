use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use crate::budget::Budget as RunBudget;
use crate::cli::{FindArgs, GlobalArgs};
use crate::commands::common::{
    CommandSuccess, estimated_envelope, metered_envelope, model_from, new_shared_spend, open_store,
    require_key, resolve_library_path, response_value, zero_cost_success,
};
use crate::config::Config;
use crate::error::LensError;
use crate::find::{FindContext, FindData, FindHit, FindOptions, find_with_options, plan};
use crate::providers::cerebras::CerebrasClient;
use crate::store::IndexRecord;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FindDryRunData<'a> {
    query: &'a str,
    dry_run: bool,
    #[serde(flatten)]
    plan: crate::find::FindPlan,
    warnings: Vec<String>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    kind_filter: &'a [String],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EmptyFindData<'a> {
    query: &'a str,
    hits: [FindHit; 0],
    searched: usize,
    mode: String,
    chunks: usize,
    warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'static str>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    kind_filter: &'a [String],
}

#[derive(Serialize)]
struct AnsweredFindData {
    #[serde(flatten)]
    data: FindData,
    outcome: &'static str,
}

pub fn run(global: &GlobalArgs, args: &FindArgs) -> Result<CommandSuccess, LensError> {
    let started = Instant::now();
    if args.query.trim().is_empty() {
        return Err(LensError::no_input("find requires a non-empty <QUERY>")
            .with_suggested_fix("run `lens find \"beach club hero shot\" --dir ./photos`"));
    }
    let library_path = resolve_library_path(&args.dir)?;
    let cfg = Config::load()?;
    let model = model_from(&global.model, &cfg);
    let store = open_store(&library_path, global.index_path.as_ref())?;
    let mut loaded = store.load(&model)?;
    let unfiltered_count = loaded.records.len();
    let kind_filter = dedupe_kinds(args.kind.clone());
    let kinds_present = if kind_filter.is_empty() {
        Vec::new()
    } else {
        sorted_kinds(&loaded.records)
    };
    if !kind_filter.is_empty() {
        let wanted = kind_filter.iter().cloned().collect::<HashSet<_>>();
        loaded
            .records
            .retain(|record| wanted.contains(&normalize_kind(&record.kind)));
    }
    let zero_kind_warning =
        (!kind_filter.is_empty() && unfiltered_count > 0 && loaded.records.is_empty())
            .then(|| zero_kind_warning(unfiltered_count, &kinds_present));

    if global.dry_run {
        let plan = plan(&args.query, &loaded.records, args.top);
        let mut warnings = loaded.warnings.clone();
        if let Some(warning) = zero_kind_warning.clone() {
            warnings.push(warning);
        }
        let projected_cost = plan.projected_cost_dollars;
        let envelope = estimated_envelope(
            "find",
            "find data",
            FindDryRunData {
                query: &args.query,
                dry_run: true,
                plan,
                warnings,
                kind_filter: &kind_filter,
            },
            projected_cost,
        )?;
        return Ok(CommandSuccess {
            envelope,
            exit_code: 0,
            hint: Some("remove --dry-run to spend one model call on search"),
        });
    }

    if unfiltered_count == 0 {
        return zero_cost_success(
            "find",
            EmptyFindData {
                query: &args.query,
                hits: [],
                searched: 0,
                mode: "single_shot".into(),
                chunks: 0,
                warnings: vec!["index is empty; run lens index".into()],
                outcome: None,
                kind_filter: &[],
            },
            &started,
            "run `lens index --json` before searching this library",
        );
    }

    if let Some(warning) = zero_kind_warning {
        let mut warnings = loaded.warnings;
        warnings.push(warning);
        return zero_cost_success(
            "find",
            EmptyFindData {
                query: &args.query,
                hits: [],
                searched: 0,
                mode: "single_shot".into(),
                chunks: 0,
                warnings,
                outcome: Some("answered"),
                kind_filter: &kind_filter,
            },
            &started,
            "try a --kind value present in the index",
        );
    }

    let api_key = require_key(cfg.cerebras_api_key.as_deref())?;
    let spend = new_shared_spend();
    let chat = CerebrasClient::new(api_key, cfg.api_base, model).with_spend(Arc::clone(&spend));
    let budget = RunBudget::new(global.max_dollars, global.max_seconds);
    // The CLI default is a sentinel here; an unchanged value defers to config.
    let concurrency = if global.concurrency != 25 {
        usize::from(global.concurrency)
    } else {
        cfg.max_concurrency as usize
    };
    let options = FindOptions { concurrency };
    let data = match find_with_options(
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
            data.kind_filter = kind_filter.clone();
            if loaded.stale_all {
                data.warnings
                    .push("index metadata is stale; run lens index".into());
            }
            if let Some(path) = &args.gallery {
                write_gallery(path, &data)?;
                data.gallery_path = Some(path.to_string_lossy().to_string());
            }
            response_value(
                "find data",
                AnsweredFindData {
                    data,
                    outcome: "answered",
                },
            )?
        }
        Err(err) if err.exit_code() == 10 && budget.hit().is_some() => {
            let plan = plan(&args.query, &loaded.records, args.top);
            response_value(
                "find data",
                EmptyFindData {
                    query: &args.query,
                    hits: [],
                    searched: loaded.records.len(),
                    mode: plan.mode,
                    chunks: plan.chunks,
                    warnings: vec!["budget refused find call".into()],
                    outcome: Some("refused"),
                    kind_filter: &kind_filter,
                },
            )?
        }
        Err(err) => return Err(err),
    };
    let envelope = metered_envelope("find", data, &spend, budget.hit(), &started);
    let exit_code = if budget.hit().is_some() { 10 } else { 0 };
    Ok(CommandSuccess {
        envelope,
        exit_code,
        hint: Some("use --gallery PATH to write a contact sheet for human review"),
    })
}

fn normalize_kind(kind: &str) -> String {
    kind.trim().to_lowercase()
}

fn dedupe_kinds(kinds: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    kinds
        .into_iter()
        .filter(|kind| seen.insert(kind.clone()))
        .collect()
}

fn sorted_kinds(records: &[IndexRecord]) -> Vec<String> {
    records
        .iter()
        .map(|record| normalize_kind(&record.kind))
        .filter(|kind| !kind.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn zero_kind_warning(unfiltered_count: usize, kinds_present: &[String]) -> String {
    format!(
        "kind filter matched 0 of {unfiltered_count} records; kinds present: {}",
        kinds_present.join(",")
    )
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
    use super::{dedupe_kinds, escape_html};

    #[test]
    fn html_escape_covers_model_text_boundaries() {
        assert_eq!(escape_html("<&>\"'"), "&lt;&amp;&gt;&quot;&#39;");
    }

    #[test]
    fn dedupe_kinds_preserves_first_occurrence_order() {
        assert_eq!(dedupe_kinds(vec![]), Vec::<String>::new());
        assert_eq!(
            dedupe_kinds(vec!["photo".into()]),
            vec!["photo".to_string()]
        );
        assert_eq!(
            dedupe_kinds(vec!["photo".into(), "photo".into()]),
            vec!["photo".to_string()]
        );
        assert_eq!(
            dedupe_kinds(vec![
                "photo".into(),
                "graphic".into(),
                "photo".into(),
                "screenshot".into(),
                "graphic".into()
            ]),
            vec![
                "photo".to_string(),
                "graphic".to_string(),
                "screenshot".to_string()
            ]
        );
    }
}
