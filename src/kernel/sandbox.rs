//! The sandbox executes every external side effect: file access, subprocess
//! execution, and Git operations.
//!
//! All paths are resolved against `root_dir`, normalized, and verified to
//! remain inside it, including symlink parents. Commands must be on the
//! whitelist and run without a shell, with a hard time limit. Every operation
//! writes a `ToolCall` audit event to the event store when one is configured.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::warn;

use crate::error::{Error, Result};
use crate::kernel::capability::{CapabilitySet, Permissions, Resource};
use crate::kernel::event_store::{EventStore, EventType, ToolOutcome};

/// The result of a sandboxed command execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub duration_ms: u64,
}

/// An entry returned by `list_dir`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size_bytes: u64,
}

/// A match returned by `search_code`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchHit {
    pub path: String,
    pub line_num: usize,
    pub line: String,
}

const FS_OP_TIMEOUT_SECS: u64 = 30;
const SEARCH_MAX_HITS: usize = 200;
const SEARCH_LINE_LIMIT: usize = 200;
const OUTPUT_LIMIT_BYTES: usize = 1024 * 1024;
const SLEEP_MAX_MS: u64 = 25_000;

/// The sandboxed tool executor.
#[derive(Debug)]
pub struct Sandbox {
    root_dir: PathBuf,
    allowed_commands: HashSet<String>,
    command_timeout: Duration,
    max_file_size: usize,
    sleep_cap: Duration,
    store: Option<Arc<EventStore>>,
    continuity_id: Mutex<Option<String>>,
}

impl Sandbox {
    /// Create a sandbox rooted at `root_dir`.
    pub fn new(
        root_dir: PathBuf,
        allowed_commands: HashSet<String>,
        command_timeout: Duration,
        max_file_size: usize,
    ) -> Result<Sandbox> {
        if !root_dir.exists() {
            std::fs::create_dir_all(&root_dir).map_err(|e| Error::io(Some(root_dir.clone()), e))?;
        }
        let canonical =
            std::fs::canonicalize(&root_dir).map_err(|e| Error::io(Some(root_dir.clone()), e))?;
        Ok(Sandbox {
            root_dir: canonical,
            allowed_commands,
            command_timeout,
            max_file_size,
            sleep_cap: Duration::from_millis(SLEEP_MAX_MS),
            store: None,
            continuity_id: Mutex::new(None),
        })
    }

    /// Override the maximum sleep duration accepted by the `sleep` tool.
    pub fn with_sleep_cap(mut self, cap: Duration) -> Sandbox {
        self.sleep_cap = cap;
        self
    }

    /// Attach the event store used for audit events.
    pub fn with_event_store(mut self, store: Arc<EventStore>) -> Sandbox {
        self.store = Some(store);
        self
    }

    /// The canonical sandbox root.
    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    /// Bind subsequent audit events to `continuity_id`.
    pub fn set_continuity(&self, continuity_id: &str) {
        *self
            .continuity_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(continuity_id.to_string());
    }

    // ------------------------------------------------------------------
    // Audit and execution helpers
    // ------------------------------------------------------------------

    fn audit<T: Serialize>(&self, tool_name: &str, args: Value, result: &Result<T>) {
        let outcome = match result {
            Ok(value) => ToolOutcome::Ok(serde_json::to_value(value).unwrap_or(json!(null))),
            Err(e) => ToolOutcome::Err(e.to_string()),
        };
        let event = EventType::ToolCall {
            tool_name: tool_name.to_string(),
            args,
            result: outcome,
        };
        let continuity_id = self
            .continuity_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let (Some(store), Some(id)) = (&self.store, continuity_id)
            && let Err(e) = store.append(&id, &event)
        {
            warn!(error = %e, tool = tool_name, "failed to persist audit event");
        }
    }

    /// Run a blocking tool body on a worker thread with a timeout and audit it.
    async fn run_blocking<F, T>(&self, tool_name: &str, args: Value, body: F) -> Result<T>
    where
        F: FnOnce() -> Result<T> + Send + 'static,
        T: Serialize + Send + 'static,
    {
        let future = tokio::task::spawn_blocking(body);
        let result =
            match tokio::time::timeout(Duration::from_secs(FS_OP_TIMEOUT_SECS), future).await {
                Err(_) => Err(Error::Timeout {
                    operation: tool_name.to_string(),
                    timeout_secs: FS_OP_TIMEOUT_SECS,
                }),
                Ok(Err(join_error)) => Err(Error::Internal(format!(
                    "{tool_name} worker panicked: {join_error}"
                ))),
                Ok(Ok(inner)) => inner,
            };
        self.audit(tool_name, args, &result);
        result
    }

