//! The agent's main loop: budget checks, heartbeats, snapshots, and the
//! per-state handlers that drive a continuity to completion.

use std::time::Duration;

use crate::error::{Error, Result};
use crate::evolution::{
    EvolutionContext, EvolutionOutcome, STRATEGY_SCRIPT, ValidationSpec, recent_messages,
    recent_tool_results,
};
use crate::kernel::event_store::{EventType, now_ms};
use crate::llm::Message;
use crate::scheduler::plan::KNOWN_TOOLS;
use crate::scheduler::{Agent, AgentState, ContinuityReport, TerminationReason};

const PLAN_SYSTEM_PROMPT: &str = r#"You are Lemon Agent, an autonomous software engineering agent running inside a sandboxed working directory. You receive a task, inspect the workspace with tools, modify files, and verify your work by running commands.

Respond with ONLY a JSON object, no prose or markdown:
{"steps": [{"name": "short description", "tool": "tool_name", "args": { ... }}], "verify": {"cmd": "whitelisted_command", "args": ["arg1", ...]}}

Tools:
- read_file: {"path": "relative/path"}
- write_file: {"path": "relative/path", "content": "file contents"}
- append_file: {"path": "relative/path", "content": "text to append"}
- list_dir: {"path": "relative/path"}
- search_code: {"query": "text", "path": "relative/path"}
- exec_command: {"cmd": "git|cargo|rustc|python3|ls", "args": ["argument", ...]}
- git_add_commit: {"message": "single-line message"}
- llm_query: {"prompt": "question to the model"}
- sleep: {"ms": 100}

Rules:
- All paths are relative to the sandbox root. Never use absolute paths or "..".
- Only whitelisted commands may be executed; they run non-interactively with a 120-second timeout.
- Commit changes only after verification passes.
- Keep the plan small: a few steps is enough.
- The optional "verify" object runs after the steps; use it for tests or a final check (for example {"cmd": "cargo", "args": ["test"]})."#;

const SUMMARIZE_PROMPT: &str = "You are a conversation summarizer. Compress the conversation below into a concise summary that preserves all facts, decisions, tool results, and remaining work. Output only the summary text.";

impl Agent {
    /// Run the continuity to completion and return the final report.
    pub async fn run(&mut self) -> Result<ContinuityReport> {
        if self.state == AgentState::Idle && self.pending_task.is_none() {
            return Ok(ContinuityReport {
                continuity_id: self.continuity_id.clone(),
                status: "idle".to_string(),
                steps_used: 0,
                summary: "no task provided; agent stayed idle".to_string(),
            });
        }

        loop {
            if self.state == AgentState::Terminated {
                break;
            }
            if self.state == AgentState::Idle && self.pending_task.is_none() {
                break;
            }
            if let Some(event) = self.maybe_heartbeat()? {
                self.store.append(&self.continuity_id, &event)?;
            }
            if let Err(e) = self.budget.check(now_ms()) {
                tracing::error!(
                    continuity_id = %self.continuity_id,
                    code = %e.code(),
                    error = %e,
                    "budget exhausted"
                );
                self.termination = Some(TerminationReason::BudgetExhausted(e.to_string()));
                self.state = AgentState::Terminated;
                break;
            }

            let (next, events) = self.transition().await?;
            self.state = next;
            self.store.append_many(&self.continuity_id, &events)?;
            tracing::info!(
                continuity_id = %self.continuity_id,
                state = %self.state.as_str(),
                step_num = self.budget.steps_used,
                events = events.len(),
                "state transition complete"
            );
            self.maybe_snapshot()?;
            tokio::time::sleep(Duration::from_millis(self.loop_sleep_ms)).await;
        }

        self.finish().await
    }

    /// Handle one state transition, returning the next state and the events
    /// to persist.
    async fn transition(&mut self) -> Result<(AgentState, Vec<EventType>)> {
        match self.state {
            AgentState::Idle => self.handle_idle().await,
            AgentState::Planning => self.handle_planning().await,
            AgentState::Executing => self.handle_executing().await,
            AgentState::Evaluating => self.handle_evaluating().await,
            AgentState::Evolving => self.handle_evolving().await,
            AgentState::Terminated => Err(Error::Internal(
                "transition called in terminated state".to_string(),
            )),
        }
    }

