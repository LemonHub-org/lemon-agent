//! Integration tests for the sandbox: path confinement, command whitelist,
//! timeouts, capability denial, and audit events.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use lemon_agent::error::{Error, ErrorCode, Result};
use lemon_agent::kernel::capability::{Capability, CapabilitySet, Permissions, Resource};
use lemon_agent::kernel::event_store::{EventStore, EventType, ToolOutcome};
use lemon_agent::kernel::sandbox::Sandbox;

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    sandbox: Sandbox,
    store: Arc<EventStore>,
}

fn command_set(extra: &[&str]) -> HashSet<String> {
    let mut set: HashSet<String> = ["git", "cargo", "rustc", "python3", "ls"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for c in extra {
        set.insert(c.to_string());
    }
    set
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(EventStore::open(&dir.path().join("audit.db")).unwrap());
    let sandbox = Sandbox::new(
        root.clone(),
        command_set(&["sleep", "ping"]),
        Duration::from_secs(120),
        1024 * 1024,
    )
    .unwrap()
    .with_event_store(store.clone());
    sandbox.set_continuity("c1");
    Fixture {
        _dir: dir,
        root,
        sandbox,
        store,
    }
}

fn token() -> CapabilitySet {
    CapabilitySet::full()
}

async fn write_in_root(root: &Path, name: &str, content: &str) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(root.join(name), content).unwrap();
}

#[tokio::test]
async fn read_write_append_list_roundtrip() {
    let f = fixture();
    let sb = &f.sandbox;

    sb.write_file(&token(), "a/b.txt", "hello").await.unwrap();
    let content = sb.read_file(&token(), "a/b.txt").await.unwrap();
    assert_eq!(content, "hello");

    sb.append_file(&token(), "a/b.txt", "\nworld")
        .await
        .unwrap();
    let content = sb.read_file(&token(), "a/b.txt").await.unwrap();
    assert_eq!(content, "hello\nworld");

    let entries = sb.list_dir(&token(), "a").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "b.txt");
    assert!(!entries[0].is_dir);
    assert_eq!(entries[0].size_bytes, 11);
}

#[tokio::test]
async fn missing_file_returns_e001() {
    let f = fixture();
    let err = f.sandbox.read_file(&token(), "nope.txt").await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::FileNotFound);
}

#[tokio::test]
async fn path_traversal_is_rejected() {
    let f = fixture();
    write_in_root(&f.root, "secret.txt", "s3cret").await;
    let outside = f.root.parent().unwrap().join("outside.txt");
    std::fs::write(&outside, "outside").unwrap();

    let err = f
        .sandbox
        .read_file(&token(), "../outside.txt")
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::PathViolation, "{err}");

    let err = f
        .sandbox
        .read_file(&token(), "sub/../../secret.txt")
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::PathViolation, "{err}");

    let err = f
        .sandbox
        .read_file(&token(), &outside.to_string_lossy())
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::PathViolation, "{err}");

    let err = f
        .sandbox
        .write_file(&token(), "../evil.txt", "x")
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::PathViolation, "{err}");
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_escape_is_rejected() {
    let f = fixture();
    let outside = f.root.parent().unwrap().join("outside.txt");
    std::fs::write(&outside, "outside").unwrap();
    std::os::unix::fs::symlink(&outside, f.root.join("link.txt")).unwrap();

    let err = f.sandbox.read_file(&token(), "link.txt").await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::PathViolation, "{err}");
}

#[tokio::test]
async fn unauthorized_command_is_denied() {
    let f = fixture();
    let err = f
        .sandbox
        .exec_command(&token(), "rm", &["-rf".to_string(), "/".to_string()])
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::CapabilityDenied, "{err}");
}

#[tokio::test]
async fn missing_capability_is_denied() {
    let f = fixture();
    write_in_root(&f.root, "a.txt", "x").await;

    let read_only = CapabilitySet::from_capabilities(vec![Capability::new(
        Resource::FileSystem,
        Permissions::read(),
    )]);
    let err = f
        .sandbox
        .write_file(&read_only, "a.txt", "y")
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::CapabilityDenied, "{err}");

    let fs_only = CapabilitySet::full().with_only(Resource::FileSystem);
    let err = f
        .sandbox
        .exec_command(&fs_only, "ls", &["-la".to_string()])
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::CapabilityDenied, "{err}");
}

#[tokio::test]
async fn command_timeout_kills_child() {
    // Rebuild a sandbox with a 1-second command timeout.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let sandbox = Sandbox::new(
        root,
        command_set(&["sleep", "ping"]),
        Duration::from_millis(1000),
        1024 * 1024,
    )
    .unwrap();

    let args = if cfg!(windows) {
        vec!["-n".to_string(), "5".to_string(), "127.0.0.1".to_string()]
    } else {
        vec!["5".to_string()]
    };
    let cmd = if cfg!(windows) { "ping" } else { "sleep" };
    let output = sandbox.exec_command(&token(), cmd, &args).await.unwrap();
    assert!(output.timed_out, "expected timeout: {output:?}");
    assert_eq!(output.exit_code, -1);
    assert!(output.stderr.contains("timed out"));
}