    /// Run an async tool body with a timeout and audit it.
    async fn run_async<F, Fut, T>(&self, tool_name: &str, args: Value, body: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
        T: Serialize,
    {
        let result =
            match tokio::time::timeout(Duration::from_secs(FS_OP_TIMEOUT_SECS), body()).await {
                Err(_) => Err(Error::Timeout {
                    operation: tool_name.to_string(),
                    timeout_secs: FS_OP_TIMEOUT_SECS,
                }),
                Ok(inner) => inner,
            };
        self.audit(tool_name, args, &result);
        result
    }

    // ------------------------------------------------------------------
    // File tools
    // ------------------------------------------------------------------

    /// Read a text file relative to the root.
    pub async fn read_file(&self, token: &CapabilitySet, path: &str) -> Result<String> {
        token.require(Resource::FileSystem, Permissions::read())?;
        let root = self.root_dir.clone();
        let max_file_size = self.max_file_size;
        let path = path.to_string();
        self.run_blocking("read_file", json!({ "path": path }), move || {
            let resolved = Sandbox::resolve_outer(&root, &path)?;
            let real = Sandbox::canonicalize_outer(&root, &resolved)?;
            let size = std::fs::metadata(&real)
                .map_err(|e| Error::io(Some(real.clone()), e))?
                .len();
            if size > max_file_size as u64 {
                return Err(Error::InvalidInput(format!(
                    "file {} is {size} bytes, exceeding the {max_file_size}-byte limit",
                    real.display()
                )));
            }
            std::fs::read_to_string(&real).map_err(|e| Error::io(Some(real), e))
        })
        .await
    }

    /// Write a file atomically (temp file + rename).
    pub async fn write_file(&self, token: &CapabilitySet, path: &str, content: &str) -> Result<()> {
        token.require(Resource::FileSystem, Permissions::write())?;
        let root = self.root_dir.clone();
        let max_file_size = self.max_file_size;
        let content = content.to_string();
        let path = path.to_string();
        self.run_blocking(
            "write_file",
            json!({ "path": path, "content_len": content.len() }),
            move || {
                if content.len() > max_file_size {
                    return Err(Error::InvalidInput(format!(
                        "content is {} bytes, exceeding the {max_file_size}-byte limit",
                        content.len()
                    )));
                }
                let resolved = Sandbox::resolve_outer(&root, &path)?;
                let real = Sandbox::ensure_real_path_outer(&root, &resolved)?;
                atomic_write(&real, content.as_bytes())
            },
        )
        .await
    }

    /// Append to a file, creating it when missing.
    pub async fn append_file(
        &self,
        token: &CapabilitySet,
        path: &str,
        content: &str,
    ) -> Result<()> {
        token.require(Resource::FileSystem, Permissions::write())?;
        let root = self.root_dir.clone();
        let max_file_size = self.max_file_size;
        let content = content.to_string();
        let path = path.to_string();
        self.run_blocking(
            "append_file",
            json!({ "path": path, "content_len": content.len() }),
            move || {
                let resolved = Sandbox::resolve_outer(&root, &path)?;
                let real = Sandbox::ensure_real_path_outer(&root, &resolved)?;
                if real.exists() {
                    let size = std::fs::metadata(&real)
                        .map_err(|e| Error::io(Some(real.clone()), e))?
                        .len();
                    if size + content.len() as u64 > max_file_size as u64 {
                        return Err(Error::InvalidInput(format!(
                            "append would exceed the {max_file_size}-byte limit"
                        )));
                    }
                }
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&real)
                    .map_err(|e| Error::io(Some(real.clone()), e))?;
                std::io::Write::write_all(&mut file, content.as_bytes())
                    .map_err(|e| Error::io(Some(real.clone()), e))
            },
        )
        .await
    }

