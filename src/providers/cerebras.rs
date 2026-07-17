use std::collections::BTreeMap;
use std::time::Instant;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{LensError, Provider};
use crate::providers::{
    HttpFailure, SharedSpend, USER_AGENT, http_agent, join_url, new_spend, run_with_retries,
};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Message {
    pub role: String,
    pub content: MessageContent,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImageUrl {
    pub url: String,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: MessageContent::Text(text.into()),
        }
    }

    pub fn user_with_image(text: &str, data_uri: String) -> Self {
        Self {
            role: "user".into(),
            content: MessageContent::Parts(vec![
                ContentPart::Text {
                    text: text.to_string(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrl { url: data_uri },
                },
            ]),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ChatOpts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponseFormat {
    #[serde(rename = "type")]
    response_type: &'static str,
    json_schema: StructuredOutput,
}

impl ResponseFormat {
    pub fn json_schema(name: impl Into<String>, schema: JsonSchema) -> Self {
        Self {
            response_type: "json_schema",
            json_schema: StructuredOutput {
                name: name.into(),
                strict: true,
                schema,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StructuredOutput {
    name: String,
    strict: bool,
    schema: JsonSchema,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JsonSchema {
    #[serde(rename = "type")]
    schema_type: JsonSchemaType,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    properties: BTreeMap<String, JsonSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    items: Option<Box<JsonSchema>>,
    #[serde(rename = "enum", skip_serializing_if = "Vec::is_empty")]
    enum_values: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    required: Vec<String>,
    #[serde(
        rename = "additionalProperties",
        skip_serializing_if = "Option::is_none"
    )]
    additional_properties: Option<bool>,
}

impl JsonSchema {
    pub fn object(
        properties: impl IntoIterator<Item = (impl Into<String>, JsonSchema)>,
        required: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            schema_type: JsonSchemaType::Object,
            properties: properties
                .into_iter()
                .map(|(name, schema)| (name.into(), schema))
                .collect(),
            required: required.into_iter().map(Into::into).collect(),
            additional_properties: Some(false),
            ..Self::bare(JsonSchemaType::Object)
        }
    }

    pub fn string() -> Self {
        Self::bare(JsonSchemaType::String)
    }

    pub fn integer() -> Self {
        Self::bare(JsonSchemaType::Integer)
    }

    pub fn array(items: JsonSchema) -> Self {
        Self {
            items: Some(Box::new(items)),
            ..Self::bare(JsonSchemaType::Array)
        }
    }

    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn one_of(mut self, values: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.enum_values = values.into_iter().map(Into::into).collect();
        self
    }

    fn bare(schema_type: JsonSchemaType) -> Self {
        Self {
            schema_type,
            description: None,
            properties: BTreeMap::new(),
            items: None,
            enum_values: Vec::new(),
            required: Vec::new(),
            additional_properties: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum JsonSchemaType {
    Object,
    String,
    Array,
    Integer,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatResponse {
    pub content: String,
    pub usage: TokenUsage,
    pub wall_time_ms: u64,
}

pub struct CerebrasClient {
    api_key: String,
    base_url: String,
    model: String,
    agent: ureq::Agent,
    spend: SharedSpend,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
    #[serde(flatten)]
    opts: ChatOpts,
}

impl CerebrasClient {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            api_key,
            base_url,
            model,
            agent: http_agent(),
            spend: new_spend(),
        }
    }

    pub fn with_spend(mut self, spend: SharedSpend) -> Self {
        self.spend = spend;
        self
    }

    pub(crate) fn spend_snapshot(&self) -> crate::providers::Spend {
        self.spend.lock().unwrap().clone()
    }

    pub fn chat(&self, messages: &[Message], opts: ChatOpts) -> Result<ChatResponse, LensError> {
        let url = join_url(&self.base_url, "/chat/completions");
        let body = self.request_body(messages, opts);
        let start = Instant::now();
        let (raw, retries) = run_with_retries(
            Provider::Cerebras,
            || self.post_json(&url, &body),
            &std::thread::sleep,
        )?;
        let mut response = parse_chat_response(&raw)?;
        response.wall_time_ms = start.elapsed().as_millis() as u64;
        self.record_spend(&response.usage, retries);
        Ok(response)
    }

    fn request_body<'a>(&'a self, messages: &'a [Message], opts: ChatOpts) -> ChatRequest<'a> {
        ChatRequest {
            model: &self.model,
            messages,
            stream: false,
            opts,
        }
    }

    fn post_json(&self, url: &str, body: &impl Serialize) -> Result<String, HttpFailure> {
        let mut response = self
            .agent
            .post(url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("User-Agent", USER_AGENT)
            .send_json(body)
            .map_err(HttpFailure::from)?;

        response
            .body_mut()
            .read_to_string()
            .map_err(|err| HttpFailure::Transport(err.to_string()))
    }

    fn record_spend(&self, usage: &TokenUsage, retries: u32) {
        let dollars = model_cost_dollars(&self.model, usage);
        let mut spend = self.spend.lock().unwrap();

        spend.prompt_tokens += usage.prompt_tokens;
        spend.completion_tokens += usage.completion_tokens;
        spend.dollars += dollars;
        spend.call_count += 1;
        spend.retries += retries as u64;
    }
}

/// Per-model price table: `(input $/1M tokens, output $/1M tokens)`.
///
/// Unknown models fall back to the gemma pricing so the meter errs on the high
/// side rather than silently zeroing cost (which would corrupt `--max-dollars`).
/// Cerebras bills image tokens as prompt tokens, so there is no separate image
/// rate in lens.
fn model_price_dollars_per_mtok(model: &str) -> (f64, f64) {
    match model {
        "gpt-oss-120b" => (0.35, 0.75),
        "gemma-4-31b" | "zai-glm-4.7" => (2.15, 2.70),
        _ => (2.15, 2.70),
    }
}

fn model_cost_dollars(model: &str, usage: &TokenUsage) -> f64 {
    let (in_per_mtok, out_per_mtok) = model_price_dollars_per_mtok(model);
    (usage.prompt_tokens as f64 / 1_000_000.0 * in_per_mtok)
        + (usage.completion_tokens as f64 / 1_000_000.0 * out_per_mtok)
}

/// Apply to model-emitted JSON text (structured output content), never to the
/// HTTP envelope. Strips bad control chars and escapes bare `\u` sequences so
/// model-mangled JSON becomes parseable. The envelope is valid JSON from the
/// server; running this on it would corrupt legitimate escaped content
/// (e.g. `"C:\\users\\x"` arriving server-escaped).
pub fn json_repair(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if is_stripped_control(ch) {
            continue;
        }

        if ch == '\\' && chars.peek() == Some(&'u') {
            chars.next();
            let valid_escape = chars
                .clone()
                .take(4)
                .collect::<Vec<_>>()
                .as_slice()
                .try_into()
                .is_ok_and(|next: [char; 4]| next.iter().all(char::is_ascii_hexdigit));

            out.push('\\');
            if !valid_escape {
                out.push('\\');
            }
            out.push('u');
        } else {
            out.push(ch);
        }
    }

    out
}

pub(crate) fn parse_repaired_json<T: DeserializeOwned>(s: &str) -> serde_json::Result<T> {
    serde_json::from_str(&json_repair(s))
}

fn is_stripped_control(ch: char) -> bool {
    matches!(ch as u32, 0x00..=0x08 | 0x0B | 0x0C | 0x0E..=0x1F)
}

fn parse_chat_response(raw: &str) -> Result<ChatResponse, LensError> {
    let response: RawChatResponse = serde_json::from_str(raw).map_err(|err| {
        LensError::upstream(format!("failed to parse Cerebras response JSON: {err}"))
            .with_provider(Provider::Cerebras)
            .with_retryable(false)
    })?;

    let message = &response
        .choices
        .first()
        .ok_or_else(|| {
            LensError::upstream("Cerebras response contained no choices")
                .with_provider(Provider::Cerebras)
        })?
        .message;
    let content = message.content.clone().ok_or_else(|| {
        LensError::upstream("Cerebras response contained no content")
            .with_provider(Provider::Cerebras)
    })?;

    Ok(ChatResponse {
        content,
        usage: response.usage,
        wall_time_ms: 0,
    })
}

#[derive(Debug, Deserialize)]
struct RawChatResponse {
    choices: Vec<RawChoice>,
    usage: TokenUsage,
}

#[derive(Debug, Deserialize)]
struct RawChoice {
    message: RawMessage,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Spend;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[derive(Deserialize)]
    struct NamePayload {
        name: String,
    }

    #[derive(Deserialize)]
    struct ClaimPayload {
        claim: String,
    }

    #[test]
    fn json_repair_strips_bad_control_chars_but_keeps_accented_text() {
        let repaired = json_repair("{\"name\":\"Prós\u{0001}pera\"}");
        let parsed: NamePayload = serde_json::from_str(&repaired).unwrap();

        assert_eq!(parsed.name, "Próspera");
    }

    #[test]
    fn json_repair_escapes_bad_unicode_escape_but_keeps_valid_accent_escape() {
        let repaired = json_repair(r#"{"name":"Próspera \u broken \u00F3"}"#);
        let parsed: NamePayload = serde_json::from_str(&repaired).unwrap();

        assert_eq!(parsed.name, "Próspera \\u broken ó");
    }

    #[test]
    fn parses_content_and_usage_verbatim() {
        // Repairing the already-valid HTTP envelope would corrupt escaped content.
        let raw = r#"{
            "choices": [{
                "message": {
                    "content": "Próspera nope"
                }
            }],
            "usage": {"prompt_tokens": 1000, "completion_tokens": 2000}
        }"#;

        let parsed = parse_chat_response(raw).unwrap();

        assert_eq!(parsed.content, "Próspera nope");
        assert_eq!(parsed.usage.prompt_tokens, 1000);
        assert_eq!(parsed.usage.completion_tokens, 2000);
    }

    #[test]
    fn rejects_incomplete_response_envelopes() {
        for raw in [
            r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
            r#"{"choices":[{"message":{}}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
            r#"{"choices":[{"message":{"content":"ok"}}],"usage":{"prompt_tokens":1}}"#,
        ] {
            assert!(parse_chat_response(raw).is_err());
        }
    }

    #[test]
    fn json_repair_on_model_content_yields_parseable_json() {
        // Repair is limited to untrusted model content; HTTP envelopes are valid JSON.
        let repaired = json_repair("{\"claim\":\"Café\u{0001} \\u broken \\u00E9\"}");
        let parsed: ClaimPayload = serde_json::from_str(&repaired).unwrap();

        assert_eq!(parsed.claim, "Café \\u broken é");
    }

    #[test]
    fn message_serializes_text_and_image_shapes() {
        let user = serde_json::to_value(Message::user("hi")).unwrap();
        let user_obj = user.as_object().unwrap();
        assert_eq!(user_obj.len(), 2);
        assert_eq!(user["role"], "user");
        assert_eq!(user["content"], "hi");

        let image = serde_json::to_value(Message::user_with_image(
            "describe this",
            "data:image/jpeg;base64,abc123".into(),
        ))
        .unwrap();
        assert_eq!(
            image,
            json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "describe this"},
                    {
                        "type": "image_url",
                        "image_url": {"url": "data:image/jpeg;base64,abc123"}
                    }
                ]
            })
        );
    }

    #[test]
    fn records_model_spend_into_shared_meter() {
        let spend = Arc::new(Mutex::new(Spend::default()));
        let client = CerebrasClient::new(
            "key".into(),
            "http://localhost".into(),
            "gemma-4-31b".into(),
        )
        .with_spend(Arc::clone(&spend));

        client.record_spend(
            &TokenUsage {
                prompt_tokens: 1_000_000,
                completion_tokens: 1_000_000,
            },
            2,
        );

        let spend = spend.lock().unwrap();
        assert_eq!(spend.prompt_tokens, 1_000_000);
        assert_eq!(spend.completion_tokens, 1_000_000);
        assert!((spend.dollars - 4.85).abs() < f64::EPSILON);
        assert_eq!(spend.call_count, 1);
        assert_eq!(spend.retries, 2);
    }

    #[test]
    fn non_gemma_model_uses_correct_pricing() {
        let spend = Arc::new(Mutex::new(Spend::default()));
        let client = CerebrasClient::new(
            "key".into(),
            "http://localhost".into(),
            "gpt-oss-120b".into(),
        )
        .with_spend(Arc::clone(&spend));

        client.record_spend(
            &TokenUsage {
                prompt_tokens: 1_000_000,
                completion_tokens: 1_000_000,
            },
            0,
        );

        let spend = spend.lock().unwrap();
        assert!((spend.dollars - 1.10).abs() < f64::EPSILON);
    }
}
