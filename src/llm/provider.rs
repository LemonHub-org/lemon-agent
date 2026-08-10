//! LLM provider abstraction: normalized requests/responses are converted to
//! and from each provider's wire format.
//!
//! Built-in providers: `openai` (OpenAI-compatible chat completions, the
//! default), `anthropic` (Messages API), and `gemini` (GenerateContent).
//! `custom` wraps any OpenAI-compatible endpoint with configurable headers
//! and path, for self-hosted gateways and proxies.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Value, json};

use crate::config::{CustomLlmConfig, LlmConfig};
use crate::error::{Error, Result};
use crate::kernel::event_store::ToolCall;
use crate::llm::{LLMResponse, Message, Role, ToolDefinition, Usage};

/// A streaming delta forwarded to the caller.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamDelta {
    /// A content fragment to deliver immediately.
    Content(String),
    /// The provider signalled the end of the stream.
    Done,
}

/// Provider-specific stream accumulator. One instance per request.
pub trait StreamParser: Send {
    /// Feed one SSE `data:` payload (raw text, which may be `[DONE]`).
    fn on_event(&mut self, data: &str) -> Result<Option<StreamDelta>>;
    /// Produce the final parsed response once the stream ended.
    fn finish(&self) -> Result<LLMResponse>;
}

/// A chat-completions provider.
pub trait LlmProvider: fmt::Debug + Send + Sync {
    /// The provider name used in configuration.
    fn name(&self) -> &str;
    /// The URL path relative to `base_url` (may depend on model/streaming).
    fn chat_path(&self, model: &str, stream: bool) -> String;
    /// Headers carrying the API key (skipped when the key is empty).
    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)>;
    /// Additional static headers.
    fn extra_headers(&self) -> Vec<(String, String)>;
    /// Serialize the request body for a chat completion.
    fn build_body(
        &self,
        model: &str,
        max_output_tokens: u64,
        messages: &[Message],
        tools: &[ToolDefinition],
        temperature: f64,
        stream: bool,
    ) -> Result<Value>;
    /// Parse a non-streaming response body.
    fn parse_completion(&self, body: &Value, model: &str) -> Result<LLMResponse>;
    /// Create a fresh stream parser.
    fn stream_parser(&self) -> Box<dyn StreamParser>;
}

/// Build the provider selected by the configuration.
pub fn provider_from_config(config: &LlmConfig) -> Result<Box<dyn LlmProvider>> {
    match config.provider.as_str() {
        "openai" => Ok(Box::new(OpenAiProvider)),
        "anthropic" => Ok(Box::new(AnthropicProvider)),
        "gemini" => Ok(Box::new(GeminiProvider)),
        "custom" => Ok(Box::new(CustomProvider {
            chat_path: config.custom.chat_path.clone(),
            api_key_header: config.custom.api_key_header.clone(),
            api_key_scheme: config.custom.api_key_scheme.clone(),
            headers: config.custom.headers.clone(),
        })),
        other => Err(Error::InvalidConfig(format!(
            "unknown llm.provider {other:?}; expected openai|anthropic|gemini|custom"
        ))),
    }
}

// ----------------------------------------------------------------------
// OpenAI-compatible
// ----------------------------------------------------------------------

/// OpenAI chat completions (also covers DeepSeek, Ollama, vLLM, and any
/// OpenAI-compatible gateway via `base_url`).
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiProvider;

impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn chat_path(&self, _model: &str, _stream: bool) -> String {
        "/chat/completions".to_string()
    }

    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)> {
        if api_key.is_empty() {
            Vec::new()
        } else {
            vec![("authorization".to_string(), format!("Bearer {api_key}"))]
        }
    }

    fn extra_headers(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn build_body(
        &self,
        model: &str,
        _max_output_tokens: u64,
        messages: &[Message],
        tools: &[ToolDefinition],
        temperature: f64,
        stream: bool,
    ) -> Result<Value> {
        let wire_messages: Vec<Value> = messages.iter().map(openai_message).collect();
        let wire_tools = openai_tools_schema(tools);
        Ok(json!({
            "model": model,
            "messages": wire_messages,
            "tools": if wire_tools.is_empty() { Value::Null } else { Value::Array(wire_tools) },
            "temperature": temperature,
            "stream": stream,
        }))
    }

    fn parse_completion(&self, body: &Value, model: &str) -> Result<LLMResponse> {
        parse_openai_completion(body, model)
    }

    fn stream_parser(&self) -> Box<dyn StreamParser> {
        Box::new(OpenAiStreamParser::default())
    }
}