    /// List entries in a directory relative to the root.
    pub async fn list_dir(&self, token: &CapabilitySet, path: &str) -> Result<Vec<FileEntry>> {
        token.require(Resource::FileSystem, Permissions::read())?;
        let root = self.root_dir.clone();
        let path = path.to_string();
        self.run_blocking("list_dir", json!({ "path": path }), move || {
            let resolved = Sandbox::resolve_outer(&root, &path)?;
            let real = Sandbox::canonicalize_outer(&root, &resolved)?;
            if !real.is_dir() {
                return Err(Error::InvalidInput(format!(
                    "{} is not a directory",
                    real.display()
                )));
            }
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(&real).map_err(|e| Error::io(Some(real.clone()), e))? {
                let entry = entry.map_err(|e| Error::io(Some(real.clone()), e))?;
                let file_type = entry
                    .file_type()
                    .map_err(|e| Error::io(Some(real.clone()), e))?;
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                entries.push(FileEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    is_dir: file_type.is_dir(),
                    size_bytes: size,
                });
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(entries)
        })
        .await
    }

    /// Search text files under `path` for `query` (case-insensitive).
    pub async fn search_code(
        &self,
        token: &CapabilitySet,
        query: &str,
        path: &str,
    ) -> Result<Vec<SearchHit>> {
        token.require(Resource::FileSystem, Permissions::read())?;
        if query.trim().is_empty() {
            return Err(Error::InvalidInput(
                "search query must not be empty".to_string(),
            ));
        }
        let root = self.root_dir.clone();
        let query = query.to_string();
        let path = path.to_string();
        self.run_blocking(
            "search_code",
            json!({ "query": query, "path": path }),
            move || {
                let resolved = Sandbox::resolve_outer(&root, &path)?;
                let real = Sandbox::canonicalize_outer(&root, &resolved)?;
                if !real.is_dir() {
                    return Err(Error::InvalidInput(format!(
                        "{} is not a directory",
                        real.display()
                    )));
                }
                let needle = query.to_lowercase();
                let mut hits = Vec::new();
                for entry in ignore::WalkBuilder::new(&real)
                    .hidden(true)
                    .build()
                    .flatten()
                {
                    if hits.len() >= SEARCH_MAX_HITS {
                        break;
                    }
                    if entry.file_type().is_some_and(|t| !t.is_file()) {
                        continue;
                    }
                    if is_binary(entry.path()) {
                        continue;
                    }
                    let Ok(contents) = std::fs::read_to_string(entry.path()) else {
                        continue;
                    };
                    for (line_num, line) in contents.lines().enumerate() {
                        if line.to_lowercase().contains(&needle) {
                            hits.push(SearchHit {
                                path: entry
                                    .path()
                                    .strip_prefix(&real)
                                    .unwrap_or(entry.path())
                                    .to_string_lossy()
                                    .into_owned(),
                                line_num: line_num + 1,
                                line: truncate(line, SEARCH_LINE_LIMIT),
                            });
                            if hits.len() >= SEARCH_MAX_HITS {
                                break;
                            }
                        }
                    }
                }
                Ok(hits)
            },
        )
        .await
    }

    // ------------------------------------------------------------------
    // Process tools
    // ------------------------------------------------------------------

