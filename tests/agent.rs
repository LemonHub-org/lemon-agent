//! End-to-end tests of the scheduler loop against a mock LLM: task
//! completion, script-driven execution, evolution fixes, rollback, budget
//! termination, and crash-recovery resume.

use lemon_agent::config::Config;
use lemon_agent::kernel::event_store::{EventStore, EventType, now_ms};
use lemon_agent::scheduler::{Agent, Budget};
use serde_json::json;

/// The strategy script the agent executes plans with. This one throws for
/// every plan, simulating an injected defect that evolution must fix.
const DEFECTIVE_SCRIPT: &str = r#"
fn execute_plan(plan) {
    throw "script bug: plan execution not implemented";
}
"#;

/// A working dispatcher strategy, the kind evolution produces as a fix.
const WORKING_SCRIPT: &str = r#"
fn execute_plan(plan) {
    for step in plan.steps {
        switch step.tool {
            "read_file" => read_file(step.args.path),
            "write_file" => write_file(step.args.path, step.args.content),
            "append_file" => append_file(step.args.path, step.args.content),
            "list_dir" => list_dir(step.args.path),
            "search_code" => search_code(step.args.query, step.args.path),
            "exec_command" => exec_command(step.args.cmd, step.args.args),
            "git_add_commit" => git_add_commit(step.args.message),
            "llm_query" => llm_query(step.args.prompt),
            "sleep" => sleep(step.args.ms),
            _ => throw "unknown tool in plan step: " + step.tool,
        }
    }
    if "verify" in plan {
        let out = exec_command(plan.verify.cmd, plan.verify.args);
        if out.exit_code != 0 {
            throw "verification failed (exit " + out.exit_code + "): " + out.stderr;
        }
    }
    "plan executed successfully"
}

fn test_plan_and_execute() {
    let out = exec_command("git", ["--version"]);
    if out.exit_code == 0 {
        "git is available"
    } else {
        throw "git is unavailable"
    }
}
"#;

fn config(root: &std::path::Path, db: &std::path::Path, base_url: &str) -> Config {
    let mut c = Config::default();
    c.agent.work_dir = root.to_path_buf();
    c.agent.scripts_dir = root.parent().unwrap().join("scripts");
    c.agent.db_path = db.to_path_buf();
    c.agent.heartbeat_interval_secs = 0; // heartbeat on every step
    c.agent.snapshot_interval_steps = 1; // snapshot every step
    c.agent.loop_sleep_ms = 0;
    c.sandbox.root_dir = root.to_path_buf();
    c.llm.base_url = base_url.to_string();
    c.llm.model = "mock-model".to_string();
    c.llm.max_retries = 0;
    c
}

/// A mock chat completion whose content is `content`.
fn completion(content: &str) -> serde_json::Value {
    json!({
        "id": "cmpl-test",
        "object": "chat.completion",
        "model": "mock-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

fn fixture(base_url: &str) -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
    let config = config(&root, &dir.path().join("agent.db"), base_url);
    (dir, config)
}

fn write_strategy(config: &Config, source: &str) {
    let path = config.agent.scripts_dir.join("plan_and_execute.rhai");
    std::fs::create_dir_all(config.agent.scripts_dir.as_path()).unwrap();
    std::fs::write(path, source).unwrap();
}

#[tokio::test]
async fn completes_task_end_to_end() {
    let mut server = mockito::Server::new_async().await;
    let plan = json!({
        "steps": [
            {"name": "write hello file", "tool": "write_file", "args": {"path": "hello.txt", "content": "Hello, Lemon!"}},
            {"name": "check git availability", "tool": "exec_command", "args": {"cmd": "git", "args": ["--version"]}}
        ],
        "verify": {"cmd": "git", "args": ["--version"]}
    });
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion(&plan.to_string()).to_string())
        .create_async()
        .await;

    let (dir, config) = fixture(&server.url());
    write_strategy(&config, WORKING_SCRIPT);
    let mut agent = Agent::new(&config, Some("write a hello file".to_string())).unwrap();
    let report = agent.run().await.unwrap();

    assert_eq!(report.status, "completed", "{}", report.summary);
    assert_eq!(report.steps_used, 3); // planning, executing, evaluating

    let written = std::fs::read_to_string(dir.path().join("workspace/hello.txt")).unwrap();
    assert_eq!(written, "Hello, Lemon!");

    let store = EventStore::open(&config.agent.db_path).unwrap();
    let events = store.events_after(&report.continuity_id, 0).unwrap();
    let names: Vec<&str> = events.iter().map(|e| e.event.name()).collect();
    assert!(names.contains(&"ContinuityStarted"), "{names:?}");
    assert_eq!(names.last().copied(), Some("ContinuityFinished"));
    assert!(names.contains(&"Heartbeat"), "missing heartbeat: {names:?}");
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event.name() == "ToolCall")
            .count(),
        3 // write_file, exec_command, verify
    );
    match events.last().unwrap().event.clone() {
        EventType::ContinuityFinished { status, .. } => assert_eq!(status, "completed"),
        other => panic!("unexpected final event: {other:?}"),
    }
}