fn openai_message(m: &Message) -> Value {
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
    let mut message = json!({ "role": role, "content": content });
    if let Some(id) = &m.tool_call_id {
        message["tool_call_id"] = Value::String(id.clone());
    }
    if let Some(calls) = &m.tool_calls {
        message["tool_calls"] = Value::Array(
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
                .collect(),
        );
    }
    message
}

/// Convert tool definitions into the OpenAI Function Calling schema.
pub fn openai_tools_schema(tools: &[ToolDefinition]) -> Vec<Value> {
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

fn parse_openai_completion(value: &Value, model: &str) -> Result<LLMResponse> {
    let choices = value
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| llm_error("missing choices"))?;
    let message = choices
        .first()
        .and_then(|c| c.get("message"))
        .ok_or_else(|| llm_error("missing message"))?;

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
                .ok_or_else(|| llm_error("malformed tool call: missing arguments"))?;
            let arguments: Value = serde_json::from_str(arguments).map_err(|_| {
                llm_error(&format!(
                    "malformed tool call arguments for {name}: {arguments}"
                ))
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
            .unwrap_or(model)
            .to_string(),
        usage,
    })
}

#[derive(Debug, Default)]
struct OpenAiStreamParser {
    content: String,
    tool_calls: Vec<ToolCall>,
    model: String,
    usage: Usage,
}

impl StreamParser for OpenAiStreamParser {
    fn on_event(&mut self, data: &str) -> Result<Option<StreamDelta>> {
        if data == "[DONE]" {
            return Ok(Some(StreamDelta::Done));
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|e| llm_error(&format!("malformed stream chunk: {e}")))?;
        let before = self.content.len();
        apply_openai_chunk(
            &value,
            &mut self.content,
            &mut self.tool_calls,
            &mut self.model,
            &mut self.usage,
        );
        let delta = &self.content[before..];
        if delta.is_empty() {
            Ok(None)
        } else {
            Ok(Some(StreamDelta::Content(delta.to_string())))
        }
    }

    fn finish(&self) -> Result<LLMResponse> {
        if self.content.is_empty() && self.tool_calls.is_empty() {
            return Err(stream_empty_error());
        }
        let mut tool_calls = self.tool_calls.clone();
        for call in &mut tool_calls {
            if let Value::String(args) = &call.arguments {
                call.arguments = serde_json::from_str(args).map_err(|_| {
                    llm_error(&format!(
                        "malformed streamed tool call arguments for {}: {args}",
                        call.name
                    ))
                })?;
            }
        }
        Ok(LLMResponse {
            content: self.content.clone(),
            tool_calls,
            model: self.model.clone(),
            usage: self.usage,
        })
    }
}

