//! The scheduler orchestrates the agent's state machine, budget, and context.

pub mod budget;
pub mod context;
pub mod loop_runner;
pub mod plan;
pub mod tools;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::config::Config;
use crate::error::Result;
use crate::kernel::capability::CapabilitySet;
use crate::kernel::event_store::{EventStore, EventType, StoredSnapshot};
use crate::kernel::sandbox::Sandbox;
use crate::llm::LLMClient;

pub use budget::Budget;
pub use context::Context;
pub use plan::{Plan, PlanStep};

/// The agent's finite state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Waiting for a new task.
    Idle,
    /// Producing a plan through the LLM.
    Planning,
    /// Executing plan steps through sandboxed tools.
    Executing,
    /// Verifying the outcome (running the verify command).
    Evaluating,
    /// Improving the approach after a failure.
    Evolving,
    /// The continuity is over.
    Terminated,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Planning => "planning",
            AgentState::Executing => "executing",
            AgentState::Evaluating => "evaluating",
            AgentState::Evolving => "evolving",
            AgentState::Terminated => "terminated",
        }
    }
}

/// Why a continuity ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    Completed,
    Failed(String),
    BudgetExhausted(String),
}

/// The final result of a run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContinuityReport {
    pub continuity_id: String,
    pub status: String,
    pub steps_used: usize,
    pub summary: String,
}

/// The agent: owns the components and the running state of one continuity.
#[derive(Debug)]
pub struct Agent {
    pub store: Arc<EventStore>,
    pub sandbox: Arc<Sandbox>,
    pub llm: Arc<LLMClient>,
    pub token: CapabilitySet,
    pub continuity_id: String,
    pub state: AgentState,
    pub ctx: Context,
    pub budget: Budget,
    pub plan: Option<Plan>,
    pub plan_failed_reason: Option<String>,
    pub replan_context: Option<String>,
    pub evolution_attempts: usize,
    pub initial_prompt: String,
    pub termination: Option<TerminationReason>,
    pub pending_task: Option<String>,
    pub max_evolution_attempts: usize,
    pub heartbeat_interval_secs: u64,
    pub snapshot_interval_steps: usize,
    pub loop_sleep_ms: u64,
    pub tool_result_max_chars: usize,
    pub keep_recent_messages: usize,
    pub max_context_tokens: usize,
    last_heartbeat_at_ms: u64,
    last_heartbeat_step: usize,
    last_snapshot_step: usize,
}

impl Agent {
    /// Create an agent for `config`. An incomplete continuity is resumed;
    /// otherwise a new continuity is started, optionally with `task` pending.
    pub fn new(config: &Config, task: Option<String>) -> Result<Agent> {
        let store = Arc::new(EventStore::open(&config.agent.db_path)?);
        let sandbox = Arc::new(
            Sandbox::new(
                config.sandbox.root_dir.clone(),
                config.sandbox.allowed_commands.iter().cloned().collect(),
                std::time::Duration::from_secs(config.agent.command_timeout_secs),
                config.agent.max_file_size_bytes,
            )?
            .with_event_store(store.clone()),
        );
        let llm = Arc::new(LLMClient::new(&config.llm)?);

        let mut agent = Agent {
            store: store.clone(),
            sandbox: sandbox.clone(),
            llm,
            token: CapabilitySet::full(),
            continuity_id: String::new(),
            state: AgentState::Idle,
            ctx: Context::new(config.agent.max_context_tokens, 6),
            budget: Budget::new(
                config.agent.max_steps,
                config.agent.max_input_tokens,
                config.agent.max_llm_calls,
                config.agent.max_tool_calls,
                config.agent.max_wall_clock_secs,
                crate::kernel::event_store::now_ms(),
            ),
            plan: None,
            plan_failed_reason: None,
            replan_context: None,
            evolution_attempts: 0,
            initial_prompt: String::new(),
            termination: None,
            pending_task: None,
            max_evolution_attempts: config.agent.max_evolution_attempts,
            heartbeat_interval_secs: config.agent.heartbeat_interval_secs,
            snapshot_interval_steps: config.agent.snapshot_interval_steps,
            loop_sleep_ms: config.agent.loop_sleep_ms,
            tool_result_max_chars: 4096,
            keep_recent_messages: 6,
            max_context_tokens: config.agent.max_context_tokens,
            last_heartbeat_at_ms: crate::kernel::event_store::now_ms(),
            last_heartbeat_step: 0,
            last_snapshot_step: 0,
        };

        let resumed = match store.incomplete_continuities()?.into_iter().next() {
            Some(continuity_id) => {
                let snapshot = store.latest_snapshot(&continuity_id)?;
                if let Some(snapshot) = snapshot {
                    agent.restore_from_snapshot(&snapshot)?;
                    true
                } else {
                    agent.continuity_id = continuity_id;
                    false
                }
            }
            None => false,
        };

        if !resumed {
            // A fresh continuity is only materialized when a task arrives
            // (in the Idle handler); a taskless start leaves no artifacts.
            agent.continuity_id = Uuid::new_v4().to_string();
            agent.pending_task = task;
        }
        sandbox.set_continuity(&agent.continuity_id);
        Ok(agent)
    }