#[tokio::test]
async fn injected_script_defect_is_fixed_by_evolution() {
    let mut server = mockito::Server::new_async().await;
    let plan = json!({
        "steps": [
            {"name": "write hello file", "tool": "write_file", "args": {"path": "hello.txt", "content": "Evolved"}}
        ]
    });
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion(&plan.to_string()).to_string())
        .create_async()
        .await;
    // Mocks are served in creation order: first the plan, then the evolution
    // candidate script.
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion(WORKING_SCRIPT).to_string())
        .create_async()
        .await;

    let (dir, config) = fixture(&server.url());
    write_strategy(&config, DEFECTIVE_SCRIPT);
    let mut agent = Agent::new(&config, Some("fix my strategy".to_string())).unwrap();
    let report = agent.run().await.unwrap();

    assert_eq!(report.status, "completed", "{}", report.summary);
    assert!(agent.evolution_attempts >= 1, "evolution never triggered");
    assert_eq!(
        std::fs::read_to_string(config.agent.scripts_dir.join("plan_and_execute.rhai")).unwrap(),
        WORKING_SCRIPT.trim(),
        "fixed script must replace the defective one"
    );
    assert!(
        std::fs::read_to_string(dir.path().join("workspace/hello.txt")).is_ok(),
        "fixed strategy must execute the plan"
    );

    let store = EventStore::open(&config.agent.db_path).unwrap();
    let events = store.events_after(&report.continuity_id, 0).unwrap();
    let attempts = events
        .iter()
        .filter(|e| matches!(e.event, EventType::EvolutionAttempt { .. }))
        .count();
    assert_eq!(attempts, 1);
    let results = events
        .iter()
        .filter_map(|e| match &e.event {
            EventType::EvolutionResult { success, .. } => Some(*success),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results, vec![true]);
    match &events
        .iter()
        .find(|e| matches!(e.event, EventType::EvolutionAttempt { .. }))
        .unwrap()
        .event
    {
        EventType::EvolutionAttempt {
            old_hash, new_hash, ..
        } => {
            assert_ne!(old_hash, new_hash);
            assert!(!old_hash.starts_with("<missing>"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn bad_candidate_rolls_back_and_fails() {
    let mut server = mockito::Server::new_async().await;
    let plan = json!({
        "steps": [
            {"name": "write tmp", "tool": "write_file", "args": {"path": "tmp.txt", "content": "x"}}
        ]
    });
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion(&plan.to_string()).to_string())
        .create_async()
        .await;
    // The evolution candidate is not valid Rhai.
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion("fn execute_plan(plan) { this is not rhai").to_string())
        .create_async()
        .await;

    let (dir, mut config) = fixture(&server.url());
    config.agent.max_evolution_attempts = 1;
    write_strategy(&config, DEFECTIVE_SCRIPT);
    let mut agent = Agent::new(&config, Some("doomed task".to_string())).unwrap();
    let report = agent.run().await.unwrap();

    assert_eq!(report.status, "failed", "{}", report.summary);
    assert!(
        report.summary.contains("evolution failed"),
        "{}",
        report.summary
    );
    assert_eq!(
        std::fs::read_to_string(config.agent.scripts_dir.join("plan_and_execute.rhai")).unwrap(),
        DEFECTIVE_SCRIPT,
        "failed candidate must roll back to the original"
    );
    assert!(
        !config
            .agent
            .scripts_dir
            .join("plan_and_execute.rhai.bak")
            .exists(),
        "no backup file may remain after rollback"
    );

    let store = EventStore::open(&config.agent.db_path).unwrap();
    let events = store.events_after(&report.continuity_id, 0).unwrap();
    let results = events
        .iter()
        .filter_map(|e| match &e.event {
            EventType::EvolutionResult { success, .. } => Some(*success),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results, vec![false]);
    let _ = dir;
}

#[tokio::test]
async fn budget_exhaustion_terminates_safely() {
    let mut server = mockito::Server::new_async().await;
    let plan = json!({
        "steps": [
            {"name": "write hello file", "tool": "write_file", "args": {"path": "hello.txt", "content": "Hi"}}
        ]
    });
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion(&plan.to_string()).to_string())
        .create_async()
        .await;

    let (dir, mut config) = fixture(&server.url());
    config.agent.max_steps = 2;
    write_strategy(&config, WORKING_SCRIPT);
    let mut agent = Agent::new(&config, Some("short task".to_string())).unwrap();
    let report = agent.run().await.unwrap();

    assert_eq!(report.status, "budget_exhausted", "{}", report.summary);
    assert!(report.summary.contains("step limit"), "{}", report.summary);

    let store = EventStore::open(&config.agent.db_path).unwrap();
    let events = store.events_after(&report.continuity_id, 0).unwrap();
    match events.last().unwrap().event.clone() {
        EventType::ContinuityFinished { status, .. } => {
            assert_eq!(status, "budget_exhausted")
        }
        other => panic!("unexpected final event: {other:?}"),
    }
    let _ = dir;
}

#[tokio::test]
async fn resumes_incomplete_continuity() {
    let mut server = mockito::Server::new_async().await;
    let plan = json!({
        "steps": [
            {"name": "write hello file", "tool": "write_file", "args": {"path": "hello.txt", "content": "Resumed"}}
        ]
    });
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion(&plan.to_string()).to_string())
        .create_async()
        .await;

    let (dir, config) = fixture(&server.url());
    write_strategy(&config, WORKING_SCRIPT);

    // Pre-seed an interrupted continuity: started, one step, snapshot in
    // Planning with one step already used.
    let store = EventStore::open(&config.agent.db_path).unwrap();
    store
        .append(
            "cont-resume",
            &EventType::ContinuityStarted {
                initial_prompt: "resume me".to_string(),
            },
        )
        .unwrap();
    store
        .append("cont-resume", &EventType::StepStarted { step_num: 1 })
        .unwrap();
    let mut budget = Budget::new(200, 100_000, 50, 200, 86_400, now_ms());
    budget.record_step();
    let state = json!({
        "state": "planning",
        "initial_prompt": "resume me",
        "plan": null,
        "plan_failed_reason": null,
        "evolution_attempts": 0,
        "messages": [],
        "budget": budget,
        "last_heartbeat_at_ms": now_ms(),
        "last_heartbeat_step": 1,
        "last_snapshot_step": 1
    });
    store.save_snapshot("cont-resume", 2, &state).unwrap();

    let mut agent = Agent::new(&config, Some("ignored new task".to_string())).unwrap();
    assert_eq!(agent.continuity_id, "cont-resume");
    assert_eq!(agent.budget.steps_used, 1, "replay must recount steps");
    let report = agent.run().await.unwrap();

    assert_eq!(report.continuity_id, "cont-resume");
    assert_eq!(report.status, "completed", "{}", report.summary);
    assert!(
        report.steps_used >= 3,
        "expected resumed steps: {}",
        report.steps_used
    );
    assert!(
        std::fs::read_to_string(dir.path().join("workspace/hello.txt")).is_ok(),
        "resumed task must write its file"
    );

    // The new step must be numbered after the replayed one.
    let events = store.events_after("cont-resume", 2).unwrap();
    let new_steps: Vec<u64> = events
        .iter()
        .filter_map(|e| match &e.event {
            EventType::StepStarted { step_num } => Some(*step_num as u64),
            _ => None,
        })
        .collect();
    assert!(!new_steps.is_empty());
    assert_eq!(new_steps[0], 2, "resumed step numbering off: {new_steps:?}");
    assert!(matches!(
        events.last().unwrap().event,
        EventType::ContinuityFinished { .. }
    ));
}

#[tokio::test]
async fn idle_without_task_creates_no_artifacts() {
    let (dir, config) = fixture("http://127.0.0.1:9");
    let mut agent = Agent::new(&config, None).unwrap();
    let report = agent.run().await.unwrap();
    assert_eq!(report.status, "idle");

    let store = EventStore::open(&config.agent.db_path).unwrap();
    assert!(store.incomplete_continuities().unwrap().is_empty());
    let _ = dir;
}

#[tokio::test]
async fn malformed_plan_is_retried_once_then_fails() {
    let mut server = mockito::Server::new_async().await;
    let bad_plan = "this is not a plan at all";
    let also_bad = "neither is this";
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion(bad_plan).to_string())
        .create_async()
        .await;
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion(also_bad).to_string())
        .create_async()
        .await;

    let (dir, config) = fixture(&server.url());
    let mut agent = Agent::new(&config, Some("unplannable".to_string())).unwrap();
    let report = agent.run().await.unwrap();
    assert_eq!(report.status, "failed", "{}", report.summary);
    assert!(
        report
            .summary
            .contains("planning failed after two attempts"),
        "{}",
        report.summary
    );
    let _ = dir;
}

