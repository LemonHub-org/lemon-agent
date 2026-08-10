//! LLM gateway: shared transport, retries, and timeouts over a pluggable
//! provider. Providers (OpenAI-compatible, Anthropic, Gemini, custom) live in
//! `provider.rs` and own their wire formats.
//!
//! The API key is never logged. All responses are parsed strictly; malformed
//! payloads fail loudly rather than being silently ignored.

use std::time::Duration;

use futures_util::StreamExt;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::LlmConfig;
use crate::error::{Error, Result};
use crate::llm::provider::{LlmProvider, StreamDelta, provider_from_config};

/// The role of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A chat message in the normalized (provider-agnostic) format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<crate::kernel::event_store::ToolCall>>,
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

    pub fn assistant_with_tool_calls(
        tool_calls: Vec<crate::kernel::event_store::ToolCall>,
    ) -> Message {
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
    pub parameters: serde_json::Value,
}

/// The parsed model response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LLMResponse {
    pub content: String,
    pub tool_calls: Vec<crate::kernel::event_store::ToolCall>,
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

/// The OpenAI-compatible client, provider-agnostic.
#[derive(Debug)]
pub struct LLMClient {
    api_key: String,
    base_url: String,
    model: String,
    temperature: f64,
    max_output_tokens: u64,
    request_timeout: Duration,
    max_retries: u32,
    retry_base_delay: Duration,
    http: reqwest::Client,
    provider: Box<dyn LlmProvider>,
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
            max_output_tokens: config.max_output_tokens,
            request_timeout: Duration::from_secs(config.request_timeout_secs),
            max_retries: config.max_retries,
            retry_base_delay: Duration::from_secs(config.retry_base_delay_secs),
            http,
            provider: provider_from_config(config)?,
        })
    }

    /// The provider name, e.g. "openai" or "anthropic".
    pub fn provider_name(&self) -> &str {
        self.provider.name()
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
        on_delta: impl FnMut(String) + Send,
    ) -> Result<LLMResponse> {
        self.chat_inner(messages, tools, Some(on_delta)).await
    }

    async fn chat_inner(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        mut on_delta: Option<impl FnMut(String) + Send>,
    ) -> Result<LLMResponse> {
        let body = self.provider.build_body(
            &self.model,
            self.max_output_tokens,
            messages,
            tools,
            self.temperature,
            on_delta.is_some(),
        )?;

        let mut attempts = 0;
        loop {
            attempts += 1;
            let attempt = self
                .attempt(
                    &body,
                    on_delta
                        .as_mut()
                        .map(|cb| cb as &mut (dyn FnMut(String) + Send)),
                )
                .await;
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
        body: &serde_json::Value,
        on_delta: Option<&mut (dyn FnMut(String) + Send)>,
    ) -> Result<LLMResponse> {
        let url = format!(
            "{}{}",
            self.base_url,
            self.provider.chat_path(&self.model, on_delta.is_some())
        );
        let mut request = self
            .http
            .post(&url)
            .json(body)
            .timeout(self.request_timeout);
        if !self.api_key.is_empty() {
            for (name, value) in self.provider.auth_headers(&self.api_key) {
                request = request.header(name, value);
            }
        }
        for (name, value) in self.provider.extra_headers() {
            request = request.header(name, value);
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

        if let Some(callback) = on_delta {
            self.parse_stream(response, callback).await
        } else {
            let bytes = response.bytes().await.map_err(|e| Error::Llm {
                message: format!("failed to read response body: {e}"),
                retryable: true,
            })?;
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|e| Error::Llm {
                    message: format!("malformed response JSON: {e}"),
                    retryable: false,
                })?;
            self.provider.parse_completion(&value, &self.model)
        }
    }

    /// Read an SSE stream and feed every `data:` payload to the provider's
    /// stream parser, forwarding content deltas.
    async fn parse_stream(
        &self,
        response: reqwest::Response,
        on_delta: &mut (dyn FnMut(String) + Send),
    ) -> Result<LLMResponse> {
        let mut parser = self.provider.stream_parser();
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();

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
                if data.is_empty() {
                    continue;
                }
                if let Some(delta) = parser.on_event(data)? {
                    match delta {
                        StreamDelta::Content(text) => on_delta(text),
                        StreamDelta::Done => {
                            buffer.clear();
                            break;
                        }
                    }
                }
            }
            if buffer.is_empty() {
                break;
            }
        }

        parser.finish()
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