fn apply_openai_chunk(
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

// ----------------------------------------------------------------------
// Anthropic Messages API
// ----------------------------------------------------------------------

/// Anthropic Messages API (`POST /v1/messages`).
#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicProvider;

impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn chat_path(&self, _model: &str, _stream: bool) -> String {
        "/v1/messages".to_string()
    }

    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)> {
        if api_key.is_empty() {
            Vec::new()
        } else {
            vec![
                ("x-api-key".to_string(), api_key.to_string()),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ]
        }
    }

    fn extra_headers(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn build_body(
        &self,
        model: &str,
        max_output_tokens: u64,
        messages: &[Message],
        tools: &[ToolDefinition],
        temperature: f64,
        stream: bool,
    ) -> Result<Value> {
        let mut system = String::new();
        let mut wire_messages: Vec<Value> = Vec::new();
        for message in messages {
            match message.role {
                Role::System => {
                    if !system.is_empty() {
                        system.push('\n');
                    }
                    system.push_str(&message.content);
                }
                Role::User => {
                    let mut blocks = Vec::new();
                    if !message.content.is_empty() {
                        blocks.push(json!({"type": "text", "text": message.content}));
                    }
                    wire_messages.push(json!({"role": "user", "content": blocks}));
                }
                Role::Assistant => {
                    let mut blocks = Vec::new();
                    if !message.content.is_empty() {
                        blocks.push(json!({"type": "text", "text": message.content}));
                    }
                    if let Some(calls) = &message.tool_calls {
                        for call in calls {
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": call.id,
                                "name": call.name,
                                "input": call.arguments,
                            }));
                        }
                    }
                    wire_messages.push(json!({"role": "assistant", "content": blocks}));
                }
                Role::Tool => {
                    // Anthropic nests tool results inside a user message.
                    wire_messages.push(json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
                            "content": message.content,
                        }]
                    }));
                }
            }
        }
        let wire_tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
        Ok(json!({
            "model": model,
            "max_tokens": max_output_tokens,
            "system": if system.is_empty() { Value::Null } else { Value::String(system) },
            "messages": wire_messages,
            "tools": if wire_tools.is_empty() { Value::Null } else { Value::Array(wire_tools) },
            "temperature": temperature,
            "stream": stream,
        }))
    }

    fn parse_completion(&self, body: &Value, model: &str) -> Result<LLMResponse> {
        let content_blocks = body
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| llm_error("missing content"))?;
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        for block in content_blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        content.push_str(text);
                    }
                }
                Some("tool_use") => {
                    tool_calls.push(ToolCall {
                        id: block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        arguments: block.get("input").cloned().unwrap_or(json!({})),
                    });
                }
                _ => {}
            }
        }
        let usage = body.get("usage").map(|u| Usage {
            prompt_tokens: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
            completion_tokens: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
            total_tokens: 0,
        });
        Ok(LLMResponse {
            content,
            tool_calls,
            model: body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(model)
                .to_string(),
            usage: usage.unwrap_or_default(),
        })
    }

    fn stream_parser(&self) -> Box<dyn StreamParser> {
        Box::new(AnthropicStreamParser::default())
    }
}

#[derive(Debug, Default)]
struct AnthropicStreamParser {
    content: String,
    tool_calls: Vec<ToolCall>,
    tool_args: Vec<String>,
    model: String,
    usage: Usage,
    done: bool,
}

impl StreamParser for AnthropicStreamParser {
    fn on_event(&mut self, data: &str) -> Result<Option<StreamDelta>> {
        let value: Value = serde_json::from_str(data)
            .map_err(|e| llm_error(&format!("malformed anthropic stream chunk: {e}")))?;
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match event_type {
            "message_start" => {
                if let Some(message) = value.get("message") {
                    if let Some(m) = message.get("model").and_then(Value::as_str) {
                        self.model = m.to_string();
                    }
                    if let Some(u) = message.get("usage") {
                        self.usage.prompt_tokens =
                            u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                    }
                }
                Ok(None)
            }
            "content_block_start" => {
                if let Some(block) = value.get("content_block")
                    && block.get("type").and_then(Value::as_str) == Some("tool_use")
                {
                    let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    while self.tool_calls.len() <= index {
                        self.tool_calls.push(ToolCall {
                            id: String::new(),
                            name: String::new(),
                            arguments: Value::Null,
                        });
                        self.tool_args.push(String::new());
                    }
                    self.tool_calls[index].id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    self.tool_calls[index].name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                }
                Ok(None)
            }
            "content_block_delta" => {
                let delta = value.get("delta");
                match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => {
                        let text = delta
                            .and_then(|d| d.get("text"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        self.content.push_str(text);
                        Ok(Some(StreamDelta::Content(text.to_string())))
                    }
                    Some("input_json_delta") => {
                        let partial = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let index =
                            value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        if let Some(acc) = self.tool_args.get_mut(index) {
                            acc.push_str(partial);
                        }
                        Ok(None)
                    }
                    _ => Ok(None),
                }
            }
            "message_delta" => {
                if let Some(u) = value.get("usage") {
                    self.usage.completion_tokens =
                        u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                }
                Ok(None)
            }
            "message_stop" | "error" => {
                self.done = true;
                Ok(Some(StreamDelta::Done))
            }
            _ => Ok(None),
        }
    }

    fn finish(&self) -> Result<LLMResponse> {
        if self.content.is_empty() && self.tool_calls.is_empty() {
            return Err(stream_empty_error());
        }
        let mut tool_calls = self.tool_calls.clone();
        for (call, args) in tool_calls.iter_mut().zip(self.tool_args.iter()) {
            if !args.is_empty() {
                call.arguments = serde_json::from_str(args).map_err(|_| {
                    llm_error(&format!(
                        "malformed streamed tool call arguments for {}: {args}",
                        call.name
                    ))
                })?;
            }
        }
        Ok(LLMResponse {
            content: self.content.clone(),
            tool_calls,
            model: self.model.clone(),
            usage: self.usage,
        })
    }
}

// ----------------------------------------------------------------------
// Google Gemini GenerateContent
// ----------------------------------------------------------------------

/// Google Gemini (`/v1beta/models/{model}:generateContent`).
#[derive(Debug, Clone, Copy, Default)]
pub struct GeminiProvider;

impl LlmProvider for GeminiProvider {
    fn name(&self) -> &str {
        "gemini"
    }

