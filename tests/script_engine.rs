//! Integration tests for the Rhai script engine: loading, execution, error
//! propagation, hot reload, capability gating, and tool-call limits.

use std::sync::Arc;
use std::time::Duration;

use lemon_agent::evolution::script_engine::ScriptEngine;
use lemon_agent::kernel::capability::{Capability, CapabilitySet, Permissions, Resource};
use lemon_agent::kernel::event_store::{EventStore, EventType};
use lemon_agent::kernel::sandbox::Sandbox;
use lemon_agent::scheduler::plan::{Plan, PlanStep};
use serde_json::json;

const EXAMPLE: &str = r#"
fn execute_plan(plan) {
    log_info("executing plan with " + plan.steps.len + " steps");
    for step in plan.steps {
        switch step.tool {
            "write_file" => write_file(step.args.path, step.args.content),
            "read_file" => read_file(step.args.path),
            "exec_command" => exec_command(step.args.cmd, step.args.args),
            "git_add_commit" => git_add_commit(step.args.message),
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
    if exec_command("git", ["--version"]).exit_code == 0 {
        "git is available"
    } else {
        throw "git is unavailable"
    }
}
"#;

struct Fixture {
    _dir: tempfile::TempDir,
    scripts_dir: std::path::PathBuf,
    sandbox: Arc<Sandbox>,
    store: Arc<EventStore>,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    let scripts_dir = dir.path().join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let commands: std::collections::HashSet<String> = ["git", "cargo", "rustc", "python3", "ls"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let store = Arc::new(EventStore::open(&dir.path().join("audit.db")).unwrap());
    let sandbox = Arc::new(
        Sandbox::new(root, commands, Duration::from_secs(30), 1024 * 1024)
            .unwrap()
            .with_event_store(store.clone()),
    );
    sandbox.set_continuity("c1");
    Fixture {
        _dir: dir,
        scripts_dir,
        sandbox,
        store,
    }
}

fn engine(f: &Fixture) -> ScriptEngine {
    let engine = ScriptEngine::new(
        f.scripts_dir.clone(),
        CapabilitySet::full(),
        f.sandbox.clone(),
        None,
        Some(f.store.clone()),
    )
    .unwrap();
    engine.set_continuity("c1");
    engine
}

fn plan_with_steps(steps: Vec<PlanStep>) -> Plan {
    Plan {
        steps,
        verify: Some(lemon_agent::scheduler::plan::VerifyCommand {
            cmd: "git".to_string(),
            args: vec!["--version".to_string()],
        }),
    }
}

#[tokio::test]
async fn loads_and_executes_example_plan() {
    let f = fixture();
    std::fs::write(f.scripts_dir.join("plan_and_execute.rhai"), EXAMPLE).unwrap();
    let engine = engine(&f);

    let plan = plan_with_steps(vec![
        PlanStep {
            name: "write file".to_string(),
            tool: "write_file".to_string(),
            args: json!({"path": "out.txt", "content": "scripted"}),
        },
        PlanStep {
            name: "read file".to_string(),
            tool: "read_file".to_string(),
            args: json!({"path": "out.txt"}),
        },
    ]);
    let result = engine
        .execute_plan("plan_and_execute", &plan)
        .await
        .unwrap();
    assert_eq!(result, "plan executed successfully");
    assert_eq!(
        std::fs::read_to_string(f.sandbox.root_dir().join("out.txt")).unwrap(),
        "scripted"
    );
    assert_eq!(engine.script_names(), vec!["plan_and_execute"]);
}

#[tokio::test]
async fn script_errors_propagate() {
    let f = fixture();
    std::fs::write(
        f.scripts_dir.join("plan_and_execute.rhai"),
        r#"
fn execute_plan(plan) {
    throw "boom: " + plan.steps.len;
}
"#,
    )
    .unwrap();
    let engine = engine(&f);
    let plan = plan_with_steps(vec![]);
    let err = engine
        .execute_plan("plan_and_execute", &plan)
        .await
        .unwrap_err();
    assert!(err.contains("boom"), "{err}");
}

#[tokio::test]
async fn missing_entry_point_is_rejected() {
    let f = fixture();
    std::fs::write(f.scripts_dir.join("bad.rhai"), "fn other() { 42 }").unwrap();
    let engine = engine(&f);
    assert!(
        engine.script_names().is_empty(),
        "bad script must be rejected"
    );
}

#[tokio::test]
async fn invalid_script_never_breaks_runtime() {
    let f = fixture();
    std::fs::write(f.scripts_dir.join("plan_and_execute.rhai"), EXAMPLE).unwrap();
    std::fs::write(
        f.scripts_dir.join("broken.rhai"),
        "fn execute_plan(plan) { this is not rhai }",
    )
    .unwrap();
    let engine = engine(&f);
    assert_eq!(engine.script_names(), vec!["plan_and_execute"]);

    let plan = plan_with_steps(vec![]);
    let result = engine
        .execute_plan("plan_and_execute", &plan)
        .await
        .unwrap();
    assert_eq!(result, "plan executed successfully");
}

#[tokio::test]
async fn hot_reload_applies_changes() {
    let f = fixture();
    std::fs::write(
        f.scripts_dir.join("plan_and_execute.rhai"),
        "fn execute_plan(plan) { \"version-1\" }",
    )
    .unwrap();
    let engine = engine(&f);
    let plan = plan_with_steps(vec![]);
    assert_eq!(
        engine
            .execute_plan("plan_and_execute", &plan)
            .await
            .unwrap(),
        "version-1"
    );

    std::fs::write(
        f.scripts_dir.join("plan_and_execute.rhai"),
        "fn execute_plan(plan) { \"version-2\" }",
    )
    .unwrap();

    // The watcher applies changes asynchronously; poll until visible.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match engine.execute_plan("plan_and_execute", &plan).await {
            Ok(v) if v == "version-2" => break,
            Ok(v) => assert_eq!(v, "version-1", "unexpected version {v}"),
            Err(e) => panic!("{e}"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "hot reload never applied"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn broken_hot_reload_keeps_previous_version() {
    let f = fixture();
    std::fs::write(
        f.scripts_dir.join("plan_and_execute.rhai"),
        "fn execute_plan(plan) { \"version-1\" }",
    )
    .unwrap();
    let engine = engine(&f);
    let plan = plan_with_steps(vec![]);

    std::fs::write(
        f.scripts_dir.join("plan_and_execute.rhai"),
        "fn execute_plan(plan) { broken",
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let result = engine
        .execute_plan("plan_and_execute", &plan)
        .await
        .unwrap();
    assert_eq!(
        result, "version-1",
        "previous version must survive a bad reload"
    );
}

#[tokio::test]
async fn tool_call_limit_bounds_runaway_scripts() {
    let f = fixture();
    std::fs::write(
        f.scripts_dir.join("plan_and_execute.rhai"),
        r#"
fn execute_plan(plan) {
    for i in 0..200 {
        write_file("f" + i + ".txt", "x");
    }
    "done"
}
"#,
    )
    .unwrap();
    let engine = engine(&f);
    let plan = plan_with_steps(vec![]);
    let err = engine
        .execute_plan("plan_and_execute", &plan)
        .await
        .unwrap_err();
    assert!(err.contains("tool call limit"), "{err}");
}

#[tokio::test]
async fn capabilities_gate_script_tools() {
    let f = fixture();
    std::fs::write(
        f.scripts_dir.join("plan_and_execute.rhai"),
        "fn execute_plan(plan) { exec_command(\"git\", [\"--version\"]) }",
    )
    .unwrap();
    let read_only = CapabilitySet::from_capabilities(vec![Capability::new(
        Resource::FileSystem,
        Permissions::read(),
    )]);
    let engine = ScriptEngine::new(
        f.scripts_dir.clone(),
        read_only,
        f.sandbox.clone(),
        None,
        None,
    )
    .unwrap();
    let plan = plan_with_steps(vec![]);
    let err = engine
        .execute_plan("plan_and_execute", &plan)
        .await
        .unwrap_err();
    assert!(err.contains("E006"), "{err}");
}

#[tokio::test]
async fn test_entry_point_runs_when_present() {
    let f = fixture();
    std::fs::write(f.scripts_dir.join("plan_and_execute.rhai"), EXAMPLE).unwrap();
    let engine = engine(&f);
    let result = engine.run_test("plan_and_execute").await.unwrap();
    assert!(result.is_some());
    assert!(result.unwrap().contains("git is available"));

    // A script without a test entry returns None.
    std::fs::write(
        f.scripts_dir.join("plain.rhai"),
        "fn execute_plan(plan) { \"ok\" }",
    )
    .unwrap();
    let engine2 = ScriptEngine::new(
        f.scripts_dir.clone(),
        CapabilitySet::full(),
        f.sandbox.clone(),
        None,
        None,
    )
    .unwrap();
    assert!(engine2.run_test("plain").await.unwrap().is_none());
}

#[tokio::test]
async fn script_tool_calls_are_audited() {
    let f = fixture();
    std::fs::write(
        f.scripts_dir.join("plan_and_execute.rhai"),
        r#"
fn execute_plan(plan) {
    write_file("a.txt", "hello from script");
    read_file("a.txt");
    "ok"
}
"#,
    )
    .unwrap();
    let engine = engine(&f);
    let plan = plan_with_steps(vec![]);
    engine
        .execute_plan("plan_and_execute", &plan)
        .await
        .unwrap();

    let events = f.store.events_after("c1", 0).unwrap();
    let tools: Vec<&EventType> = events
        .iter()
        .map(|e| &e.event)
        .filter(|e| matches!(e, EventType::ToolCall { .. }))
        .collect();
    assert_eq!(tools.len(), 2);
    match tools[0] {
        EventType::ToolCall { tool_name, .. } => assert_eq!(tool_name, "write_file"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn reload_replaces_single_script() {
    let f = fixture();
    std::fs::write(
        f.scripts_dir.join("plan_and_execute.rhai"),
        "fn execute_plan(plan) { \"before-reload\" }",
    )
    .unwrap();
    let engine = engine(&f);
    std::fs::write(
        f.scripts_dir.join("plan_and_execute.rhai"),
        "fn execute_plan(plan) { \"after-reload\" }",
    )
    .unwrap();
    engine.reload("plan_and_execute").unwrap();
    let plan = plan_with_steps(vec![]);
    let result = engine
        .execute_plan("plan_and_execute", &plan)
        .await
        .unwrap();
    assert_eq!(result, "after-reload");

    let err = engine.reload("does_not_exist").unwrap_err();
    assert_eq!(err.code(), lemon_agent::error::ErrorCode::FileNotFound);
}