    async fn handle_idle(&mut self) -> Result<(AgentState, Vec<EventType>)> {
        if let Some(task) = self.pending_task.take() {
            self.initial_prompt = task.clone();
            self.store.append(
                &self.continuity_id,
                &EventType::ContinuityStarted {
                    initial_prompt: task,
                },
            )?;
            Ok((AgentState::Planning, Vec::new()))
        } else {
            Ok((AgentState::Idle, Vec::new()))
        }
    }

    async fn handle_planning(&mut self) -> Result<(AgentState, Vec<EventType>)> {
        let mut events = Vec::new();
        let step_num = self.start_step(&mut events);

        if self.ctx.is_empty() {
            self.ctx.push(Message::system(PLAN_SYSTEM_PROMPT));
            self.ctx.push(Message::user(&self.initial_prompt));
        }

        let mut failures = 0;
        let mut last_failure = String::new();
        let plan = loop {
            if failures >= 2 {
                let reason = if last_failure.is_empty() {
                    "planning produced no valid plan after two attempts".to_string()
                } else {
                    format!("planning failed after two attempts: {last_failure}")
                };
                self.termination = Some(TerminationReason::Failed(reason.clone()));
                events.push(EventType::Error {
                    error: reason,
                    recoverable: false,
                });
                events.push(EventType::StepFinished { step_num });
                return Ok((AgentState::Terminated, events));
            }

            let preview_text = self
                .ctx
                .messages()
                .last()
                .map(|m| preview(&m.content, 200))
                .unwrap_or_default();
            events.push(EventType::LlmRequest {
                prompt_preview: preview_text,
                tools: KNOWN_TOOLS.iter().map(|s| s.to_string()).collect(),
            });
            self.budget.record_llm_call(self.ctx.estimated_tokens());

            let response = match self.llm.chat(self.ctx.messages(), &[]).await {
                Ok(response) => response,
                Err(e) => {
                    last_failure = format!("LLM planning failed: {e}");
                    events.push(EventType::Error {
                        error: last_failure.clone(),
                        recoverable: true,
                    });
                    failures += 1;
                    continue;
                }
            };
            events.push(EventType::LlmResponse {
                content: preview(&response.content, 200),
                tool_calls: response.tool_calls.clone(),
            });
            self.ctx.push(Message::assistant(&response.content));

            match crate::scheduler::plan::Plan::parse(&response.content) {
                Ok(plan) => break plan,
                Err(e) => {
                    last_failure = format!("plan rejected: {e}");
                    events.push(EventType::Error {
                        error: last_failure.clone(),
                        recoverable: true,
                    });
                    failures += 1;
                }
            }
        };
        self.plan = Some(plan);

        self.compact_if_needed(&mut events).await?;
        events.push(EventType::StepFinished { step_num });
        Ok((AgentState::Executing, events))
    }

    async fn handle_executing(&mut self) -> Result<(AgentState, Vec<EventType>)> {
        let mut events = Vec::new();
        let step_num = self.start_step(&mut events);

        let Some(plan) = self.plan.clone() else {
            self.termination = Some(TerminationReason::Failed("no plan to execute".to_string()));
            events.push(EventType::Error {
                error: "no plan to execute".to_string(),
                recoverable: false,
            });
            events.push(EventType::StepFinished { step_num });
            return Ok((AgentState::Terminated, events));
        };

        // The strategy script owns plan execution: it runs the steps and the
        // verification command with the sandboxed tools. Its errors become
        // the failure reason that drives the Evolving state.
        self.budget.record_tool_call();
        match self.scripts.execute_plan(STRATEGY_SCRIPT, &plan).await {
            Ok(message) => {
                self.ctx
                    .push_tool_result("script", &message, self.tool_result_max_chars);
            }
            Err(e) => {
                let reason = format!("strategy script failed: {e}");
                self.plan_failed_reason = Some(reason.clone());
                events.push(EventType::Error {
                    error: reason,
                    recoverable: true,
                });
            }
        }

        events.push(EventType::StepFinished { step_num });
        Ok((AgentState::Evaluating, events))
    }

    async fn handle_evaluating(&mut self) -> Result<(AgentState, Vec<EventType>)> {
        let mut events = Vec::new();
        let step_num = self.start_step(&mut events);

        if self.plan_failed_reason.is_some() {
            events.push(EventType::StepFinished { step_num });
            return Ok((AgentState::Evolving, events));
        }

        self.termination = Some(TerminationReason::Completed);
        events.push(EventType::StepFinished { step_num });
        Ok((AgentState::Terminated, events))
    }