    fn chat_path(&self, model: &str, stream: bool) -> String {
        if stream {
            format!("/v1beta/models/{model}:streamGenerateContent?alt=sse")
        } else {
            format!("/v1beta/models/{model}:generateContent")
        }
    }

    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)> {
        if api_key.is_empty() {
            Vec::new()
        } else {
            vec![("x-goog-api-key".to_string(), api_key.to_string())]
        }
    }

    fn extra_headers(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn build_body(
        &self,
        _model: &str,
        _max_output_tokens: u64,
        messages: &[Message],
        tools: &[ToolDefinition],
        temperature: f64,
        _stream: bool,
    ) -> Result<Value> {
        let mut system = String::new();
        let mut contents: Vec<Value> = Vec::new();
        for message in messages {
            match message.role {
                Role::System => {
                    if !system.is_empty() {
                        system.push('\n');
                    }
                    system.push_str(&message.content);
                }
                Role::User => {
                    contents.push(json!({
                        "role": "user",
                        "parts": [{"text": message.content}],
                    }));
                }
                Role::Assistant => {
                    let mut parts = Vec::new();
                    if !message.content.is_empty() {
                        parts.push(json!({"text": message.content}));
                    }
                    if let Some(calls) = &message.tool_calls {
                        for call in &calls.clone() {
                            parts.push(json!({
                                "functionCall": {"name": call.name, "args": call.arguments}
                            }));
                        }
                    }
                    contents.push(json!({"role": "model", "parts": parts}));
                }
                Role::Tool => {
                    contents.push(json!({
                        "role": "function",
                        "parts": [{
                            "functionResponse": {
                                "name": message.tool_call_id.clone().unwrap_or_default(),
                                "response": {"content": message.content},
                            }
                        }],
                    }));
                }
            }
        }
        let wire_tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "functionDeclarations": [{
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }]
                })
            })
            .collect();
        let mut body = json!({
            "contents": contents,
            "generationConfig": {"temperature": temperature},
        });
        if !system.is_empty() {
            body["systemInstruction"] = json!({"parts": [{"text": system}]});
        }
        if !wire_tools.is_empty() {
            body["tools"] = Value::Array(wire_tools);
        }
        Ok(body)
    }

    fn parse_completion(&self, body: &Value, model: &str) -> Result<LLMResponse> {
        let parts = body
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
            .ok_or_else(|| llm_error("missing candidates[0].content.parts"))?;
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        for part in parts {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                content.push_str(text);
            }
            if let Some(call) = part.get("functionCall") {
                tool_calls.push(ToolCall {
                    id: String::new(),
                    name: call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    arguments: call.get("args").cloned().unwrap_or(json!({})),
                });
            }
        }
        let usage = body.get("usageMetadata").map(|u| Usage {
            prompt_tokens: u
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            completion_tokens: u
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            total_tokens: u
                .get("totalTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        });
        Ok(LLMResponse {
            content,
            tool_calls,
            model: model.to_string(),
            usage: usage.unwrap_or_default(),
        })
    }

    fn stream_parser(&self) -> Box<dyn StreamParser> {
        Box::new(GeminiStreamParser::default())
    }
}

#[derive(Debug, Default)]
struct GeminiStreamParser {
    content: String,
    tool_calls: Vec<ToolCall>,
    usage: Usage,
    done: bool,
}

impl StreamParser for GeminiStreamParser {
    fn on_event(&mut self, data: &str) -> Result<Option<StreamDelta>> {
        if data == "[DONE]" || self.done {
            return Ok(Some(StreamDelta::Done));
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|e| llm_error(&format!("malformed gemini stream chunk: {e}")))?;
        if let Some(u) = value.get("usageMetadata") {
            self.usage.prompt_tokens = u
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.usage.completion_tokens = u
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.usage.total_tokens = u
                .get("totalTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
        }
        let Some(candidate) = value
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return Ok(None);
        };
        let before = self.content.len();
        if let Some(parts) = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    self.content.push_str(text);
                }
                if let Some(call) = part.get("functionCall") {
                    self.tool_calls.push(ToolCall {
                        id: String::new(),
                        name: call
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        arguments: call.get("args").cloned().unwrap_or(json!({})),
                    });
                }
            }
        }
        let delta = &self.content[before..];
        let finished = candidate
            .get("finishReason")
            .and_then(Value::as_str)
            .is_some_and(|r| !r.is_empty() && r != "STOP_UNSPECIFIED");
        if finished {
            self.done = true;
            if delta.is_empty() {
                Ok(Some(StreamDelta::Done))
            } else {
                Ok(Some(StreamDelta::Content(delta.to_string())))
            }
        } else if delta.is_empty() {
            Ok(None)
        } else {
            Ok(Some(StreamDelta::Content(delta.to_string())))
        }
    }

    fn finish(&self) -> Result<LLMResponse> {
        if self.content.is_empty() && self.tool_calls.is_empty() {
            return Err(stream_empty_error());
        }
        Ok(LLMResponse {
            content: self.content.clone(),
            tool_calls: self.tool_calls.clone(),
            model: String::new(),
            usage: self.usage,
        })
    }
}