    /// Execute a whitelisted command with the given arguments and no shell.
    pub async fn exec_command(
        &self,
        token: &CapabilitySet,
        cmd: &str,
        args: &[String],
    ) -> Result<CommandOutput> {
        token.require(Resource::Process, Permissions::execute())?;
        if !self.allowed_commands.contains(cmd) {
            return Err(Error::CapabilityDenied {
                operation: format!("exec {cmd}"),
                reason: "command is not on the whitelist".to_string(),
            });
        }
        if args.len() > 1024 {
            return Err(Error::InvalidInput(format!(
                "{cmd} called with {} arguments, exceeding the limit of 1024",
                args.len()
            )));
        }
        for arg in args {
            if arg.as_bytes().contains(&0) {
                return Err(Error::InvalidInput(format!(
                    "argument for {cmd} contains a NUL byte"
                )));
            }
        }
        let root = self.root_dir.clone();
        let command_timeout = self.command_timeout;
        let cmd = cmd.to_string();
        let args = args.to_vec();
        self.run_async(
            "exec_command",
            json!({ "cmd": &cmd, "args": &args }),
            move || async move {
                let started = std::time::Instant::now();
                let mut child = Command::new(&cmd)
                    .args(&args)
                    .current_dir(&root)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .map_err(|e| Error::io(None, e))?;
                let mut stdout_pipe = child
                    .stdout
                    .take()
                    .ok_or_else(|| Error::Internal("stdout pipe missing".to_string()))?;
                let mut stderr_pipe = child
                    .stderr
                    .take()
                    .ok_or_else(|| Error::Internal("stderr pipe missing".to_string()))?;
                let stdout_task = tokio::spawn(async move {
                    let mut buf = Vec::new();
                    stdout_pipe.read_to_end(&mut buf).await?;
                    Ok::<_, std::io::Error>(buf)
                });
                let stderr_task = tokio::spawn(async move {
                    let mut buf = Vec::new();
                    stderr_pipe.read_to_end(&mut buf).await?;
                    Ok::<_, std::io::Error>(buf)
                });
                let wait_result = tokio::time::timeout(command_timeout, child.wait()).await;
                let duration_ms = started.elapsed().as_millis() as u64;
                match wait_result {
                    Ok(Ok(status)) => {
                        let stdout = stdout_task
                            .await
                            .map_err(|e| Error::Internal(format!("stdout reader failed: {e}")))?
                            .map_err(|e| Error::io(None, e))?;
                        let stderr = stderr_task
                            .await
                            .map_err(|e| Error::Internal(format!("stderr reader failed: {e}")))?
                            .map_err(|e| Error::io(None, e))?;
                        Ok(CommandOutput {
                            exit_code: status.code().unwrap_or(-1),
                            stdout: truncate_bytes(&stdout, OUTPUT_LIMIT_BYTES),
                            stderr: truncate_bytes(&stderr, OUTPUT_LIMIT_BYTES),
                            timed_out: false,
                            duration_ms,
                        })
                    }
                    Ok(Err(e)) => Err(Error::io(None, e)),
                    Err(_) => {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        Ok(CommandOutput {
                            exit_code: -1,
                            stdout: String::new(),
                            stderr: format!(
                                "command timed out after {}s",
                                command_timeout.as_secs()
                            ),
                            timed_out: true,
                            duration_ms,
                        })
                    }
                }
            },
        )
        .await
    }

    // ------------------------------------------------------------------
    // Git tools
    // ------------------------------------------------------------------

    /// Stage and commit all changes in the sandbox root.
    pub async fn git_add_commit(&self, token: &CapabilitySet, message: &str) -> Result<String> {
        token.require(Resource::Git, Permissions::execute())?;
        self.run_async("git_add_commit", json!({ "message": message }), || async {
            if message.trim().is_empty() || message.contains('\n') {
                return Err(Error::InvalidInput(
                    "commit message must be a single non-empty line".to_string(),
                ));
            }
            if !self.root_dir.join(".git").exists() {
                return Err(Error::InvalidInput(format!(
                    "{} is not a git repository",
                    self.root_dir.display()
                )));
            }
            let identity = [
                "-c",
                "user.name=lemon-agent",
                "-c",
                "user.email=lemon-agent@local",
            ];
            let mut commit_args = identity.to_vec();
            commit_args.extend(["commit", "-m", message]);
            let add = run_git(&self.root_dir, self.command_timeout, &["add", "-A"]).await?;
            if !add.success {
                return Err(Error::Internal(format!("git add failed: {}", add.stderr)));
            }
            let commit = run_git(&self.root_dir, self.command_timeout, &commit_args).await?;
            if !commit.success {
                let combined = format!("{}{}", commit.stdout, commit.stderr);
                if combined.contains("nothing to commit") {
                    return Ok("nothing to commit".to_string());
                }
                return Err(Error::Internal(format!("git commit failed: {combined}")));
            }
            let log = run_git(&self.root_dir, self.command_timeout, &["rev-parse", "HEAD"]).await?;
            Ok(log.stdout.trim().to_string())
        })
        .await
    }

    // ------------------------------------------------------------------
    // Misc tools
    // ------------------------------------------------------------------

