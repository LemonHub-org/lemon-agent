//! The evolution engine: analyzes strategy-script failures, generates
//! candidate scripts through the LLM, and only replaces the live script after
//! compilation and isolated validation succeed. Failed candidates always
//! roll back to the previous version.

pub mod script_engine;

use std::path::Path;
use std::sync::Arc;

use tracing::{info, warn};

use crate::error::{Error, Result};
use crate::evolution::script_engine::ScriptEngine;
use crate::kernel::capability::CapabilitySet;
use crate::kernel::event_store::{EventStore, EventType};
use crate::kernel::sandbox::Sandbox;
use crate::llm::{LLMClient, Message};

/// The name of the strategy script the agent executes plans with.
pub const STRATEGY_SCRIPT: &str = "plan_and_execute";

/// Everything the evolution prompt needs to understand a failure.
#[derive(Debug, Clone)]
pub struct EvolutionContext {
    pub continuity_id: String,
    pub step_num: usize,
    pub error: String,
    pub script_name: String,
    pub script_source: String,
    pub recent_messages: Vec<Message>,
    pub recent_tool_results: Vec<String>,
    /// How to validate a candidate script.
    pub validation: ValidationSpec,
}

/// What a candidate script must pass before it replaces the live script.
#[derive(Debug, Clone)]
pub enum ValidationSpec {
    /// Only compilation is required.
    CompileOnly,
    /// The script's `test_<name>()` entry must run and return.
    TestEntry,
}

/// The outcome of an evolution attempt.
#[derive(Debug, Clone)]
pub enum EvolutionOutcome {
    /// The candidate replaced the script and passed validation.
    Fixed {
        old_hash: String,
        new_hash: String,
        message: String,
    },
    /// No usable candidate was produced or validated.
    Failed { reason: String },
}

/// Generates, validates, and applies strategy-script improvements.
#[derive(Debug)]
pub struct EvolutionEngine {
    llm: Arc<LLMClient>,
    store: Arc<EventStore>,
    engine: Arc<ScriptEngine>,
    max_attempts: usize,
    attempts_used: usize,
}

impl EvolutionEngine {
    /// Build an engine. `engine` is the live script runtime the candidates
    /// are applied to; validation runs in an isolated scratch environment.
    pub fn new(
        llm: Arc<LLMClient>,
        store: Arc<EventStore>,
        engine: Arc<ScriptEngine>,
        max_attempts: usize,
    ) -> EvolutionEngine {
        EvolutionEngine {
            llm,
            store,
            engine,
            max_attempts: max_attempts.max(1),
            attempts_used: 0,
        }
    }

    /// The number of attempts consumed (persisted per continuity by the
    /// scheduler).
    pub fn attempts_used(&self) -> usize {
        self.attempts_used
    }

    /// Restore the attempt counter, e.g. from a recovered snapshot.
    pub fn set_attempts_used(&mut self, used: usize) {
        self.attempts_used = used;
    }

    /// Run one evolution attempt. On `Fixed` the live script has been
    /// replaced and reloaded; on `Failed` the previous script (if any) is
    /// intact. Both outcomes record `EvolutionAttempt` and `EvolutionResult`
    /// events.
    pub async fn attempt(&mut self, ctx: &EvolutionContext) -> Result<EvolutionOutcome> {
        if self.attempts_used >= self.max_attempts {
            return Ok(EvolutionOutcome::Failed {
                reason: format!(
                    "evolution attempt limit reached ({} of {})",
                    self.attempts_used, self.max_attempts
                ),
            });
        }
        self.attempts_used += 1;

        let candidate = self.generate_candidate(ctx).await?;
        let Some(candidate) = candidate else {
            return Ok(EvolutionOutcome::Failed {
                reason: "LLM produced no candidate script".to_string(),
            });
        };
        if let Some(reason) = static_danger_check(&candidate) {
            return Ok(EvolutionOutcome::Failed { reason });
        }

        let script_path = self
            .engine
            .scripts_dir()
            .join(format!("{}.rhai", ctx.script_name));
        let old_hash = match std::fs::read(&script_path) {
            Ok(bytes) => sha256_hex(&bytes),
            Err(_) => "<missing>".to_string(),
        };
        let new_hash = sha256_hex(candidate.as_bytes());

        let backup = script_path.with_extension("rhai.bak");
        let had_original = script_path.exists();
        if had_original {
            std::fs::rename(&script_path, &backup)
                .map_err(|e| Error::io(Some(script_path.clone()), e))?;
        }

        let outcome = if let Err(write_err) = atomic_write(&script_path, candidate.as_bytes()) {
            self.rollback(&script_path, &backup, had_original);
            EvolutionOutcome::Failed {
                reason: format!("candidate write failed: {write_err}"),
            }
        } else if let Err(e) = self.engine.reload(&ctx.script_name) {
            self.rollback(&script_path, &backup, had_original);
            EvolutionOutcome::Failed {
                reason: format!("candidate failed to compile: {e}"),
            }
        } else {
            match self.validate_candidate(ctx, candidate.as_bytes()).await {
                Ok(message) => {
                    if had_original && let Err(e) = std::fs::remove_file(&backup) {
                        warn!(script = %ctx.script_name, error = %e, "failed to remove backup after successful evolution");
                    }
                    info!(
                        script = %ctx.script_name,
                        old_hash = %old_hash,
                        new_hash = %new_hash,
                        "evolution candidate accepted"
                    );
                    EvolutionOutcome::Fixed {
                        old_hash: old_hash.clone(),
                        new_hash: new_hash.clone(),
                        message,
                    }
                }
                Err(reason) => {
                    self.rollback(&script_path, &backup, had_original);
                    EvolutionOutcome::Failed { reason }
                }
            }
        };

        let (success, reason) = match &outcome {
            EvolutionOutcome::Fixed { message, .. } => (true, message.clone()),
            EvolutionOutcome::Failed { reason } => (false, reason.clone()),
        };
        self.record_events(ctx, &script_path, &old_hash, &new_hash, success, &reason)?;
        Ok(outcome)
    }

