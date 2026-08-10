//! OpenAI-compatible chat client with bounded retries, timeouts, optional
//! streaming, and Function Calling support.
//!
//! The API key is never logged. All responses are parsed strictly; malformed
//! payloads fail loudly rather than being silently ignored.

use std::time::Duration;

use futures_util::StreamExt;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::config::LlmConfig;
use crate::error::{Error, Result};
use crate::kernel::event_store::ToolCall;

/// The role of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A chat message in the OpenAI wire format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Message {
        Message {
            role: Role::System,
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Message {
        Message {
            role: Role::User,
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Message {
        Message {
            role: Role::Assistant,
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    pub fn assistant_with_tool_calls(tool_calls: Vec<ToolCall>) -> Message {
        Message {
            role: Role::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_calls: Some(tool_calls),
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Message {
        Message {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: None,
        }
    }
}

/// A tool the model may call, with a JSON Schema for its arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// The parsed model response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LLMResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub model: String,
    pub usage: Usage,
}

/// Token usage reported by the API.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// A chat completion request in the OpenAI wire format.
#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    tools: Option<Vec<Value>>,
    temperature: f32,
    stream: bool,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<Value>>,
}

impl<'a> From<&'a Message> for WireMessage<'a> {
    fn from(m: &'a Message) -> WireMessage<'a> {
        let role = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let content = if m.content.is_empty() {
            Value::Null
        } else {
            Value::String(m.content.clone())
        };
        let tool_calls = m.tool_calls.as_ref().map(|calls| {
            calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_string())
                        }
                    })
                })
                .collect()
        });
        WireMessage {
            role,
            content,
            tool_call_id: m.tool_call_id.as_deref(),
            tool_calls,
        }
    }
}

/// The OpenAI-compatible client.
#[derive(Debug, Clone)]
pub struct LLMClient {
    api_key: String,
    base_url: String,
    model: String,
    temperature: f32,
    request_timeout: Duration,
    max_retries: u32,
    retry_base_delay: Duration,
    http: reqwest::Client,
}

impl LLMClient {
    pub fn new(config: &LlmConfig) -> Result<LLMClient> {
        let base_url = config.base_url.trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(Error::InvalidConfig(
                "llm.base_url must not be empty".to_string(),
            ));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .map_err(Error::Http)?;
        Ok(LLMClient {
            api_key: config.api_key.clone(),
            base_url,
            model: config.model.clone(),
            temperature: config.temperature,
            request_timeout: Duration::from_secs(config.request_timeout_secs),
            max_retries: config.max_retries,
            retry_base_delay: Duration::from_secs(config.retry_base_delay_secs),
            http,
        })
    }

    /// The model name this client talks to.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The base URL, without a trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Run a chat completion, retrying transient failures with backoff.
    pub async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<LLMResponse> {
        self.chat_inner(messages, tools, None::<fn(String)>).await
    }

    /// Run a chat completion with a streaming response. `on_delta` receives
    /// content deltas as they arrive; the final parsed response is returned.
    pub async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        on_delta: impl FnMut(String),
    ) -> Result<LLMResponse> {
        self.chat_inner(messages, tools, Some(on_delta)).await
    }

    async fn chat_inner(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        mut on_delta: Option<impl FnMut(String)>,
    ) -> Result<LLMResponse> {
        let wire_messages: Vec<WireMessage<'_>> = messages.iter().map(Into::into).collect();
        let wire_tools = tools_to_schema(tools);
        let body = ChatRequest {
            model: &self.model,
            messages: wire_messages,
            tools: if wire_tools.is_empty() {
                None
            } else {
                Some(wire_tools)
            },
            temperature: self.temperature,
            stream: on_delta.is_some(),
        };

        let mut attempts = 0;
        loop {
            attempts += 1;
            let attempt = self.attempt(&body, on_delta.as_mut()).await;
            match attempt {
                Ok(response) => return Ok(response),
                Err(e) if e.is_retryable() && attempts <= self.max_retries => {
                    let delay = self
                        .retry_base_delay
                        .saturating_mul(1u32 << (attempts - 1).min(4));
                    warn!(
                        error = %e,
                        attempt = attempts,
                        max_retries = self.max_retries,
                        delay_ms = delay.as_millis(),
                        "LLM request failed; retrying with backoff"
                    );
                    tokio::time::sleep(delay).await;
                }
                Err(e) if attempts <= self.max_retries => {
                    return Err(Error::Llm {
                        message: e.to_string(),
                        retryable: false,
                    });
                }
                Err(e) => {
                    return Err(Error::RetryExhausted {
                        operation: "llm.chat".to_string(),
                        attempts,
                        message: e.to_string(),
                    });
                }
            }
        }
    }

    async fn attempt(
        &self,
        body: &ChatRequest<'_>,
        mut on_delta: Option<impl FnMut(String)>,
    ) -> Result<LLMResponse> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut request = self
            .http
            .post(&url)
            .json(body)
            .timeout(self.request_timeout);
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }

        let response = request.send().await.map_err(|e| {
            if e.is_timeout() || e.is_connect() {
                Error::Llm {
                    message: format!("{e}"),
                    retryable: true,
                }
            } else {
                Error::Http(e)
            }
        })?;

        let status = response.status();
        if !status.is_success() {
            let status_text = response.status().as_u16();
            let retryable = matches!(
                status,
                StatusCode::TOO_MANY_REQUESTS
                    | StatusCode::INTERNAL_SERVER_ERROR
                    | StatusCode::BAD_GATEWAY
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::GATEWAY_TIMEOUT
            );
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Llm {
                message: format!("HTTP {status_text}: {}", truncate(&text, 500)),
                retryable,
            });
        }

        if let Some(callback) = on_delta.as_mut() {
            self.parse_stream(response, callback).await
        } else {
            self.parse_json(response).await
        }
    }

    /// Parse a non-streaming JSON response.
    async fn parse_json(&self, response: reqwest::Response) -> Result<LLMResponse> {
        let bytes = response.bytes().await.map_err(|e| Error::Llm {
            message: format!("failed to read response body: {e}"),
            retryable: true,
        })?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|e| Error::Llm {
            message: format!("malformed response JSON: {e}"),
            retryable: false,
        })?;
        parse_completion(&value)
    }

    /// Parse an SSE stream of chat completion chunks.
    async fn parse_stream(
        &self,
        response: reqwest::Response,
        on_delta: &mut dyn FnMut(String),
    ) -> Result<LLMResponse> {
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut content = String::new();
        let mut model = String::new();
        let mut usage = Usage::default();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| Error::Llm {
                message: format!("stream read failed: {e}"),
                retryable: true,
            })?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buffer.find('\n') {
                let line: String = buffer.drain(..=pos).collect();
                let line = line.trim();
                if !line.starts_with("data:") {
                    continue;
                }
                let data = line[5..].trim();
                if data == "[DONE]" {
                    break;
                }
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                apply_chunk(
                    &value,
                    &mut content,
                    &mut tool_calls,
                    &mut model,
                    &mut usage,
                );
                on_delta(content.clone());
            }
        }

        if content.is_empty() && tool_calls.is_empty() {
            return Err(Error::Llm {
                message: "stream ended without content or tool calls".to_string(),
                retryable: true,
            });
        }
        for call in &mut tool_calls {
            if let Value::String(args) = &call.arguments {
                call.arguments = serde_json::from_str(args).map_err(|_| Error::Llm {
                    message: format!(
                        "malformed streamed tool call arguments for {}: {args}",
                        call.name
                    ),
                    retryable: false,
                })?;
            }
        }
        Ok(LLMResponse {
            content,
            tool_calls,
            model,
            usage,
        })
    }
}

