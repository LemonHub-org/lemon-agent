//! OpenAI-compatible LLM gateway.

pub mod client;

pub use client::{LLMClient, LLMResponse, Message, Role, ToolDefinition};