// ----------------------------------------------------------------------
// Custom OpenAI-compatible endpoint
// ----------------------------------------------------------------------

/// Any OpenAI-compatible endpoint with configurable path and auth headers,
/// for self-hosted gateways, proxies, and LLM aggregators.
#[derive(Debug, Clone)]
pub struct CustomProvider {
    chat_path: String,
    api_key_header: String,
    api_key_scheme: String,
    headers: BTreeMap<String, String>,
}

impl LlmProvider for CustomProvider {
    fn name(&self) -> &str {
        "custom"
    }

    fn chat_path(&self, _model: &str, _stream: bool) -> String {
        self.chat_path.clone()
    }

    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)> {
        if api_key.is_empty() {
            Vec::new()
        } else {
            vec![(
                self.api_key_header.clone(),
                format!("{}{}", self.api_key_scheme, api_key),
            )]
        }
    }

    fn extra_headers(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    fn build_body(
        &self,
        model: &str,
        max_output_tokens: u64,
        messages: &[Message],
        tools: &[ToolDefinition],
        temperature: f64,
        stream: bool,
    ) -> Result<Value> {
        OpenAiProvider.build_body(
            model,
            max_output_tokens,
            messages,
            tools,
            temperature,
            stream,
        )
    }

    fn parse_completion(&self, body: &Value, model: &str) -> Result<LLMResponse> {
        parse_openai_completion(body, model)
    }

    fn stream_parser(&self) -> Box<dyn StreamParser> {
        Box::new(OpenAiStreamParser::default())
    }
}

/// Validate the custom provider definition.
pub fn validate_custom(config: &CustomLlmConfig) -> Result<()> {
    if !config.chat_path.starts_with('/') {
        return Err(Error::InvalidConfig(format!(
            "llm.custom.chat_path must start with '/', got {:?}",
            config.chat_path
        )));
    }
    if config.api_key_header.trim().is_empty() {
        return Err(Error::InvalidConfig(
            "llm.custom.api_key_header must not be empty".to_string(),
        ));
    }
    for name in config.headers.keys() {
        if name.trim().is_empty() {
            return Err(Error::InvalidConfig(
                "llm.custom.headers contains an empty header name".to_string(),
            ));
        }
    }
    Ok(())
}

/// A retryable "stream ended empty" error: transient provider hiccups are
/// retried rather than failing the call.
fn stream_empty_error() -> Error {
    Error::Llm {
        message: "stream ended without content or tool calls".to_string(),
        retryable: true,
    }
}

fn llm_error(message: &str) -> Error {
    Error::Llm {
        message: message.to_string(),
        retryable: false,
    }
}
