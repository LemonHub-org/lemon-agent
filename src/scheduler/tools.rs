//! Tool dispatch: turn a validated `{tool, args}` request into a sandboxed
//! execution. Used by the scheduler loop and by the Rhai script bridge.

use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::kernel::capability::CapabilitySet;
use crate::kernel::event_store::EventType;
use crate::kernel::sandbox::Sandbox;
use crate::llm::{LLMClient, Message};

/// The components a tool call may touch. The LLM is optional so pure
/// file/process tools work without a model configured.
pub struct ToolComponents<'a> {
    pub sandbox: &'a Sandbox,
    pub token: &'a CapabilitySet,
    pub llm: Option<&'a LLMClient>,
    /// Called before an LLM query with the prompt preview; used for
    /// `LlmRequest` audit events.
    pub on_llm_request: Option<&'a dyn Fn(&str)>,
}

/// Execute `tool` with `args`, validating arguments against the tool contract.
pub async fn dispatch(components: &ToolComponents<'_>, tool: &str, args: &Value) -> Result<Value> {
    match tool {
        "read_file" => {
            let path = require_str(args, "path")?;
            Ok(json!({ "content": components.sandbox.read_file(components.token, path).await? }))
        }
        "write_file" => {
            let path = require_str(args, "path")?;
            let content = require_str(args, "content")?;
            components
                .sandbox
                .write_file(components.token, path, content)
                .await?;
            Ok(json!({ "ok": true }))
        }
        "append_file" => {
            let path = require_str(args, "path")?;
            let content = require_str(args, "content")?;
            components
                .sandbox
                .append_file(components.token, path, content)
                .await?;
            Ok(json!({ "ok": true }))
        }
        "list_dir" => {
            let path = require_str(args, "path")?;
            let entries = components.sandbox.list_dir(components.token, path).await?;
            Ok(json!({ "entries": entries }))
        }
        "search_code" => {
            let query = require_str(args, "query")?;
            let path = require_str(args, "path")?;
            let hits = components
                .sandbox
                .search_code(components.token, query, path)
                .await?;
            Ok(json!({ "hits": hits }))
        }
        "exec_command" => {
            let cmd = require_str(args, "cmd")?;
            let arg_values = require_array(args, "args")?;
            let mut cmd_args = Vec::with_capacity(arg_values.len());
            for value in arg_values {
                let s = value.as_str().ok_or_else(|| {
                    Error::InvalidInput("exec_command args must be an array of strings".to_string())
                })?;
                cmd_args.push(s.to_string());
            }
            let output = components
                .sandbox
                .exec_command(components.token, cmd, &cmd_args)
                .await?;
            Ok(json!(output))
        }
        "git_add_commit" => {
            let message = require_str(args, "message")?;
            let hash = components
                .sandbox
                .git_add_commit(components.token, message)
                .await?;
            Ok(json!({ "hash": hash }))
        }
        "llm_query" => {
            let llm = components.llm.ok_or_else(|| Error::CapabilityDenied {
                operation: "llm_query".to_string(),
                reason: "no LLM client configured".to_string(),
            })?;
            let prompt = require_str(args, "prompt")?;
            if let Some(cb) = components.on_llm_request {
                cb(prompt);
            }
            let response = llm.chat(&[Message::user(prompt)], &[]).await?;
            Ok(json!({ "content": response.content }))
        }
        "sleep" => {
            let ms = require_u64(args, "ms")?;
            components.sandbox.sleep(components.token, ms).await?;
            Ok(json!({ "ok": true }))
        }
        other => Err(Error::InvalidInput(format!("unknown tool {other:?}"))),
    }
}

fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidInput(format!("tool args missing string field {key:?}")))
}

fn require_u64(args: &Value, key: &str) -> Result<u64> {
    args.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::InvalidInput(format!("tool args missing integer field {key:?}")))
}

fn require_array<'a>(args: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    args.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidInput(format!("tool args missing array field {key:?}")))
}

/// Map a tool call outcome into a context message plus the event to persist.
/// Returns the `Tool` role message for the LLM and the audit event type.
pub fn tool_message_and_event(
    tool: &str,
    args: &Value,
    result: &Result<Value>,
) -> (Message, EventType) {
    let content = match result {
        Ok(value) => serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string()),
        Err(e) => format!("ERROR: {e}"),
    };
    let event = EventType::ToolCall {
        tool_name: tool.to_string(),
        args: args.clone(),
        result: match result {
            Ok(value) => crate::kernel::event_store::ToolOutcome::Ok(value.clone()),
            Err(e) => crate::kernel::event_store::ToolOutcome::Err(e.to_string()),
        },
    };
    (Message::tool_result(tool.to_string(), content), event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_args_fail_loudly() {
        let err = require_str(&json!({}), "path").unwrap_err();
        assert!(err.to_string().contains("missing string field"));
        let err = require_u64(&json!({}), "ms").unwrap_err();
        assert!(err.to_string().contains("missing integer field"));
        let err = require_array(&json!({}), "args").unwrap_err();
        assert!(err.to_string().contains("missing array field"));
    }
}