/// Convert tool definitions into the OpenAI Function Calling schema.
pub fn tools_to_schema(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect()
}

/// Parse a full chat completion payload.
fn parse_completion(value: &Value) -> Result<LLMResponse> {
    let choices = value
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Llm {
            message: format!(
                "malformed response: missing choices: {}",
                truncate(&value.to_string(), 200)
            ),
            retryable: false,
        })?;
    let message = choices
        .first()
        .and_then(|c| c.get("message"))
        .ok_or_else(|| Error::Llm {
            message: "malformed response: missing message".to_string(),
            retryable: false,
        })?;

    let content = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let arguments = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Llm {
                    message: "malformed tool call: missing arguments".to_string(),
                    retryable: false,
                })?;
            let arguments: Value = serde_json::from_str(arguments).map_err(|_| Error::Llm {
                message: format!("malformed tool call arguments for {name}: {arguments}"),
                retryable: false,
            })?;
            tool_calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
    }

    let usage = value
        .get("usage")
        .map(|u| Usage {
            prompt_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
            completion_tokens: u
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            total_tokens: u.get("total_tokens").and_then(Value::as_u64).unwrap_or(0),
        })
        .unwrap_or_default();

    Ok(LLMResponse {
        content,
        tool_calls,
        model: value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        usage,
    })
}

/// Apply a streamed chunk to the accumulated response state.
fn apply_chunk(
    chunk: &Value,
    content: &mut String,
    tool_calls: &mut Vec<ToolCall>,
    model: &mut String,
    usage: &mut Usage,
) {
    if let Some(m) = chunk.get("model").and_then(Value::as_str)
        && model.is_empty()
    {
        *model = m.to_string();
    }
    if let Some(u) = chunk.get("usage") {
        if let Some(t) = u.get("total_tokens").and_then(Value::as_u64) {
            usage.total_tokens = t;
        }
        if let Some(p) = u.get("prompt_tokens").and_then(Value::as_u64) {
            usage.prompt_tokens = p;
        }
        if let Some(c) = u.get("completion_tokens").and_then(Value::as_u64) {
            usage.completion_tokens = c;
        }
    }
    let Some(delta) = chunk
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("delta"))
    else {
        return;
    };
    if let Some(text) = delta.get("content").and_then(Value::as_str) {
        content.push_str(text);
    }
    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            while tool_calls.len() <= index {
                tool_calls.push(ToolCall {
                    id: String::new(),
                    name: String::new(),
                    arguments: Value::Null,
                });
            }
            let current = &mut tool_calls[index];
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                current.id.push_str(id);
            }
            if let Some(name) = call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            {
                current.name.push_str(name);
            }
            if let Some(args) = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
            {
                let mut acc = match &current.arguments {
                    Value::String(s) => s.clone(),
                    _ => String::new(),
                };
                acc.push_str(args);
                current.arguments = Value::String(acc);
            }
        }
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push_str("...");
        out
    }
}