#[tokio::test]
async fn command_output_is_captured() {
    let f = fixture();
    let output = f
        .sandbox
        .exec_command(&token(), "git", &["--version".to_string()])
        .await
        .unwrap();
    assert!(!output.timed_out);
    assert_eq!(output.exit_code, 0);
    assert!(output.stdout.contains("git version"), "{output:?}");
}

#[tokio::test]
async fn injection_args_are_blocked() {
    let f = fixture();
    let err = f
        .sandbox
        .exec_command(&token(), "git", &["-c\x00foo".to_string()])
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidInput, "{err}");
}

#[tokio::test]
async fn git_add_commit_works_and_is_idempotent() {
    let f = fixture();
    let out = f
        .sandbox
        .exec_command(
            &token(),
            "git",
            &["init".to_string(), "-b".to_string(), "main".to_string()],
        )
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0, "git init failed: {out:?}");

    f.sandbox
        .write_file(&token(), "code.rs", "fn main() {}")
        .await
        .unwrap();
    let hash = f
        .sandbox
        .git_add_commit(&token(), "add code")
        .await
        .unwrap();
    assert!(!hash.starts_with("nothing"), "unexpected: {hash}");

    // Second call with no changes must not fail.
    let result = f.sandbox.git_add_commit(&token(), "again").await.unwrap();
    assert!(result.contains("nothing to commit"));
}

#[tokio::test]
async fn git_commit_message_injection_is_rejected() {
    let f = fixture();
    let out = f
        .sandbox
        .exec_command(&token(), "git", &["init".to_string()])
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0);

    let err = f
        .sandbox
        .git_add_commit(&token(), "evil\n--amend")
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidInput, "{err}");
}

#[tokio::test]
async fn every_tool_call_is_audited() {
    let f = fixture();
    f.sandbox
        .write_file(&token(), "a.txt", "hello")
        .await
        .unwrap();
    f.sandbox.read_file(&token(), "a.txt").await.unwrap();
    let err = f
        .sandbox
        .read_file(&token(), "missing.txt")
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::FileNotFound);
    f.sandbox
        .exec_command(&token(), "git", &["--version".to_string()])
        .await
        .unwrap();

    let events = f.store.events_after("c1", 0).unwrap();
    let tools: Vec<&EventType> = events
        .iter()
        .map(|e| &e.event)
        .filter(|e| matches!(e, EventType::ToolCall { .. }))
        .collect();
    assert_eq!(tools.len(), 4);

    let names: Vec<&str> = tools
        .iter()
        .map(|e| match e {
            EventType::ToolCall { tool_name, .. } => tool_name.as_str(),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        names,
        vec!["write_file", "read_file", "read_file", "exec_command"]
    );

    let failed = tools.iter().find_map(|e| match e {
        EventType::ToolCall {
            result: ToolOutcome::Err(msg),
            ..
        } => Some(msg.clone()),
        _ => None,
    });
    assert!(failed.is_some());
    assert!(failed.unwrap().contains("E001"));
}

#[tokio::test]
async fn oversized_file_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let sandbox = Sandbox::new(root, command_set(&[]), Duration::from_secs(5), 16).unwrap();

    let err = sandbox
        .write_file(&token(), "big.txt", "this content is way too long")
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidInput, "{err}");
    assert!(!std::path::Path::new(&sandbox.root_dir().join("big.txt")).exists());
}

#[tokio::test]
async fn search_code_finds_matches_without_following_binary() {
    let f = fixture();
    f.sandbox
        .write_file(
            &token(),
            "src/main.rs",
            "pub fn compute_ratio(a: u32, b: u32) -> f64 { a as f64 / b as f64 }",
        )
        .await
        .unwrap();
    f.sandbox
        .write_file(&token(), "src/binary.bin", "\u{0}\u{0}compute_ratio\u{0}")
        .await
        .unwrap();

    let hits = f
        .sandbox
        .search_code(&token(), "compute_ratio", "src")
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "main.rs");
    assert_eq!(hits[0].line_num, 1);
}

#[tokio::test]
async fn sleep_respects_capability_and_cap() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let sandbox = Sandbox::new(root, command_set(&[]), Duration::from_secs(5), 1024)
        .unwrap()
        .with_sleep_cap(Duration::from_millis(200));

    let start = std::time::Instant::now();
    sandbox.sleep(&token(), 60_000).await.unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(150),
        "not capped: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "capped sleep too long: {elapsed:?}"
    );

    let empty = CapabilitySet::from_capabilities(vec![]);
    let err = sandbox.sleep(&empty, 1).await.unwrap_err();
    assert_eq!(err.code(), ErrorCode::CapabilityDenied, "{err}");
}

#[tokio::test]
async fn failed_audit_keeps_error_context() {
    let f = fixture();
    let err: Result<()> = f.sandbox.write_file(&token(), "/etc/hosts", "pwned").await;
    let err = err.unwrap_err();
    assert!(matches!(err, Error::PathViolation { .. }), "{err}");
}
