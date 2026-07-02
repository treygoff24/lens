//! `lens.cli.response.v1` (stdout, success) and `lens.cli.error.v1`
//! (stderr, failure) envelopes. On failure stdout stays empty — the whole
//! result travels in the error envelope on stderr.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::error::LensError;

fn new_request_id(request_id: Option<String>) -> String {
    request_id.unwrap_or_else(|| {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let bits = nanos
            ^ ((COUNTER.fetch_add(1, Ordering::Relaxed) as u128) << 64)
            ^ (std::process::id() as u128);
        let hex = format!("{bits:032x}");
        format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        )
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostDollars {
    pub model: f64,
    pub search: f64,
    pub total: f64,
    pub estimated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Budget {
    /// "dollars" | "seconds" | null
    pub hit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostics {
    pub duration_ms: u64,
    pub retries: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SuccessEnvelope {
    pub schema: String,
    pub ok: bool,
    pub command: String,
    pub request_id: String,
    pub data: serde_json::Value,
    pub cost_dollars: CostDollars,
    pub budget: Budget,
    pub diagnostics: Diagnostics,
}

impl SuccessEnvelope {
    pub fn new(
        command: impl Into<String>,
        data: serde_json::Value,
        cost_dollars: CostDollars,
        budget: Budget,
        diagnostics: Diagnostics,
        request_id: Option<String>,
    ) -> Self {
        Self {
            schema: "lens.cli.response.v1".to_string(),
            ok: true,
            command: command.into(),
            request_id: new_request_id(request_id),
            data,
            cost_dollars,
            budget,
            diagnostics,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorDetail {
    pub code: String,
    pub category: String,
    pub retryable: bool,
    pub provider: Option<String>,
    pub message: String,
    pub partial: Option<serde_json::Value>,
    pub suggested_fix: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorEnvelope {
    pub schema: String,
    pub ok: bool,
    pub command: String,
    pub request_id: String,
    pub error: ErrorDetail,
    /// The process exit code for this error. Not part of the JSON contract
    /// (the consumer reads the process exit status, not this payload) — kept
    /// on the envelope so `emit_error` can return it without re-deriving it
    /// from `LensError` a second time.
    #[serde(skip)]
    pub exit_code: i32,
}

impl ErrorEnvelope {
    pub fn from_error(
        command: impl Into<String>,
        err: &LensError,
        request_id: Option<String>,
    ) -> Self {
        Self {
            schema: "lens.cli.error.v1".to_string(),
            ok: false,
            command: command.into(),
            request_id: new_request_id(request_id),
            error: ErrorDetail {
                code: err.code().to_string(),
                category: err.category().to_string(),
                retryable: err.is_retryable(),
                provider: err.provider().map(|p| p.as_str().to_string()),
                message: err.to_string(),
                partial: err.partial_data().cloned(),
                suggested_fix: err.suggested_fix().map(ToString::to_string),
            },
            exit_code: err.exit_code(),
        }
    }
}

/// Prints the success envelope. Human-readable text when stdout is a TTY and
/// JSON wasn't forced; compact JSON otherwise (piped, or `--json`/`force_json`).
pub fn emit_success(env: &SuccessEnvelope, force_json: bool) {
    let stdout = io::stdout();
    if !force_json && stdout.is_terminal() {
        render_success_human(env);
    } else {
        let mut lock = stdout.lock();
        let _ = writeln!(
            lock,
            "{}",
            serde_json::to_string(env).expect("envelope serializes")
        );
    }
}

/// Prints the error envelope to stderr (stdout stays empty on failure) and
/// returns the process exit code for this error.
pub fn emit_error(env: &ErrorEnvelope, force_json: bool) -> i32 {
    let stderr = io::stderr();
    if !force_json && stderr.is_terminal() {
        render_error_human(env);
    } else {
        let mut lock = stderr.lock();
        let _ = writeln!(
            lock,
            "{}",
            serde_json::to_string(env).expect("envelope serializes")
        );
    }
    env.exit_code
}

fn render_success_human(env: &SuccessEnvelope) {
    println!("lens {} — ok (requestId {})", env.command, env.request_id);
    println!(
        "cost: ${:.4} (model ${:.4} + search ${:.4}){}",
        env.cost_dollars.total,
        env.cost_dollars.model,
        env.cost_dollars.search,
        if env.cost_dollars.estimated {
            " [estimated]"
        } else {
            ""
        }
    );
    if let Some(hit) = &env.budget.hit {
        println!("budget hit: {hit}");
    }
    println!(
        "duration: {}ms, retries: {}",
        env.diagnostics.duration_ms, env.diagnostics.retries
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&env.data).unwrap_or_else(|_| env.data.to_string())
    );
}

fn render_error_human(env: &ErrorEnvelope) {
    let stderr = io::stderr();
    let mut lock = stderr.lock();
    let _ = render_error_human_to(env, &mut lock);
}

fn render_error_human_to(env: &ErrorEnvelope, writer: &mut impl Write) -> io::Result<()> {
    writeln!(
        writer,
        "lens {} — error [{}] (requestId {})",
        env.command, env.error.code, env.request_id
    )?;
    writeln!(writer, "{}", env.error.message)?;
    if let Some(provider) = &env.error.provider {
        writeln!(writer, "provider: {provider}")?;
    }
    writeln!(writer, "retryable: {}", env.error.retryable)?;
    if let Some(partial) = &env.error.partial {
        writeln!(
            writer,
            "partial: {}",
            serde_json::to_string_pretty(partial).unwrap_or_else(|_| partial.to_string())
        )?;
    }
    if let Some(suggested_fix) = &env.error.suggested_fix {
        writeln!(writer, "fix: {suggested_fix}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Provider;

    const FIXED_REQUEST_ID: &str = "00000000-0000-0000-0000-000000000000";

    #[test]
    fn golden_success_envelope_has_exact_camel_case_fields() {
        let env = SuccessEnvelope::new(
            "ask",
            serde_json::json!({"question": "what is prospera", "outcome": "answered"}),
            CostDollars {
                model: 0.09,
                search: 0.04,
                total: 0.13,
                estimated: false,
            },
            Budget { hit: None },
            Diagnostics {
                duration_ms: 12100,
                retries: 0,
            },
            Some(FIXED_REQUEST_ID.to_string()),
        );

        let json = serde_json::to_value(&env).unwrap();

        assert_eq!(json["schema"], "lens.cli.response.v1");
        assert_eq!(json["ok"], true);
        assert_eq!(json["command"], "ask");
        assert_eq!(json["requestId"], FIXED_REQUEST_ID);
        assert_eq!(json["data"]["outcome"], "answered");
        assert_eq!(json["costDollars"]["model"], 0.09);
        assert_eq!(json["costDollars"]["search"], 0.04);
        assert_eq!(json["costDollars"]["total"], 0.13);
        assert_eq!(json["costDollars"]["estimated"], false);
        assert_eq!(json["budget"]["hit"], serde_json::Value::Null);
        assert_eq!(json["diagnostics"]["durationMs"], 12100);
        assert_eq!(json["diagnostics"]["retries"], 0);

        // No snake_case leakage.
        assert!(json.get("request_id").is_none());
        assert!(json.get("cost_dollars").is_none());
        assert!(json["diagnostics"].get("duration_ms").is_none());
    }

    #[test]
    fn golden_error_envelope_has_exact_camel_case_fields() {
        let err = LensError::rate_limit("Cerebras returned 429")
            .with_provider(Provider::Cerebras)
            .with_retryable(true)
            .with_partial(serde_json::json!({"claims": []}))
            .with_suggested_fix("wait and retry; Cerebras rate windows are minute-scale");

        let env = ErrorEnvelope::from_error("ask", &err, Some(FIXED_REQUEST_ID.to_string()));
        let json = serde_json::to_value(&env).unwrap();

        assert_eq!(json["schema"], "lens.cli.error.v1");
        assert_eq!(json["ok"], false);
        assert_eq!(json["command"], "ask");
        assert_eq!(json["requestId"], FIXED_REQUEST_ID);
        assert_eq!(json["error"]["code"], "rate_limited");
        assert_eq!(json["error"]["category"], "rate_limited");
        assert_eq!(json["error"]["retryable"], true);
        assert_eq!(json["error"]["provider"], "cerebras");
        assert_eq!(
            json["error"]["message"],
            "rate limited: Cerebras returned 429"
        );
        assert_eq!(json["error"]["partial"], serde_json::json!({"claims": []}));
        assert_eq!(
            json["error"]["suggestedFix"],
            "wait and retry; Cerebras rate windows are minute-scale"
        );

        assert!(json.get("request_id").is_none());
        // exit_code is intentionally not part of the JSON contract.
        assert!(json.get("exitCode").is_none());
        assert!(json.get("exit_code").is_none());

        assert_eq!(env.exit_code, 6);
    }

    #[test]
    fn error_envelope_without_provider_or_partial_omits_neither_field() {
        let err = LensError::usage("unknown flag --frobnicate");
        let env = ErrorEnvelope::from_error("ask", &err, Some(FIXED_REQUEST_ID.to_string()));
        let json = serde_json::to_value(&env).unwrap();

        assert_eq!(json["error"]["provider"], serde_json::Value::Null);
        assert_eq!(json["error"]["partial"], serde_json::Value::Null);
        assert_eq!(json["error"]["suggestedFix"], serde_json::Value::Null);
        assert_eq!(env.exit_code, 1);
    }

    #[test]
    fn request_id_defaults_to_a_uuid_shaped_value_when_none_given() {
        let env = SuccessEnvelope::new(
            "capabilities",
            serde_json::json!({}),
            CostDollars {
                model: 0.0,
                search: 0.0,
                total: 0.0,
                estimated: false,
            },
            Budget { hit: None },
            Diagnostics {
                duration_ms: 1,
                retries: 0,
            },
            None,
        );
        assert_eq!(env.request_id.len(), 36);
        assert_eq!(&env.request_id[8..9], "-");
        assert_eq!(&env.request_id[13..14], "-");
        assert_eq!(&env.request_id[18..19], "-");
        assert_eq!(&env.request_id[23..24], "-");
    }

    #[test]
    fn human_error_renderer_prints_suggested_fix() {
        let err = LensError::auth("missing CEREBRAS_API_KEY")
            .with_provider(Provider::Cerebras)
            .with_suggested_fix("set CEREBRAS_API_KEY");
        let env = ErrorEnvelope::from_error("doctor", &err, Some(FIXED_REQUEST_ID.to_string()));
        let mut out = Vec::new();

        render_error_human_to(&env, &mut out).unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                "lens doctor — error [auth] (requestId 00000000-0000-0000-0000-000000000000)\n",
                "authentication failed: missing CEREBRAS_API_KEY\n",
                "provider: cerebras\n",
                "retryable: false\n",
                "fix: set CEREBRAS_API_KEY\n",
            )
        );
    }
}