    /// Restore runtime state from the newest snapshot and recount budget
    /// usage from the events that follow it.
    fn restore_from_snapshot(&mut self, snapshot: &StoredSnapshot) -> Result<()> {
        self.continuity_id = snapshot.continuity_id.clone();
        self.restore_from_value(&snapshot.state)?;
        let events = self.store.events_after(&self.continuity_id, snapshot.seq)?;
        for stored in &events {
            self.apply_event_for_accounting(&stored.event);
        }
        Ok(())
    }

    fn apply_event_for_accounting(&mut self, event: &EventType) {
        match event {
            EventType::StepStarted { .. } => self.budget.record_step(),
            EventType::ToolCall { .. } => self.budget.record_tool_call(),
            EventType::LlmRequest { prompt_preview, .. } => {
                self.budget
                    .record_llm_call(prompt_preview.chars().count() / 4 + 4);
            }
            EventType::EvolutionAttempt { .. } => self.evolution_attempts += 1,
            _ => {}
        }
    }

    /// Serialize the runtime state for a snapshot.
    pub fn snapshot_value(&self) -> Value {
        json!({
            "state": self.state,
            "initial_prompt": self.initial_prompt,
            "plan": self.plan,
            "plan_failed_reason": self.plan_failed_reason,
            "replan_context": self.replan_context,
            "evolution_attempts": self.evolution_attempts,
            "messages": self.ctx.messages(),
            "budget": self.budget,
            "last_heartbeat_at_ms": self.last_heartbeat_at_ms,
            "last_heartbeat_step": self.last_heartbeat_step,
            "last_snapshot_step": self.last_snapshot_step,
        })
    }

    /// Restore runtime state from a snapshot value. Unknown or missing keys
    /// fall back to safe defaults so older snapshots stay loadable.
    fn restore_from_value(&mut self, state: &Value) -> Result<()> {
        self.state = state
            .get("state")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or(AgentState::Idle);
        self.initial_prompt = state
            .get("initial_prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.plan = state
            .get("plan")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        self.plan_failed_reason = state
            .get("plan_failed_reason")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.replan_context = state
            .get("replan_context")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.evolution_attempts = state
            .get("evolution_attempts")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let messages: Vec<crate::llm::Message> = state
            .get("messages")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        self.ctx =
            Context::from_messages(messages, self.max_context_tokens, self.keep_recent_messages);
        if let Some(budget) = state
            .get("budget")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
        {
            self.budget = budget;
        }
        self.last_heartbeat_at_ms = state
            .get("last_heartbeat_at_ms")
            .and_then(Value::as_u64)
            .unwrap_or(crate::kernel::event_store::now_ms());
        self.last_heartbeat_step = state
            .get("last_heartbeat_step")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        self.last_snapshot_step = state
            .get("last_snapshot_step")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        self.termination = state
            .get("termination")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        Ok(())
    }

    /// Persist a snapshot when the step interval has been reached. A value of
    /// 0 means every step.
    pub fn maybe_snapshot(&mut self) -> Result<()> {
        let since_last = self
            .budget
            .steps_used
            .saturating_sub(self.last_snapshot_step);
        if self.snapshot_interval_steps == 0 || since_last >= self.snapshot_interval_steps {
            self.last_snapshot_step = self.budget.steps_used;
            self.save_snapshot()?;
        }
        Ok(())
    }

    /// Force a snapshot at the current event sequence.
    pub fn save_snapshot(&mut self) -> Result<()> {
        let seq = self.store.max_seq(&self.continuity_id)?;
        self.store
            .save_snapshot(&self.continuity_id, seq, &self.snapshot_value())
    }

    /// Emit a heartbeat when the interval elapsed. A value of 0 means every
    /// step.
    fn maybe_heartbeat(&mut self) -> Result<Option<EventType>> {
        let now = crate::kernel::event_store::now_ms();
        let elapsed = now.saturating_sub(self.last_heartbeat_at_ms);
        if self.heartbeat_interval_secs > 0 && elapsed < self.heartbeat_interval_secs * 1000 {
            return Ok(None);
        }
        let steps_since_last = self
            .budget
            .steps_used
            .saturating_sub(self.last_heartbeat_step);
        self.last_heartbeat_at_ms = now;
        self.last_heartbeat_step = self.budget.steps_used;
        Ok(Some(EventType::Heartbeat { steps_since_last }))
    }

    /// Record an error event with the current continuity context.
    pub fn record_error(&self, error: &str, recoverable: bool) -> Result<()> {
        self.store.append(
            &self.continuity_id,
            &EventType::Error {
                error: error.to_string(),
                recoverable,
            },
        )?;
        Ok(())
    }
}