    /// Ask the LLM for an improved script. Returns `None` when the response
    /// contains no usable script source.
    async fn generate_candidate(&self, ctx: &EvolutionContext) -> Result<Option<String>> {
        let prompt = build_evolution_prompt(ctx);
        let response = self.llm.chat(&[Message::user(prompt)], &[]).await?;
        let candidate = extract_script(&response.content);
        Ok(candidate)
    }

    /// Validate a candidate in an isolated scratch environment: a fresh
    /// scripts directory with a scratch sandbox, so the live workspace is
    /// never touched by an unvetted script.
    async fn validate_candidate(
        &self,
        ctx: &EvolutionContext,
        candidate: &[u8],
    ) -> std::result::Result<String, String> {
        let scratch_root = self
            .engine
            .scripts_dir()
            .join(".validation")
            .join(&ctx.continuity_id);
        if scratch_root.exists() {
            std::fs::remove_dir_all(&scratch_root).map_err(|e| e.to_string())?;
        }
        std::fs::create_dir_all(&scratch_root).map_err(|e| e.to_string())?;
        let scratch_scripts = scratch_root.join("scripts");
        std::fs::create_dir_all(&scratch_scripts).map_err(|e| e.to_string())?;
        let scratch_workspace = scratch_root.join("workspace");
        std::fs::create_dir_all(&scratch_workspace).map_err(|e| e.to_string())?;

        let candidate_path = scratch_scripts.join(format!("{}.rhai", ctx.script_name));
        std::fs::write(&candidate_path, candidate).map_err(|e| e.to_string())?;

        let allowed: std::collections::HashSet<String> = ["git", "cargo", "rustc", "python3", "ls"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let sandbox = Arc::new(
            Sandbox::new(
                scratch_workspace,
                allowed,
                std::time::Duration::from_secs(30),
                1024 * 1024,
            )
            .map_err(|e| e.to_string())?,
        );
        let isolated =
            ScriptEngine::new(scratch_scripts, CapabilitySet::full(), sandbox, None, None)
                .map_err(|e| format!("candidate failed to compile in isolation: {e}"))?;

        match &ctx.validation {
            ValidationSpec::CompileOnly => Ok("compilation passed".to_string()),
            ValidationSpec::TestEntry => match isolated.run_test(&ctx.script_name).await {
                Ok(Some(_)) => Ok("test entry passed".to_string()),
                Ok(None) => Err(format!(
                    "candidate defines no test_{}() entry",
                    ctx.script_name.replace('.', "_")
                )),
                Err(e) => Err(format!("test entry failed in isolation: {e}")),
            },
        }
    }

    fn rollback(&self, script_path: &Path, backup: &Path, had_original: bool) {
        let name = script_name_of(script_path);
        if had_original {
            if let Err(e) = std::fs::rename(backup, script_path) {
                warn!(script = %name, error = %e, "rollback failed to restore backup");
                return;
            }
        } else if let Err(e) = std::fs::remove_file(script_path) {
            warn!(script = %name, error = %e, "rollback failed to remove candidate");
        }
        if let Err(e) = self.engine.reload(&name) {
            warn!(script = %name, error = %e, "rollback reload failed");
        }
        warn!(script = %name, "evolution candidate rolled back");
    }

    fn record_events(
        &self,
        ctx: &EvolutionContext,
        script_path: &Path,
        old_hash: &str,
        new_hash: &str,
        success: bool,
        reason: &str,
    ) -> Result<()> {
        self.store.append(
            &ctx.continuity_id,
            &EventType::EvolutionAttempt {
                script_path: script_path.to_string_lossy().into_owned(),
                old_hash: old_hash.to_string(),
                new_hash: new_hash.to_string(),
            },
        )?;
        self.store.append(
            &ctx.continuity_id,
            &EventType::EvolutionResult {
                success,
                reason: reason.to_string(),
            },
        )?;
        Ok(())
    }
}

/// Build the structured prompt that asks the LLM for an improved script.
fn build_evolution_prompt(ctx: &EvolutionContext) -> String {
    let messages = ctx
        .recent_messages
        .iter()
        .map(|m| {
            let role = match m.role {
                crate::llm::Role::System => "system",
                crate::llm::Role::User => "user",
                crate::llm::Role::Assistant => "assistant",
                crate::llm::Role::Tool => "tool",
            };
            format!("[{role}] {}", truncate(&m.content, 400))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let tool_results = if ctx.recent_tool_results.is_empty() {
        "(none)".to_string()
    } else {
        ctx.recent_tool_results.join("\n")
    };
    format!(
        r#"You are improving the strategy script of an autonomous coding agent. The script is written in Rhai and executes task plans. Its execution failed with the error below.

Analyze the cause and return a COMPLETE, corrected script. Requirements:
- Define `fn execute_plan(plan)`.
- Define a self-test `fn test_{script_name}()` that runs a safe command (for example `exec_command("git", ["--version"])`) and returns a string; raise with `throw` when it fails.
- `plan` is an object map with `steps` (array of {{ name, tool, args }}) and optionally `verify` ({{ cmd, args }}).
- Available functions: read_file(path), write_file(path, content), append_file(path, content), list_dir(path), search_code(query, path), exec_command(cmd, args), git_add_commit(message), llm_query(prompt), sleep(ms), log_debug(msg), log_info(msg), log_warn(msg).
- All paths are relative to the sandbox root. Use `throw "message"` for failures.
- Respond with ONLY the script source, no markdown fences, no commentary.

Continuity: {continuity}
Step: {step}
Error:
{error}

Current script:
```rhai
{script}
```

Recent conversation:
{messages}

Recent tool results:
{tool_results}
"#,
        continuity = ctx.continuity_id,
        step = ctx.step_num,
        error = truncate(&ctx.error, 1000),
        script = truncate(&ctx.script_source, 4000),
        script_name = ctx.script_name,
        messages = truncate(&messages, 4000),
        tool_results = truncate(&tool_results, 3000),
    )
}

/// Extract the candidate script from an LLM response, tolerating markdown
/// fences and surrounding prose.
pub fn extract_script(content: &str) -> Option<String> {
    let content = content.trim();
    if let Some(start) = content.find("```") {
        let after_fence = &content[start + 3..];
        let start = after_fence.find('\n').map(|i| i + 1).unwrap_or(0);
        let end = after_fence[start..]
            .find("```")
            .map(|i| start + i)
            .unwrap_or(after_fence.len());
        let candidate = after_fence[start..end].trim();
        if candidate.contains("fn execute_plan") {
            return Some(candidate.to_string());
        }
    }
    if content.contains("fn execute_plan") {
        return Some(content.to_string());
    }
    None
}

/// A cheap static scan for capabilities scripts must not use. The sandbox
/// already gates tools; this is defense in depth.
fn static_danger_check(source: &str) -> Option<String> {
    for (needle, what) in [
        ("eval(", "dynamic evaluation"),
        ("import(", "module imports"),
        ("fn_ptr(", "function pointers"),
    ] {
        if source.contains(needle) {
            return Some(format!("candidate uses {what}, which is forbidden"));
        }
    }
    if source.len() > 64 * 1024 {
        return Some("candidate exceeds the 64 KiB size limit".to_string());
    }
    None
}

/// Hex SHA-256 of a byte slice.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "script".to_string());
    let tmp = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn script_name_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
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

/// Extract the most recent tool call outcomes from an event list.
pub fn recent_tool_results(
    events: &[crate::kernel::event_store::StoredEvent],
    max: usize,
) -> Vec<String> {
    events
        .iter()
        .filter_map(|stored| match &stored.event {
            EventType::ToolCall {
                tool_name,
                args,
                result,
            } => {
                let outcome = match result {
                    crate::kernel::event_store::ToolOutcome::Ok(value) => {
                        truncate(&value.to_string(), 300)
                    }
                    crate::kernel::event_store::ToolOutcome::Err(e) => {
                        format!("ERROR: {e}")
                    }
                };
                Some(format!(
                    "{tool_name}({}) -> {outcome}",
                    truncate(&args.to_string(), 200)
                ))
            }
            _ => None,
        })
        .rev()
        .take(max)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// Render the last `max` messages from a context.
pub fn recent_messages(messages: &[Message], max: usize) -> Vec<Message> {
    let skip = messages.len().saturating_sub(max);
    messages[skip..].to_vec()
}
