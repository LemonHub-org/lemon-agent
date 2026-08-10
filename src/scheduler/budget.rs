//! Hard budget limits that prevent runaway loops and unbounded cost.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Immutable budget limits reused when starting each continuity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetLimits {
    pub max_steps: usize,
    pub max_input_tokens: usize,
    pub max_llm_calls: usize,
    pub max_tool_calls: usize,
    pub max_wall_clock_secs: u64,
}

impl BudgetLimits {
    pub fn new(
        max_steps: usize,
        max_input_tokens: usize,
        max_llm_calls: usize,
        max_tool_calls: usize,
        max_wall_clock_secs: u64,
    ) -> BudgetLimits {
        BudgetLimits {
            max_steps,
            max_input_tokens,
            max_llm_calls,
            max_tool_calls,
            max_wall_clock_secs,
        }
    }

    /// A fresh budget with zeroed usage counters.
    pub fn new_budget(self, started_at_ms: u64) -> Budget {
        Budget::new(
            self.max_steps,
            self.max_input_tokens,
            self.max_llm_calls,
            self.max_tool_calls,
            self.max_wall_clock_secs,
            started_at_ms,
        )
    }
}

/// The budget and its live usage counters.
///
/// Usage counters are derived from persisted events on recovery, so a
/// snapshot never needs to be trusted for accounting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Budget {
    pub max_steps: usize,
    pub max_input_tokens: usize,
    pub max_llm_calls: usize,
    pub max_tool_calls: usize,
    pub max_wall_clock_secs: u64,
    pub started_at_ms: u64,
    pub steps_used: usize,
    pub input_tokens_used: usize,
    pub llm_calls_used: usize,
    pub tool_calls_used: usize,
}

impl Budget {
    pub fn new(
        max_steps: usize,
        max_input_tokens: usize,
        max_llm_calls: usize,
        max_tool_calls: usize,
        max_wall_clock_secs: u64,
        started_at_ms: u64,
    ) -> Budget {
        Budget {
            max_steps,
            max_input_tokens,
            max_llm_calls,
            max_tool_calls,
            max_wall_clock_secs,
            started_at_ms,
            steps_used: 0,
            input_tokens_used: 0,
            llm_calls_used: 0,
            tool_calls_used: 0,
        }
    }

    pub fn record_step(&mut self) {
        self.steps_used += 1;
    }

    /// Record an LLM call; returns the updated input token count.
    pub fn record_llm_call(&mut self, input_tokens: usize) {
        self.llm_calls_used += 1;
        self.input_tokens_used += input_tokens;
    }

    pub fn record_tool_call(&mut self) {
        self.tool_calls_used += 1;
    }

    /// Check all limits against the current time; fail with the first
    /// violated limit.
    pub fn check(&self, now_ms: u64) -> Result<()> {
        if self.steps_used >= self.max_steps {
            return Err(Error::BudgetExhausted(format!(
                "step limit reached: {} of {}",
                self.steps_used, self.max_steps
            )));
        }
        if self.input_tokens_used >= self.max_input_tokens {
            return Err(Error::BudgetExhausted(format!(
                "input token limit reached: {} of {}",
                self.input_tokens_used, self.max_input_tokens
            )));
        }
        if self.llm_calls_used >= self.max_llm_calls {
            return Err(Error::BudgetExhausted(format!(
                "LLM call limit reached: {} of {}",
                self.llm_calls_used, self.max_llm_calls
            )));
        }
        if self.tool_calls_used >= self.max_tool_calls {
            return Err(Error::BudgetExhausted(format!(
                "tool call limit reached: {} of {}",
                self.tool_calls_used, self.max_tool_calls
            )));
        }
        let elapsed = now_ms.saturating_sub(self.started_at_ms) / 1000;
        if elapsed >= self.max_wall_clock_secs {
            return Err(Error::BudgetExhausted(format!(
                "wall clock limit reached: {}s of {}s",
                elapsed, self.max_wall_clock_secs
            )));
        }
        Ok(())
    }

    /// A human-readable summary of usage versus limits.
    pub fn summary(&self, now_ms: u64) -> String {
        let elapsed = now_ms.saturating_sub(self.started_at_ms) / 1000;
        format!(
            "steps {}/{}; input_tokens {}/{}; llm_calls {}/{}; tool_calls {}/{}; wall_clock {}s/{}s",
            self.steps_used,
            self.max_steps,
            self.input_tokens_used,
            self.max_input_tokens,
            self.llm_calls_used,
            self.max_llm_calls,
            self.tool_calls_used,
            self.max_tool_calls,
            elapsed,
            self.max_wall_clock_secs
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> Budget {
        Budget::new(3, 100, 2, 4, 60, 0)
    }

    #[test]
    fn limits_are_enforced() {
        let mut b = budget();
        assert!(b.check(1000).is_ok());
        b.record_step();
        b.record_step();
        b.record_step();
        let err = b.check(1000).unwrap_err();
        assert!(err.to_string().contains("step limit"), "{err}");

        let mut b = budget();
        b.record_llm_call(50);
        b.record_llm_call(50);
        let err = b.check(1000).unwrap_err();
        assert!(err.to_string().contains("token limit"), "{err}");

        let mut b = budget();
        b.record_llm_call(1);
        b.record_llm_call(1);
        let err = b.check(1000).unwrap_err();
        assert!(err.to_string().contains("LLM call"), "{err}");

        let mut b = budget();
        b.record_tool_call();
        b.record_tool_call();
        b.record_tool_call();
        b.record_tool_call();
        let err = b.check(1000).unwrap_err();
        assert!(err.to_string().contains("tool call"), "{err}");

        let b = Budget::new(10, 1000, 10, 10, 5, 0);
        let err = b.check(6_000).unwrap_err();
        assert!(err.to_string().contains("wall clock"), "{err}");
    }

    #[test]
    fn records_accumulate() {
        let mut b = budget();
        b.record_llm_call(30);
        b.record_llm_call(20);
        b.record_step();
        assert_eq!(b.llm_calls_used, 2);
        assert_eq!(b.input_tokens_used, 50);
        assert_eq!(b.steps_used, 1);
    }

    #[test]
    fn summary_reports_usage() {
        let mut b = budget();
        b.record_step();
        let s = b.summary(10_000);
        assert!(s.contains("steps 1/3"), "{s}");
        assert!(s.contains("wall_clock 10s/60s"), "{s}");
    }
}