#[tokio::test]
async fn missing_strategy_script_evolves_one_into_existence() {
    let mut server = mockito::Server::new_async().await;
    let plan = json!({
        "steps": [
            {"name": "noop", "tool": "sleep", "args": {"ms": 1}}
        ]
    });
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion(&plan.to_string()).to_string())
        .create_async()
        .await;
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion(WORKING_SCRIPT).to_string())
        .create_async()
        .await;

    // No strategy script on disk: execution fails, evolution creates one.
    let (dir, config) = fixture(&server.url());
    let mut agent = Agent::new(&config, Some("create strategy".to_string())).unwrap();
    let report = agent.run().await.unwrap();

    assert_eq!(report.status, "completed", "{}", report.summary);
    assert!(
        config
            .agent
            .scripts_dir
            .join("plan_and_execute.rhai")
            .exists(),
        "evolution must create the missing strategy script"
    );
    let _ = dir;
}

#[tokio::test]
async fn budget_boundary_single_step_terminates() {
    let mut server = mockito::Server::new_async().await;
    let plan = json!({
        "steps": [
            {"name": "write", "tool": "write_file", "args": {"path": "a.txt", "content": "x"}}
        ]
    });
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion(&plan.to_string()).to_string())
        .create_async()
        .await;

    let (dir, mut config) = fixture(&server.url());
    config.agent.max_steps = 1;
    write_strategy(&config, WORKING_SCRIPT);
    let mut agent = Agent::new(&config, Some("one step only".to_string())).unwrap();
    let report = agent.run().await.unwrap();
    assert_eq!(report.status, "budget_exhausted", "{}", report.summary);
    assert_eq!(report.steps_used, 1);
    let _ = dir;
}

