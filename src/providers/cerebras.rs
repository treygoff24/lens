use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{LensError, Provider};
use crate::providers::{
    HttpFailure, SharedSpend, SleepFn, USER_AGENT, default_sleep, http_agent, join_url, new_spend,
    run_with_retries,
};

/// A chat-completions message.
///
/// Serializes to the three shapes Cerebras (OpenAI-compat) accepts:
/// - user/system/assistant text or vision parts: `{"role","content"}`
/// - assistant tool_calls (NO `content` key, which Cerebras rejects alongside tool_calls)
/// - tool results: `{"role":"tool","tool_call_id","content"}`
///
/// Fields that don't apply to a given role are omitted from serialization so the
/// wire payload stays well-formed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<MessageToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: ToolType,
    pub function: MessageToolFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageToolFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolType {
    Function,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(MessageContent::Text(text.into())),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(MessageContent::Text(text.into())),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user_with_image(text: &str, data_uri: String) -> Self {
        Self {
            role: "user".into(),
            content: Some(MessageContent::Parts(vec![
                ContentPart::Text {
                    text: text.to_string(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrl { url: data_uri },
                },
            ])),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// Assistant message carrying tool calls and no `content` (Cerebras rejects
    /// `content` alongside `tool_calls`).
    pub fn assistant_tool_calls(tool_calls: Vec<MessageToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(MessageContent::Text(content.into())),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatOpts {
    pub temperature: Option<f64>,
    pub max_completion_tokens: Option<u64>,
    pub tools: Option<Value>,
    pub response_format: Option<ResponseFormat>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    JsonObject,
    JsonSchema { json_schema: StructuredOutput },
}

impl ResponseFormat {
    pub fn json_schema(name: impl Into<String>, schema: JsonSchema) -> Self {
        Self::JsonSchema {
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

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub function_name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
    pub wall_time_ms: u64,
}

pub struct CerebrasClient {
    api_key: String,
    base_url: String,
    model: String,
    agent: ureq::Agent,
    spend: SharedSpend,
    sleep_fn: SleepFn,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
}

impl CerebrasClient {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            api_key,
            base_url,
            model,
            agent: http_agent(),
            spend: new_spend(),
            sleep_fn: default_sleep(),
        }
    }

    pub fn with_spend(mut self, spend: SharedSpend) -> Self {
        self.spend = spend;
        self
    }

    pub fn with_sleep_fn(
        mut self,
        sleep_fn: impl Fn(std::time::Duration) + Send + Sync + 'static,
    ) -> Self {
        self.sleep_fn = Arc::new(sleep_fn);
        self
    }

    pub fn spend(&self) -> SharedSpend {
        Arc::clone(&self.spend)
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
            self.sleep_fn.as_ref(),
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
            temperature: opts.temperature,
            max_completion_tokens: opts.max_completion_tokens,
            tools: opts.tools,
            response_format: opts.response_format,
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
    let content = match &message.content {
        Some(content) => content.clone(),
        None if message
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty()) =>
        {
            String::new()
        }
        None => {
            return Err(LensError::upstream(
                "Cerebras response contained neither content nor tool calls",
            )
            .with_provider(Provider::Cerebras));
        }
    };

    Ok(ChatResponse {
        content,
        tool_calls: tool_calls(message),
        usage: TokenUsage {
            prompt_tokens: response.usage.prompt_tokens,
            completion_tokens: response.usage.completion_tokens,
        },
        wall_time_ms: 0,
    })
}

fn tool_calls(message: &RawMessage) -> Vec<ToolCall> {
    message
        .tool_calls
        .iter()
        .flatten()
        .map(|call| ToolCall {
            id: call.id.clone(),
            function_name: call.function.name.clone(),
            arguments: call.function.arguments.clone(),
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct RawChatResponse {
    choices: Vec<RawChoice>,
    usage: RawUsage,
}

#[derive(Debug, Deserialize)]
struct RawChoice {
    message: RawMessage,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    content: Option<String>,
    tool_calls: Option<Vec<RawToolCall>>,
}

#[derive(Debug, Deserialize)]
struct RawToolCall {
    id: String,
    function: RawFunction,
}

#[derive(Debug, Deserialize)]
struct RawFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct RawUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Spend;
    use serde_json::json;
    use std::sync::Mutex;

    #[test]
    fn json_repair_strips_bad_control_chars_but_keeps_accented_text() {
        let repaired = json_repair("{\"name\":\"Prós\u{0001}pera\"}");
        let parsed: Value = serde_json::from_str(&repaired).unwrap();

        assert_eq!(parsed["name"], "Próspera");
    }

    #[test]
    fn json_repair_escapes_bad_unicode_escape_but_keeps_valid_accent_escape() {
        let repaired = json_repair(r#"{"name":"Próspera \u broken \u00F3"}"#);
        let parsed: Value = serde_json::from_str(&repaired).unwrap();

        assert_eq!(parsed["name"], "Próspera \\u broken ó");
    }

    #[test]
    fn parses_content_tool_calls_and_usage_verbatim() {
        // Repairing the already-valid HTTP envelope would corrupt escaped content.
        let raw = r#"{
            "choices": [{
                "message": {
                    "content": "Próspera nope",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {"name": "search", "arguments": "{\"q\":\"x\"}"}
                    }]
                }
            }],
            "usage": {"prompt_tokens": 1000, "completion_tokens": 2000}
        }"#;

        let parsed = parse_chat_response(raw).unwrap();

        assert_eq!(parsed.content, "Próspera nope");
        assert_eq!(parsed.tool_calls[0].id, "call_1");
        assert_eq!(parsed.tool_calls[0].function_name, "search");
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
    fn accepts_tool_call_response_without_content() {
        let raw = r#"{
            "choices": [{"message": {"tool_calls": [{
                "id": "call_1",
                "function": {"name": "search", "arguments": "{}"}
            }]}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }"#;

        let parsed = parse_chat_response(raw).unwrap();

        assert!(parsed.content.is_empty());
        assert_eq!(parsed.tool_calls[0].id, "call_1");
    }

    #[test]
    fn json_repair_on_model_content_yields_parseable_json() {
        // Repair is limited to untrusted model content; HTTP envelopes are valid JSON.
        let repaired = json_repair("{\"claim\":\"Café\u{0001} \\u broken \\u00E9\"}");
        let parsed: Value = serde_json::from_str(&repaired).unwrap();

        assert_eq!(parsed["claim"], "Café \\u broken é");
    }

    #[test]
    fn message_serializes_each_role_shape_correctly() {
        let user = serde_json::to_value(Message::user("hi")).unwrap();
        let user_obj = user.as_object().unwrap();
        assert_eq!(user_obj.len(), 2);
        assert_eq!(user["role"], "user");
        assert_eq!(user["content"], "hi");
        assert!(user.get("tool_calls").is_none());
        assert!(user.get("tool_call_id").is_none());

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

        let assistant =
            serde_json::to_value(Message::assistant_tool_calls(vec![MessageToolCall {
                id: "call_1".into(),
                kind: ToolType::Function,
                function: MessageToolFunction {
                    name: "search".into(),
                    arguments: "{}".into(),
                },
            }]))
            .unwrap();
        let assistant_obj = assistant.as_object().unwrap();
        assert_eq!(assistant_obj.len(), 2);
        assert_eq!(assistant["role"], "assistant");
        assert!(assistant.get("content").is_none(), "content must be absent");
        assert!(assistant["tool_calls"].is_array());

        let tool = serde_json::to_value(Message::tool("call_1", "result body")).unwrap();
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "call_1");
        assert_eq!(tool["content"], "result body");
        assert!(tool.get("tool_calls").is_none());
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
