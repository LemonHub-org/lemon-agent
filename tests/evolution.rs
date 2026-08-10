//! Tests for the evolution engine: candidate generation, static checks,
//! isolated validation, application with audit events, and rollback.

use std::sync::Arc;

use lemon_agent::evolution::script_engine::ScriptEngine;
use lemon_agent::evolution::{
    EvolutionContext, EvolutionEngine, EvolutionOutcome, ValidationSpec, sha256_hex,
};
use lemon_agent::kernel::capability::CapabilitySet;
use lemon_agent::kernel::event_store::{EventStore, EventType};
use lemon_agent::kernel::sandbox::Sandbox;
use serde_json::json;

const DEFECTIVE: &str = r#"
fn execute_plan(plan) {
    throw "script bug: cannot execute plans";
}
"#;

const FIXED: &str = r#"
fn execute_plan(plan) {
    "ok"
}

fn test_plan_and_execute() {
    "self-test ok"
}
"#;

struct Fixture {
    _dir: tempfile::TempDir,
    scripts_dir: std::path::PathBuf,
    store: Arc<EventStore>,
    engine: Arc<ScriptEngine>,
    evolution: EvolutionEngine,
}

fn fixture(server_url: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let scripts_dir = dir.path().join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let mut cfg = lemon_agent::config::LlmConfig::default();
    cfg.base_url = server_url.to_string();
    cfg.model = "mock".to_string();
    cfg.max_retries = 0;
    let llm = Arc::new(lemon_agent::llm::LLMClient::new(&cfg).unwrap());

    let store = Arc::new(EventStore::open(&dir.path().join("audit.db")).unwrap());
    let allowed: std::collections::HashSet<String> = ["git", "cargo", "rustc", "python3", "ls"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let sandbox = Arc::new(
        Sandbox::new(
            workspace,
            allowed,
            std::time::Duration::from_secs(30),
            1024 * 1024,
        )
        .unwrap()
        .with_event_store(store.clone()),
    );
    sandbox.set_continuity("c1");
    let engine = Arc::new(
        ScriptEngine::new(
            scripts_dir.clone(),
            CapabilitySet::full(),
            sandbox,
            Some(llm.clone()),
            Some(store.clone()),
        )
        .unwrap(),
    );
    let evolution = EvolutionEngine::new(llm, store.clone(), engine.clone(), 3);
    Fixture {
        _dir: dir,
        scripts_dir,
        store,
        engine,
        evolution,
    }
}

fn completion(content: &str) -> serde_json::Value {
    json!({
        "id": "cmpl-e",
        "object": "chat.completion",
        "model": "mock",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

fn context(f: &Fixture) -> EvolutionContext {
    EvolutionContext {
        continuity_id: "c1".to_string(),
        step_num: 4,
        error: "script bug: cannot execute plans".to_string(),
        script_name: "plan_and_execute".to_string(),
        script_source: std::fs::read_to_string(f.scripts_dir.join("plan_and_execute.rhai"))
            .unwrap_or_default(),
        recent_messages: vec![lemon_agent::llm::Message::user("do the thing")],
        recent_tool_results: vec!["write_file({\"path\": \"a.txt\"}) -> ERROR: boom".to_string()],
        validation: ValidationSpec::TestEntry,
    }
}

#[tokio::test]
async fn accepted_candidate_replaces_script_and_audits() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion(FIXED).to_string())
        .create_async()
        .await;

    let mut f = fixture(&server.url());
    std::fs::write(f.scripts_dir.join("plan_and_execute.rhai"), DEFECTIVE).unwrap();
    f.engine.reload("plan_and_execute").unwrap();

    let old_hash = sha256_hex(DEFECTIVE.as_bytes());
    let outcome = f.evolution.attempt(&context(&f)).await.unwrap();
    match outcome {
        EvolutionOutcome::Fixed {
            old_hash: oh,
            new_hash: nh,
            ..
        } => {
            assert_eq!(oh, old_hash);
            assert_eq!(nh, sha256_hex(FIXED.trim().as_bytes()));
        }
        other => panic!("expected Fixed, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(f.scripts_dir.join("plan_and_execute.rhai")).unwrap(),
        FIXED.trim()
    );
    assert!(
        !f.scripts_dir.join("plan_and_execute.rhai.bak").exists(),
        "backup must be consumed on success"
    );

    let events = f.store.events_after("c1", 0).unwrap();
    let names: Vec<&str> = events.iter().map(|e| e.event.name()).collect();
    assert_eq!(names, vec!["EvolutionAttempt", "EvolutionResult"]);
    match &events[0].event {
        EventType::EvolutionAttempt {
            script_path,
            old_hash,
            new_hash,
        } => {
            assert!(script_path.ends_with("plan_and_execute.rhai"));
            assert_ne!(old_hash, new_hash);
        }
        other => panic!("unexpected: {other:?}"),
    }
    match &events[1].event {
        EventType::EvolutionResult { success, .. } => assert!(*success),
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn rejected_candidate_rolls_back_and_audits_failure() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion("fn execute_plan(plan) { this is not rhai").to_string())
        .create_async()
        .await;

    let mut f = fixture(&server.url());
    std::fs::write(f.scripts_dir.join("plan_and_execute.rhai"), DEFECTIVE).unwrap();
    f.engine.reload("plan_and_execute").unwrap();

    let outcome = f.evolution.attempt(&context(&f)).await.unwrap();
    match outcome {
        EvolutionOutcome::Failed { reason } => {
            assert!(reason.contains("compile"), "{reason}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(f.scripts_dir.join("plan_and_execute.rhai")).unwrap(),
        DEFECTIVE,
        "rollback must restore the original"
    );
    assert!(!f.scripts_dir.join("plan_and_execute.rhai.bak").exists());

    let events = f.store.events_after("c1", 0).unwrap();
    match &events[1].event {
        EventType::EvolutionResult { success, .. } => assert!(!*success),
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn candidate_without_test_entry_is_rejected() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion("fn execute_plan(plan) { \"ok\" }").to_string())
        .create_async()
        .await;

    let mut f = fixture(&server.url());
    std::fs::write(f.scripts_dir.join("plan_and_execute.rhai"), DEFECTIVE).unwrap();
    f.engine.reload("plan_and_execute").unwrap();

    let outcome = f.evolution.attempt(&context(&f)).await.unwrap();
    match outcome {
        EvolutionOutcome::Failed { reason } => {
            assert!(reason.contains("test_plan_and_execute"), "{reason}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(f.scripts_dir.join("plan_and_execute.rhai")).unwrap(),
        DEFECTIVE
    );
}

#[tokio::test]
async fn static_danger_check_rejects_eval() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion("fn execute_plan(plan) { eval(\"rm -rf /\") }").to_string())
        .create_async()
        .await;

    let mut f = fixture(&server.url());
    std::fs::write(f.scripts_dir.join("plan_and_execute.rhai"), DEFECTIVE).unwrap();
    f.engine.reload("plan_and_execute").unwrap();

    let outcome = f.evolution.attempt(&context(&f)).await.unwrap();
    match outcome {
        EvolutionOutcome::Failed { reason } => {
            assert!(reason.contains("forbidden"), "{reason}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(f.scripts_dir.join("plan_and_execute.rhai")).unwrap(),
        DEFECTIVE,
        "dangerous candidate must never touch the live script"
    );
}

#[tokio::test]
async fn attempt_limit_is_enforced() {
    let mut server = mockito::Server::new_async().await;
    for _ in 0..3 {
        server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(completion("garbage").to_string())
            .create_async()
            .await;
    }

    let mut f = fixture(&server.url());
    std::fs::write(f.scripts_dir.join("plan_and_execute.rhai"), DEFECTIVE).unwrap();
    f.engine.reload("plan_and_execute").unwrap();

    for _ in 0..3 {
        let outcome = f.evolution.attempt(&context(&f)).await.unwrap();
        assert!(
            matches!(outcome, EvolutionOutcome::Failed { .. }),
            "attempt {} should fail",
            f.evolution.attempts_used()
        );
    }
    assert_eq!(f.evolution.attempts_used(), 3);

    // The fourth attempt is rejected by the limit before any LLM call.
    let outcome = f.evolution.attempt(&context(&f)).await.unwrap();
    match outcome {
        EvolutionOutcome::Failed { reason } => {
            assert!(reason.contains("limit reached"), "{reason}");
        }
        other => panic!("expected Failed with limit, got {other:?}"),
    }
    assert_eq!(
        f.evolution.attempts_used(),
        3,
        "limit must not consume an attempt"
    );
    assert_eq!(
        std::fs::read_to_string(f.scripts_dir.join("plan_and_execute.rhai")).unwrap(),
        DEFECTIVE
    );
}

#[tokio::test]
async fn creates_script_when_missing_and_rolls_back_to_nothing() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion("fn execute_plan(plan) { broken").to_string())
        .create_async()
        .await;

    let mut f = fixture(&server.url());
    // No script on disk at all.
    let mut ctx = context(&f);
    ctx.script_source = String::new();
    let outcome = f.evolution.attempt(&ctx).await.unwrap();
    assert!(matches!(outcome, EvolutionOutcome::Failed { .. }));
    assert!(
        !f.scripts_dir.join("plan_and_execute.rhai").exists(),
        "failed candidate must be removed when no original existed"
    );
    assert!(!f.scripts_dir.join("plan_and_execute.rhai.bak").exists());
}

#[tokio::test]
async fn candidate_cannot_bypass_sandbox_whitelist() {
    let mut server = mockito::Server::new_async().await;
    // The candidate calls a command outside the whitelist; its self-test must
    // fail in isolation, so the candidate is rejected and rolled back.
    let dangerous = r#"
fn execute_plan(plan) {
    exec_command("rm", ["-rf", "/"]);
    "ok"
}

fn test_plan_and_execute() {
    let out = exec_command("rm", ["-rf", "/"]);
    "ran"
}
"#;
    server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(completion(dangerous).to_string())
        .create_async()
        .await;

    let mut f = fixture(&server.url());
    std::fs::write(f.scripts_dir.join("plan_and_execute.rhai"), DEFECTIVE).unwrap();
    f.engine.reload("plan_and_execute").unwrap();

    let outcome = f.evolution.attempt(&context(&f)).await.unwrap();
    match outcome {
        EvolutionOutcome::Failed { reason } => {
            assert!(reason.contains("isolation"), "{reason}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(f.scripts_dir.join("plan_and_execute.rhai")).unwrap(),
        DEFECTIVE,
        "whitelist bypass candidate must be rolled back"
    );
}

#[test]
fn sha256_hex_is_stable() {
    let hash = sha256_hex(b"hello");
    assert_eq!(
        hash,
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

#[test]
fn candidate_extraction_strips_fences() {
    let with_fences = "Here you go:\n```rhai\nfn execute_plan(plan) { \"x\" }\n```\n";
    let extracted = lemon_agent::evolution::extract_script(with_fences);
    assert_eq!(
        extracted.as_deref(),
        Some("fn execute_plan(plan) { \"x\" }")
    );

    let plain = "fn execute_plan(plan) { \"y\" }";
    assert_eq!(
        lemon_agent::evolution::extract_script(plain).as_deref(),
        Some(plain)
    );

    let nonsense = "no script here";
    assert!(lemon_agent::evolution::extract_script(nonsense).is_none());
}
