//! OpenAI-compatible LLM gateway with pluggable providers.

pub mod client;
pub mod provider;

pub use client::{LLMClient, LLMResponse, Message, Role, ToolDefinition, Usage};
