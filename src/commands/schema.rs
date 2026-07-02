use serde_json::{Value, json};

use crate::cli::{GlobalArgs, SchemaArgs, SchemaTarget};
use crate::commands::{CommandSuccess, zero_envelope};
use crate::error::LensError;

pub fn run(_global: &GlobalArgs, args: &SchemaArgs) -> Result<CommandSuccess, LensError> {
    let data = match args.target {
        SchemaTarget::Response => response_schema(),
        SchemaTarget::Error => error_schema(),
        SchemaTarget::All => json!({"response": response_schema(), "error": error_schema()}),
    };
    Ok(CommandSuccess {
        envelope: zero_envelope("schema", data),
        exit_code: 0,
        hint: None,
    })
}

fn response_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "lens.cli.response.v1",
        "type": "object",
        "required": ["schema", "ok", "command", "requestId", "data", "costDollars", "budget", "diagnostics"],
        "properties": {
            "schema": {"const": "lens.cli.response.v1"},
            "ok": {"const": true},
            "command": {"enum": ["index", "find", "status", "doctor", "capabilities", "schema", "help", "version"]},
            "requestId": {"type": "string"},
            "data": {"oneOf": [
                {"type": "object", "description": "index report", "properties": {"outcome": {"type": "string"}, "indexed": {"type": "integer"}, "skipped": {"type": "array"}, "failed": {"type": "array"}, "pruned": {"type": "integer"}}},
                {"type": "object", "description": "find data", "properties": {"query": {"type": "string"}, "hits": {"type": "array"}, "searched": {"type": "integer"}, "mode": {"enum": ["single_shot", "chunked"]}, "chunks": {"type": "integer"}, "warnings": {"type": "array"}, "galleryPath": {"type": "string"}, "outcome": {"type": "string"}, "dryRun": {"type": "boolean"}, "estimatedTokens": {"type": "integer"}, "projectedCostDollars": {"type": "number"}, "invalidIdsDropped": {"type": "integer"}}},
                {"type": "object", "description": "status", "properties": {"libraryPath": {"type": "string"}, "indexPath": {"type": "string"}, "indexed": {"type": "integer"}, "fresh": {"type": "integer"}, "stale": {"type": "integer"}, "new": {"type": "integer"}, "vanished": {"type": "integer"}, "staleAll": {"type": "boolean"}}},
                {"type": "object", "description": "doctor report", "properties": {"schemaVersion": {"type": "string"}, "status": {"enum": ["healthy", "degraded", "broken"]}, "summary": {"type": "object"}, "checks": {"type": "array"}, "runId": {"type": ["string", "null"]}}},
                {"type": "object", "description": "capabilities payload"},
                {"type": "object", "description": "schema payload", "properties": {"response": {"type": "object"}, "error": {"type": "object"}}},
                {"type": "object", "description": "help/version payload", "required": ["text"], "properties": {"text": {"type": "string"}}}
            ]},
            "costDollars": {"type": "object", "required": ["model", "search", "total", "estimated"], "properties": {"model": {"type": "number"}, "search": {"type": "number"}, "total": {"type": "number"}, "estimated": {"type": "boolean"}}},
            "budget": {"type": "object", "required": ["hit"], "properties": {"hit": {"enum": ["dollars", "seconds", null]}}},
            "diagnostics": {"type": "object", "required": ["durationMs", "retries"], "properties": {"durationMs": {"type": "integer"}, "retries": {"type": "integer"}}}
        }
    })
}

fn error_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "lens.cli.error.v1",
        "type": "object",
        "required": ["schema", "ok", "command", "requestId", "error"],
        "properties": {
            "schema": {"const": "lens.cli.error.v1"},
            "ok": {"const": false},
            "command": {"type": "string"},
            "requestId": {"type": "string"},
            "error": {"type": "object", "required": ["code", "category", "retryable", "provider", "message", "partial", "suggestedFix"], "properties": {"code": {"enum": ["usage", "auth", "config", "network", "upstream", "rate_limited", "partial", "no_input"]}, "category": {"type": "string"}, "retryable": {"type": "boolean"}, "provider": {"enum": ["cerebras", null]}, "message": {"type": "string"}, "partial": {"type": ["object", "array", "string", "number", "boolean", "null"]}, "suggestedFix": {"type": ["string", "null"]}}}
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // F13: verify the find data variant in the response schema includes all
    // the required property names.
    #[test]
    fn find_data_variant_has_required_properties() {
        let schema = response_schema();
        let data_variants = schema
            .get("properties")
            .and_then(|p| p.get("data"))
            .and_then(|d| d.get("oneOf"))
            .and_then(|v| v.as_array())
            .expect("data.oneOf must be an array");

        let find_variant = data_variants
            .iter()
            .find(|v| v.get("description").and_then(|d| d.as_str()) == Some("find data"))
            .expect("find data variant must exist");

        let props = find_variant
            .get("properties")
            .expect("find data variant must have properties");

        // Core fields.
        assert!(props.get("query").is_some());
        assert!(props.get("hits").is_some());
        assert!(props.get("searched").is_some());
        assert!(props.get("mode").is_some());
        assert!(props.get("chunks").is_some());
        assert!(props.get("warnings").is_some());
        assert!(props.get("galleryPath").is_some());

        // F13: new fields for find variants.
        assert!(props.get("outcome").is_some(), "missing outcome");
        assert!(props.get("dryRun").is_some(), "missing dryRun");
        assert!(
            props.get("estimatedTokens").is_some(),
            "missing estimatedTokens"
        );
        assert!(
            props.get("projectedCostDollars").is_some(),
            "missing projectedCostDollars"
        );
        assert!(
            props.get("invalidIdsDropped").is_some(),
            "missing invalidIdsDropped"
        );
    }
}
