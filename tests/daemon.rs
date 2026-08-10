//! Integration test for the daemon loop: multiple tasks submitted through a
//! channel are processed one after another, each as its own continuity, with
//! live state published between transitions.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lemon_agent::config::Config;
use lemon_agent::kernel::event_store::{EventStore, EventType};
use lemon_agent::scheduler::{Agent, LiveState};
use serde_json::json;
use tokio::sync::{mpsc, watch};

const STRATEGY: &str = r#"
fn execute_plan(plan) {
    for step in plan.steps {
        switch step.tool {
            "write_file" => write_file(step.args.path, step.args.content),
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

fn completion(content: &str) -> serde_json::Value {
    json!({
        "id": "cmpl-d",
        "object": "chat.completion",
        "model": "mock",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

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
    c.agent.snapshot_interval_steps = 0;
    c.agent.loop_sleep_ms = 0;
    c.sandbox.root_dir = root;
    c.llm.base_url = base_url.to_string();
    c.llm.model = "mock".to_string();
    c.llm.max_retries = 0;
    c
}

/// Wait until `predicate` holds, polling every 20ms for up to 10s.
async fn wait_until<F: FnMut() -> bool>(mut predicate: F) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn daemon_processes_multiple_tasks_as_separate_continuities() {
    let mut server = mockito::Server::new_async().await;
    let plan = json!({
        "steps": [
            {"name": "write", "tool": "write_file", "args": {"path": "out.txt", "content": "daemon"}}
        ]
    });
    for _ in 0..4 {
        server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(completion(&plan.to_string()).to_string())
            .create_async()
            .await;
    }

    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &server.url());

    let live = Arc::new(Mutex::new(LiveState::default()));
    let (task_tx, task_rx) = mpsc::channel::<String>(16);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let mut agent = Agent::new(&cfg, None).unwrap();
    let observer = live.clone();
    let observer_closure = live.clone();
    let result_holder: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let result_writer = result_holder.clone();
    let daemon = tokio::spawn(async move {
        let result = agent
            .run_daemon(task_rx, shutdown_rx, move |snapshot| {
                *observer_closure.lock().unwrap_or_else(|p| p.into_inner()) = snapshot.clone();
            })
            .await;
        let live = observer.lock().unwrap_or_else(|p| p.into_inner());
        *result_writer.lock().unwrap_or_else(|p| p.into_inner()) = Some(format!(
            "result={result:?} live_state={:?} live_continuity={:?} live_error={:?}",
            live.state, live.continuity_id, live.last_error
        ));
    });

    // Task 1.
    task_tx.send("first task".to_string()).await.unwrap();
    let first_done = wait_until(|| {
        live.lock()
            .unwrap_or_else(|p| p.into_inner())
            .report
            .as_ref()
            .is_some_and(|r| r.status == "completed")
    })
    .await;
    assert!(
        first_done,
        "first task never completed; daemon result: {:?}",
        result_holder.lock().unwrap_or_else(|p| p.into_inner())
    );
    let first_id = live
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .report
        .as_ref()
        .unwrap()
        .continuity_id
        .clone();

    // Task 2 must produce a different continuity.
    task_tx.send("second task".to_string()).await.unwrap();
    let second_done = wait_until(|| {
        live.lock()
            .unwrap_or_else(|p| p.into_inner())
            .report
            .as_ref()
            .is_some_and(|r| r.continuity_id != first_id && r.status == "completed")
    })
    .await;
    assert!(second_done, "second task never completed");

    shutdown_tx.send(true).ok();
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;

    let store = EventStore::open(&cfg.agent.db_path).unwrap();
    let summaries = store.continuity_summaries().unwrap();
    let ids: Vec<&str> = summaries.iter().map(|s| s.continuity_id.as_str()).collect();
    assert_eq!(
        summaries.len(),
        2,
        "two tasks must produce two continuities, got {ids:?}"
    );
    assert!(ids.contains(&first_id.as_str()));
    assert!(
        summaries.iter().all(|s| s.finished),
        "both must be finished"
    );

    let events = store.events_after(&first_id, 0).unwrap();
    assert!(matches!(
        events.last().unwrap().event,
        EventType::ContinuityFinished { .. }
    ));
}

#[tokio::test]
async fn daemon_waits_for_tasks_and_exits_on_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), "http://127.0.0.1:9"); // no LLM calls expected
    let live = Arc::new(Mutex::new(LiveState::default()));
    let (task_tx, task_rx) = mpsc::channel::<String>(4);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let mut agent = Agent::new(&cfg, None).unwrap();
    let observer = live.clone();
    let daemon = tokio::spawn(async move {
        agent
            .run_daemon(task_rx, shutdown_rx, move |snapshot| {
                *observer.lock().unwrap_or_else(|p| p.into_inner()) = snapshot.clone();
            })
            .await
            .unwrap();
    });

    // Idle with an open channel: the daemon must stay alive.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!daemon.is_finished(), "daemon must wait for tasks");

    // Closing the channel is a clean shutdown signal too.
    drop(task_tx);
    let _ = tokio::time::timeout(Duration::from_secs(5), daemon)
        .await
        .expect("daemon must exit when the task channel closes");
    shutdown_tx.send(true).ok();
}