    async fn handle_evolving(&mut self) -> Result<(AgentState, Vec<EventType>)> {
        let mut events = Vec::new();
        let step_num = self.start_step(&mut events);

        let error = self
            .plan_failed_reason
            .take()
            .unwrap_or_else(|| "unknown failure".to_string());
        let script_source = self
            .scripts
            .scripts_dir()
            .join(format!("{STRATEGY_SCRIPT}.rhai"))
            .to_string_lossy()
            .into_owned();
        let script_source = std::fs::read_to_string(&script_source).unwrap_or_default();

        let stored_events = self.store.events_after(&self.continuity_id, 0)?;
        let ctx = EvolutionContext {
            continuity_id: self.continuity_id.clone(),
            step_num,
            error,
            script_name: STRATEGY_SCRIPT.to_string(),
            script_source,
            recent_messages: recent_messages(self.ctx.messages(), 10),
            recent_tool_results: recent_tool_results(&stored_events, 5),
            validation: ValidationSpec::TestEntry,
        };

        match self.evolution.attempt(&ctx).await? {
            EvolutionOutcome::Fixed { message, .. } => {
                self.evolution_attempts = self.evolution.attempts_used();
                tracing::info!(
                    continuity_id = %self.continuity_id,
                    attempt = self.evolution_attempts,
                    "strategy script evolved; re-executing plan"
                );
                events.push(EventType::Error {
                    error: format!("script evolved: {message}"),
                    recoverable: true,
                });
                events.push(EventType::StepFinished { step_num });
                Ok((AgentState::Executing, events))
            }
            EvolutionOutcome::Failed { reason } => {
                self.evolution_attempts = self.evolution.attempts_used();
                tracing::error!(
                    continuity_id = %self.continuity_id,
                    attempt = self.evolution_attempts,
                    error = %reason,
                    "evolution failed"
                );
                self.termination = Some(TerminationReason::Failed(format!(
                    "evolution failed: {reason}"
                )));
                events.push(EventType::StepFinished { step_num });
                Ok((AgentState::Terminated, events))
            }
        }
    }

    /// Record a step boundary and return the step number.
    fn start_step(&mut self, events: &mut Vec<EventType>) -> usize {
        self.budget.record_step();
        let step_num = self.budget.steps_used;
        events.push(EventType::StepStarted { step_num });
        step_num
    }

    /// Summarize the context through the LLM when it exceeds the token limit.
    async fn compact_if_needed(&mut self, events: &mut Vec<EventType>) -> Result<()> {
        if !self.ctx.needs_compaction() {
            return Ok(());
        }
        let history = self.ctx.history_for_summary();
        events.push(EventType::LlmRequest {
            prompt_preview: "context compaction".to_string(),
            tools: Vec::new(),
        });
        self.budget.record_llm_call(history.chars().count() / 4 + 4);
        let summary = self
            .llm
            .chat(
                &[Message::system(SUMMARIZE_PROMPT), Message::user(history)],
                &[],
            )
            .await?
            .content;
        events.push(EventType::LlmResponse {
            content: preview(&summary, 200),
            tool_calls: Vec::new(),
        });
        self.ctx.apply_summary(&summary);
        Ok(())
    }

    /// Persist the terminal event, reset to Idle, and return the report.
    async fn finish(&mut self) -> Result<ContinuityReport> {
        let now = now_ms();
        let termination = self.termination.take().unwrap_or_else(|| {
            TerminationReason::Failed("terminated without a reason".to_string())
        });
        let (status, summary) = match termination {
            TerminationReason::Completed => (
                "completed",
                format!("task completed successfully. {}", self.budget.summary(now)),
            ),
            TerminationReason::Failed(reason) => (
                "failed",
                format!("task failed: {reason}. {}", self.budget.summary(now)),
            ),
            TerminationReason::BudgetExhausted(reason) => (
                "budget_exhausted",
                format!("{reason}. {}", self.budget.summary(now)),
            ),
        };
        let report = ContinuityReport {
            continuity_id: self.continuity_id.clone(),
            status: status.to_string(),
            steps_used: self.budget.steps_used,
            summary,
        };
        self.store.append(
            &self.continuity_id,
            &EventType::ContinuityFinished {
                status: status.to_string(),
                summary: report.summary.clone(),
            },
        )?;
        self.state = AgentState::Idle;
        Ok(report)
    }
}

/// Truncate a string for safe logging and event previews.
fn preview(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push_str("...");
        out
    }
}
