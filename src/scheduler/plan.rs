//! Plan representation and parsing.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

/// The known tool names a plan may reference.
pub const KNOWN_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "append_file",
    "list_dir",
    "search_code",
    "exec_command",
    "git_add_commit",
    "llm_query",
    "sleep",
];

/// One executable plan step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanStep {
    pub name: String,
    pub tool: String,
    pub args: Value,
}

/// A verification command run after all steps complete.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerifyCommand {
    pub cmd: String,
    pub args: Vec<String>,
}

/// An agent plan: ordered steps plus an optional verification command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    pub steps: Vec<PlanStep>,
    #[serde(default)]
    pub verify: Option<VerifyCommand>,
}

impl Plan {
    /// Parse a plan from LLM content, tolerating markdown fences.
    pub fn parse(content: &str) -> Result<Plan> {
        let json_text = extract_json(content).ok_or_else(|| {
            Error::InvalidInput("plan does not contain a JSON object".to_string())
        })?;
        let plan: Plan = serde_json::from_str(&json_text)
            .map_err(|e| Error::InvalidInput(format!("invalid plan JSON: {e}")))?;
        plan.validate()?;
        Ok(plan)
    }

    /// Validate the plan structure: steps exist, tools are known, args are
    /// objects, and verification uses a whitelisted command shape.
    pub fn validate(&self) -> Result<()> {
        if self.steps.is_empty() {
            return Err(Error::InvalidInput("plan has no steps".to_string()));
        }
        if self.steps.len() > 100 {
            return Err(Error::InvalidInput(format!(
                "plan has too many steps: {}",
                self.steps.len()
            )));
        }
        for step in &self.steps {
            if step.name.trim().is_empty() {
                return Err(Error::InvalidInput("plan step has no name".to_string()));
            }
            if !KNOWN_TOOLS.contains(&step.tool.as_str()) {
                return Err(Error::InvalidInput(format!(
                    "plan step {:?} uses unknown tool {:?}",
                    step.name, step.tool
                )));
            }
            if !step.args.is_object() {
                return Err(Error::InvalidInput(format!(
                    "plan step {:?} has non-object args",
                    step.name
                )));
            }
        }
        if let Some(verify) = &self.verify
            && verify.cmd.trim().is_empty()
        {
            return Err(Error::InvalidInput("verify command is empty".to_string()));
        }
        Ok(())
    }
}

/// Extract the first balanced JSON object from arbitrary text.
fn extract_json(content: &str) -> Option<String> {
    let start = content.find('{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (idx, ch) in content[start..].char_indices() {
        match ch {
            '"' if !escaped => in_string = !in_string,
            '\\' if in_string && !escaped => escaped = true,
            '\\' if in_string => escaped = false,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    let end = start + idx + 1;
                    return Some(content[start..end].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_valid_plan() {
        let content = r#"
        Here is my plan:
        ```json
        {"steps": [
            {"name": "write code", "tool": "write_file", "args": {"path": "a.rs", "content": "fn main() {}"}},
            {"name": "run tests", "tool": "exec_command", "args": {"cmd": "cargo", "args": ["test"]}}
        ], "verify": {"cmd": "cargo", "args": ["test"]}}
        ```
        "#;
        let plan = Plan::parse(content).unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].tool, "write_file");
        assert_eq!(plan.verify.as_ref().unwrap().cmd, "cargo");
    }

    #[test]
    fn rejects_unknown_tools() {
        let content = r#"{"steps": [{"name": "evil", "tool": "rm_rf", "args": {}}]}"#;
        let err = Plan::parse(content).unwrap_err();
        assert!(err.to_string().contains("unknown tool"), "{err}");
    }

    #[test]
    fn rejects_empty_plan() {
        assert!(Plan::parse(r#"{"steps": []}"#).is_err());
        assert!(Plan::parse("no json here").is_err());
    }

    #[test]
    fn rejects_malformed_json() {
        let err = Plan::parse(r#"{"steps": [{"name": "x"}"#).unwrap_err();
        assert!(err.to_string().contains("plan"), "{err}");
        assert!(err.to_string().contains("JSON object"), "{err}");
    }

    #[test]
    fn accepts_plan_without_verify() {
        let plan = Plan::parse(r#"{"steps": [{"name": "s", "tool": "sleep", "args": {"ms": 1}}]}"#)
            .unwrap();
        assert!(plan.verify.is_none());
    }

    #[test]
    fn extracts_json_from_noisy_text() {
        let content = "prefix { \"a\": { \"b\": \"c}\" } } suffix";
        let extracted = extract_json(content).unwrap();
        assert_eq!(extracted, "{ \"a\": { \"b\": \"c}\" } }");
    }

    #[test]
    fn rejects_overlong_plans() {
        let steps: Vec<Value> = (0..101)
            .map(|i| json!({"name": format!("s{i}"), "tool": "sleep", "args": {"ms": 1}}))
            .collect();
        let plan = Plan {
            steps: serde_json::from_value(json!(steps)).unwrap(),
            verify: None,
        };
        assert!(plan.validate().is_err());
    }
}