#[tokio::test]
async fn resumes_continuity_without_snapshot_from_started_event() {
    let mut server = mockito::Server::new_async().await;
    let plan = json!({
        "steps": [
            {"name": "write", "tool": "write_file", "args": {"path": "a.txt", "content": "recovered"}}
        ]
    });
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion(&plan.to_string()).to_string())
        .create_async()
        .await;

    let (dir, config) = fixture(&server.url());
    write_strategy(&config, WORKING_SCRIPT);

    // Crash right after the continuity started, before any snapshot existed.
    let store = EventStore::open(&config.agent.db_path).unwrap();
    store
        .append(
            "cont-nosnap",
            &EventType::ContinuityStarted {
                initial_prompt: "crashed task".to_string(),
            },
        )
        .unwrap();
    drop(store);

    let mut agent = Agent::new(&config, Some("ignored".to_string())).unwrap();
    assert_eq!(agent.continuity_id, "cont-nosnap");
    assert_eq!(
        agent.initial_prompt, "crashed task",
        "prompt must be recovered"
    );
    let report = agent.run().await.unwrap();
    assert_eq!(report.continuity_id, "cont-nosnap");
    assert_eq!(report.status, "completed", "{}", report.summary);
    assert!(
        std::fs::read_to_string(dir.path().join("workspace/a.txt")).is_ok(),
        "restarted task must complete its plan"
    );
}

#[test]
fn plan_validation_rejects_unknown_tools() {
    let content = r#"{"steps": [{"name": "evil", "tool": "delete_everything", "args": {}}]}"#;
    let err = lemon_agent::scheduler::plan::Plan::parse(content).unwrap_err();
    assert!(err.to_string().contains("unknown tool"), "{err}");
}