    /// Sleep for `ms` milliseconds, capped to protect the loop. The tool
    /// timeout is the sleep duration plus a margin so long (but legal) sleeps
    /// are not mistaken for hangs.
    pub async fn sleep(&self, token: &CapabilitySet, ms: u64) -> Result<()> {
        token.require(Resource::FileSystem, Permissions::read())?;
        let ms = ms.min(self.sleep_cap.as_millis() as u64);
        let timeout = Duration::from_millis(ms + 5000);
        let result: Result<()> = tokio::time::timeout(timeout, async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(())
        })
        .await
        .map_err(|_| Error::Timeout {
            operation: "sleep".to_string(),
            timeout_secs: timeout.as_secs(),
        })?;
        self.audit("sleep", json!({ "ms": ms }), &result);
        result
    }

    // ------------------------------------------------------------------
    // Static path helpers usable from `move` closures
    // ------------------------------------------------------------------

    fn resolve_outer(root: &Path, rel: &str) -> Result<PathBuf> {
        if rel.trim().is_empty() {
            return Err(Error::InvalidInput("path must not be empty".to_string()));
        }
        let candidate = Path::new(rel);
        let mut resolved = root.to_path_buf();
        let mut depth = 0usize;
        for component in candidate.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if depth == 0 {
                        return Err(Error::PathViolation {
                            path: candidate.to_path_buf(),
                            root: root.to_path_buf(),
                        });
                    }
                    depth -= 1;
                    resolved.pop();
                }
                Component::Normal(part) => {
                    depth += 1;
                    resolved.push(part);
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(Error::PathViolation {
                        path: candidate.to_path_buf(),
                        root: root.to_path_buf(),
                    });
                }
            }
        }
        Ok(resolved)
    }

    fn ensure_real_path_outer(root: &Path, resolved: &Path) -> Result<PathBuf> {
        let parent = resolved.parent().ok_or_else(|| Error::PathViolation {
            path: resolved.to_path_buf(),
            root: root.to_path_buf(),
        })?;
        if !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io(Some(parent.to_path_buf()), e))?;
        }
        let canonical_parent = std::fs::canonicalize(parent).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::FileNotFound(parent.to_path_buf())
            } else {
                Error::io(Some(parent.to_path_buf()), e)
            }
        })?;
        if !canonical_parent.starts_with(root) {
            return Err(Error::PathViolation {
                path: resolved.to_path_buf(),
                root: root.to_path_buf(),
            });
        }
        Ok(match resolved.file_name() {
            Some(name) => canonical_parent.join(name),
            None => canonical_parent,
        })
    }

    fn canonicalize_outer(root: &Path, resolved: &Path) -> Result<PathBuf> {
        let real = std::fs::canonicalize(resolved).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::FileNotFound(resolved.to_path_buf())
            } else {
                Error::io(Some(resolved.to_path_buf()), e)
            }
        })?;
        if !real.starts_with(root) {
            return Err(Error::PathViolation {
                path: resolved.to_path_buf(),
                root: root.to_path_buf(),
            });
        }
        Ok(real)
    }
}

struct GitOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

async fn run_git(root: &Path, timeout: Duration, args: &[&str]) -> Result<GitOutput> {
    let child = Command::new("git")
        .args(args)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::io(None, e))?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| Error::CommandTimeout {
            command: format!("git {}", args.join(" ")),
            timeout_secs: timeout.as_secs(),
        })?
        .map_err(|e| Error::io(None, e))?;
    Ok(GitOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Write `bytes` to `path` atomically: temp file in the same directory,
/// fsync, then rename over the target.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().and_then(OsStr::to_str).unwrap_or("file");
    let tmp = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        random_suffix()
    ));
    let write_result = (|| -> Result<()> {
        let mut file = std::fs::File::create(&tmp).map_err(|e| Error::io(Some(tmp.clone()), e))?;
        file.write_all(bytes)
            .map_err(|e| Error::io(Some(tmp.clone()), e))?;
        file.sync_all()
            .map_err(|e| Error::io(Some(tmp.clone()), e))?;
        drop(file);
        std::fs::rename(&tmp, path).map_err(|e| Error::io(Some(path.to_path_buf()), e))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_result.map_err(|e| match e {
        Error::Io { source, .. } => Error::AtomicWrite {
            path: path.to_path_buf(),
            source,
        },
        other => other,
    })
}

fn random_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{nanos:010}")
}

fn is_binary(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return true;
    };
    bytes.iter().take(8192).any(|&b| b == 0)
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push_str("...");
        out
    }
}

fn truncate_bytes(bytes: &[u8], max: usize) -> String {
    let slice = &bytes[..bytes.len().min(max)];
    let mut out = String::from_utf8_lossy(slice).into_owned();
    if bytes.len() > max {
        out.push_str("... [truncated]");
    }
    out
}
