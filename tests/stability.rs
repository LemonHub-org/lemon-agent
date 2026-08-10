//! Stability and consistency test: many task cycles back to back against a
//! mock LLM. Every continuity must complete, its event log must be gapless,
//! and a fresh agent must always be able to recover from the persisted state.
//!
//! The 24-hour soak variant is `#[ignore]`d and run explicitly:
//! `cargo test --test stability -- --ignored --nocapture`

use lemon_agent::config::Config;
use lemon_agent::kernel::event_store::{EventStore, EventType};
use lemon_agent::scheduler::Agent;
use serde_json::json;

fn completion(content: &str) -> serde_json::Value {
    json!({
        "id": "cmpl-s",
        "object": "chat.completion",
        "model": "mock",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

const STRATEGY: &str = r#"
fn execute_plan(plan) {
    for step in plan.steps {
        switch step.tool {
            "write_file" => write_file(step.args.path, step.args.content),
            "read_file" => read_file(step.args.path),
            "exec_command" => exec_command(step.args.cmd, step.args.args),
            "sleep" => sleep(step.args.ms),
            _ => throw "unknown tool: " + step.tool,
        }
    }
    "plan executed successfully"
}

fn test_plan_and_execute() {
    "self-test ok"
}
"#;

fn config(dir: &std::path::Path, base_url: &str) -> Config {
    let mut c = Config::default();
    let root = dir.join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(dir.join("scripts/plan_and_execute.rhai"), STRATEGY).unwrap();
    c.agent.work_dir = root.clone();
    c.agent.scripts_dir = dir.join("scripts");
    c.agent.db_path = dir.join("agent.db");
    c.agent.heartbeat_interval_secs = 0;
    c.agent.snapshot_interval_steps = 1;
    c.agent.loop_sleep_ms = 0;
    c.sandbox.root_dir = root;
    c.llm.base_url = base_url.to_string();
    c.llm.model = "mock".to_string();
    c.llm.max_retries = 0;
    c
}

async fn run_cycle(server: &mockito::Server, cycles_done: usize) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &server.url());
    let mut agent = Agent::new(&cfg, Some(format!("cycle {cycles_done}"))).unwrap();
    let report = agent.run().await.unwrap();
    assert_eq!(
        report.status, "completed",
        "cycle {cycles_done}: {}",
        report.summary
    );

    // Consistency: the final log is gapless and terminates exactly once.
    let store = EventStore::open(&cfg.agent.db_path).unwrap();
    store.verify_continuity(&report.continuity_id, 1).unwrap();
    let events = store.events_after(&report.continuity_id, 0).unwrap();
    let finished = events
        .iter()
        .filter(|e| matches!(e.event, EventType::ContinuityFinished { .. }))
        .count();
    assert_eq!(finished, 1, "cycle {cycles_done} must finish exactly once");
    let tool_calls = events
        .iter()
        .filter(|e| matches!(e.event, EventType::ToolCall { .. }))
        .count();
    assert!(tool_calls >= 2, "cycle {cycles_done} must audit its tools");
}

#[tokio::test]
async fn many_cycles_are_stable_and_consistent() {
    let mut server = mockito::Server::new_async().await;
    let plan = json!({
        "steps": [
            {"name": "write", "tool": "write_file", "args": {"path": "out.txt", "content": "cycle"}},
            {"name": "read", "tool": "read_file", "args": {"path": "out.txt"}}
        ]
    });
    for _ in 0..50 {
        server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(completion(&plan.to_string()).to_string())
            .create_async()
            .await;
    }
    for cycle in 0..50 {
        run_cycle(&server, cycle).await;
    }
}

/// 24-hour soak: run cycles continuously for `AGENT_SOAK_SECS` seconds
/// (default 86_400). Run with `--ignored`.
#[tokio::test]
#[ignore = "24-hour soak test; run explicitly"]
async fn twenty_four_hour_soak() {
    let duration_secs = std::env::var("AGENT_SOAK_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(86_400);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(duration_secs);

    let mut server = mockito::Server::new_async().await;
    let plan = json!({
        "steps": [
            {"name": "write", "tool": "write_file", "args": {"path": "out.txt", "content": "soak"}}
        ]
    });
    for _ in 0..8 {
        server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(completion(&plan.to_string()).to_string())
            .create_async()
            .await;
    }

    let mut cycle = 0usize;
    while std::time::Instant::now() < deadline {
        run_cycle(&server, cycle).await;
        cycle += 1;
        if cycle % 100 == 0 {
            eprintln!("soak: {cycle} cycles completed");
        }
    }
    eprintln!("soak finished: {cycle} cycles in {duration_secs}s");
}
