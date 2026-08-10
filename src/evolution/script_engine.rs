//! The Rhai script engine: dynamically loaded, hot-reloadable execution
//! strategies with sandboxed, capability-gated tools.
//!
//! Script convention: each `.rhai` file in the scripts directory must export
//! `fn execute_plan(plan)` taking the plan as an object map and returning a
//! string. Failures are raised with `throw`, which the engine converts into
//! `Err`. Tool invocations are counted per execution to bound runaway loops.
//!
//! Rhai is synchronous, so script bodies run on the blocking pool with a hard
//! timeout; tool wrappers re-enter the async runtime through the current
//! handle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use rhai::{AST, Dynamic, Engine, EvalAltResult, Map, Scope};
use serde_json::Value;
use tracing::{info, warn};

use crate::error::Error;
use crate::kernel::capability::CapabilitySet;
use crate::kernel::event_store::{EventStore, EventType, ToolOutcome};
use crate::kernel::sandbox::Sandbox;
use crate::llm::{LLMClient, Message};
use crate::scheduler::plan::Plan;

const MAX_TOOL_CALLS_PER_EXECUTION: usize = 64;
const SCRIPT_CALL_TIMEOUT_SECS: u64 = 60;
const MAX_CALL_LEVELS: usize = 32;
const MAX_EXPR_DEPTHS: (usize, usize) = (64, 32);
const MAX_OPERATIONS: u64 = 1_000_000;

type ScriptCache = Arc<Mutex<HashMap<String, Arc<AST>>>>;

/// Shared tool environment captured by every registered Rhai function.
#[derive(Debug, Clone)]
struct ToolEnv {
    sandbox: Arc<Sandbox>,
    token: CapabilitySet,
    llm: Option<Arc<LLMClient>>,
    store: Option<Arc<EventStore>>,
    continuity: Arc<Mutex<Option<String>>>,
    used: Arc<Mutex<usize>>,
}

impl ToolEnv {
    fn record_call(&self) -> RhaiResult<()> {
        let mut count = self.used.lock().unwrap_or_else(|p| p.into_inner());
        if *count >= MAX_TOOL_CALLS_PER_EXECUTION {
            return Err(Box::new(EvalAltResult::ErrorRuntime(
                format!("tool call limit exceeded ({MAX_TOOL_CALLS_PER_EXECUTION})").into(),
                rhai::Position::NONE,
            )));
        }
        *count += 1;
        Ok(())
    }

    fn err_box(e: crate::error::Error) -> Box<EvalAltResult> {
        Box::new(EvalAltResult::ErrorRuntime(
            e.to_string().into(),
            rhai::Position::NONE,
        ))
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Handle::current().block_on(future)
    }

    fn read_file(&self, path: String) -> RhaiResult<String> {
        self.record_call()?;
        Self::block_on(self.sandbox.read_file(&self.token, &path)).map_err(Self::err_box)
    }

    fn write_file(&self, path: String, content: String) -> RhaiResult<()> {
        self.record_call()?;
        Self::block_on(self.sandbox.write_file(&self.token, &path, &content)).map_err(Self::err_box)
    }

    fn append_file(&self, path: String, content: String) -> RhaiResult<()> {
        self.record_call()?;
        Self::block_on(self.sandbox.append_file(&self.token, &path, &content))
            .map_err(Self::err_box)
    }

    fn list_dir(&self, path: String) -> RhaiResult<Dynamic> {
        self.record_call()?;
        let entries =
            Self::block_on(self.sandbox.list_dir(&self.token, &path)).map_err(Self::err_box)?;
        Ok(entries_to_dynamic(&entries))
    }

    fn search_code(&self, query: String, path: String) -> RhaiResult<Dynamic> {
        self.record_call()?;
        let hits = Self::block_on(self.sandbox.search_code(&self.token, &query, &path))
            .map_err(Self::err_box)?;
        Ok(hits_to_dynamic(&hits))
    }

    fn exec_command(&self, cmd: String, args: rhai::Array) -> RhaiResult<Dynamic> {
        self.record_call()?;
        let strings: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let output = Self::block_on(self.sandbox.exec_command(&self.token, &cmd, &strings))
            .map_err(Self::err_box)?;
        Ok(output_to_dynamic(&output))
    }

    fn git_add_commit(&self, message: String) -> RhaiResult<String> {
        self.record_call()?;
        Self::block_on(self.sandbox.git_add_commit(&self.token, &message)).map_err(Self::err_box)
    }

    fn sleep(&self, ms: i64) -> RhaiResult<()> {
        self.record_call()?;
        let ms = u64::try_from(ms).map_err(|_| {
            Self::err_box(Error::InvalidInput(
                "sleep requires a non-negative duration".to_string(),
            ))
        })?;
        Self::block_on(self.sandbox.sleep(&self.token, ms)).map_err(Self::err_box)
    }

    fn llm_query(&self, prompt: String) -> RhaiResult<String> {
        self.record_call()?;
        let llm = self.llm.clone().ok_or_else(|| {
            Self::err_box(Error::CapabilityDenied {
                operation: "llm_query".to_string(),
                reason: "no LLM client configured".to_string(),
            })
        })?;
        let prompt_preview = preview(&prompt, 200);
        if let Some(store) = &self.store
            && let Some(id) = self
                .continuity
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
            && let Err(e) = store.append(
                &id,
                &EventType::LlmRequest {
                    prompt_preview: prompt_preview.clone(),
                    tools: Vec::new(),
                },
            )
        {
            warn!(error = %e, "failed to persist LlmRequest event");
        }
        let result = Self::block_on(llm.chat(&[Message::user(&prompt)], &[]));
        if let Some(store) = &self.store
            && let Some(id) = self
                .continuity
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        {
            let outcome = match &result {
                Ok(response) => {
                    let _ = store.append(
                        &id,
                        &EventType::LlmResponse {
                            content: preview(&response.content, 200),
                            tool_calls: response.tool_calls.clone(),
                        },
                    );
                    ToolOutcome::Ok(serde_json::json!({ "content": response.content }))
                }
                Err(e) => ToolOutcome::Err(e.to_string()),
            };
            let _ = store.append(
                &id,
                &EventType::ToolCall {
                    tool_name: "llm_query".to_string(),
                    args: serde_json::json!({ "prompt": prompt_preview }),
                    result: outcome,
                },
            );
        }
        result
            .map_err(Self::err_box)
            .map(|response| response.content)
    }
}

type RhaiResult<T> = std::result::Result<T, Box<EvalAltResult>>;

/// The hot-reloadable script runtime.
#[derive(Debug)]
pub struct ScriptEngine {
    engine: Arc<Engine>,
    registry: ScriptCache,
    scripts_dir: PathBuf,
    env: ToolEnv,
    _watcher: Arc<Mutex<Option<notify::RecommendedWatcher>>>,
}

impl ScriptEngine {
    /// Build an engine for `scripts_dir`. All scripts are compiled and cached;
    /// invalid scripts are skipped with a warning and never break the runtime.
    pub fn new(
        scripts_dir: PathBuf,
        token: CapabilitySet,
        sandbox: Arc<Sandbox>,
        llm: Option<Arc<LLMClient>>,
        store: Option<Arc<EventStore>>,
    ) -> crate::error::Result<ScriptEngine> {
        if !scripts_dir.exists() {
            std::fs::create_dir_all(&scripts_dir)
                .map_err(|e| Error::io(Some(scripts_dir.clone()), e))?;
        }

        let mut engine = Engine::new();
        engine.set_max_call_levels(MAX_CALL_LEVELS);
        engine.set_max_expr_depths(MAX_EXPR_DEPTHS.0, MAX_EXPR_DEPTHS.1);
        engine.set_max_operations(MAX_OPERATIONS);

        let env = ToolEnv {
            sandbox,
            token,
            llm,
            store,
            continuity: Arc::new(Mutex::new(None)),
            used: Arc::new(Mutex::new(0)),
        };
        register_builtins(&mut engine, &env);

        let mut script_engine = ScriptEngine {
            engine: Arc::new(engine),
            registry: Arc::new(Mutex::new(HashMap::new())),
            scripts_dir,
            env,
            _watcher: Arc::new(Mutex::new(None)),
        };
        script_engine.load_all()?;
        script_engine.spawn_watcher()?;
        Ok(script_engine)
    }

    /// Bind audit events to a continuity.
    pub fn set_continuity(&self, continuity_id: &str) {
        *self
            .env
            .continuity
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(continuity_id.to_string());
    }

    /// The scripts directory.
    pub fn scripts_dir(&self) -> &Path {
        &self.scripts_dir
    }

    /// The names of all successfully compiled scripts.
    pub fn script_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Compile and cache every `.rhai` file in the scripts directory.
    fn load_all(&mut self) -> crate::error::Result<()> {
        let entries = std::fs::read_dir(&self.scripts_dir)
            .map_err(|e| Error::io(Some(self.scripts_dir.clone()), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| Error::io(None, e))?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "rhai") {
                self.load_script(&path);
            }
        }
        Ok(())
    }

    /// Compile one script file; on failure keep any previous version.
    fn load_script(&self, path: &Path) {
        let name = script_name(path);
        match self.compile_file(path) {
            Ok(ast) => {
                self.registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(name.clone(), Arc::new(ast));
                info!(script = %name, "script compiled");
            }
            Err(e) => {
                warn!(script = %name, error = %e, "script failed to compile; keeping previous version");
            }
        }
    }

    fn compile_file(&self, path: &Path) -> crate::error::Result<AST> {
        let source =
            std::fs::read_to_string(path).map_err(|e| Error::io(Some(path.to_path_buf()), e))?;
        let ast = self
            .engine
            .compile_with_scope(&Scope::new(), &source)
            .map_err(|e| Error::Script {
                script: script_name(path),
                message: e.to_string(),
            })?;
        let has_entry = ast
            .iter_functions()
            .any(|f| f.name == "execute_plan" && f.params.len() == 1);
        if !has_entry {
            return Err(Error::Script {
                script: script_name(path),
                message: "script must define fn execute_plan(plan)".to_string(),
            });
        }
        Ok(ast)
    }

    /// Reload a single script by file stem (e.g. `plan_and_execute`).
    pub fn reload(&self, name: &str) -> crate::error::Result<()> {
        let path = self.scripts_dir.join(format!("{name}.rhai"));
        if !path.exists() {
            return Err(Error::FileNotFound(path));
        }
        match self.compile_file(&path) {
            Ok(ast) => {
                self.registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(name.to_string(), Arc::new(ast));
                info!(script = %name, "script reloaded");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Run `execute_plan` from `script` with the given plan.
    ///
    /// Returns the script's string on success; any script error (including
    /// `throw`) is returned as `Err` with the message.
    pub async fn execute_plan(
        &self,
        script: &str,
        plan: &Plan,
    ) -> std::result::Result<String, String> {
        let ast = {
            let registry = self.registry.lock().unwrap_or_else(|p| p.into_inner());
            registry.get(script).cloned().ok_or_else(|| {
                format!(
                    "script {script:?} is not loaded (check it compiles and defines execute_plan)"
                )
            })?
        };
        *self.env.used.lock().unwrap_or_else(|p| p.into_inner()) = 0;
        let engine = self.engine.clone();
        let plan_value = plan_to_dynamic(plan);
        let script_owned = script.to_string();
        let script_for_closure = script_owned.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let mut scope = Scope::new();
            engine
                .call_fn::<String>(&mut scope, &ast, "execute_plan", (plan_value,))
                .map_err(|e| script_error(&script_for_closure, e))
        });
        match tokio::time::timeout(Duration::from_secs(SCRIPT_CALL_TIMEOUT_SECS), handle).await {
            Err(_) => Err(format!(
                "script {script_owned:?} timed out after {SCRIPT_CALL_TIMEOUT_SECS}s"
            )),
            Ok(Err(join_error)) => Err(format!(
                "script {script_owned:?} worker panicked: {join_error}"
            )),
            Ok(Ok(result)) => result,
        }
    }

    /// Run an optional test entry point `test_<name>()` on the script named
    /// `<name>` (dots replaced with underscores). Returns `None` when the
    /// script defines no test function.
    pub async fn run_test(&self, script: &str) -> std::result::Result<Option<String>, String> {
        let ast = {
            let registry = self.registry.lock().unwrap_or_else(|p| p.into_inner());
            match registry.get(script) {
                Some(ast) => ast.clone(),
                None => return Err(format!("script {script:?} is not loaded")),
            }
        };
        let test_fn = format!("test_{}", script.replace('.', "_"));
        let has_test = ast
            .iter_functions()
            .any(|f| f.name == test_fn && f.params.is_empty());
        if !has_test {
            return Ok(None);
        }
        let engine = self.engine.clone();
        let test_fn_owned = test_fn.clone();
        let test_fn_for_closure = test_fn_owned.clone();
        let script = script.to_string();
        let script_for_closure = script.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let mut scope = Scope::new();
            engine
                .call_fn::<String>(&mut scope, &ast, &test_fn_for_closure, ())
                .map_err(|e| script_error(&script_for_closure, e))
        });
        match tokio::time::timeout(Duration::from_secs(SCRIPT_CALL_TIMEOUT_SECS), handle).await {
            Err(_) => Err(format!(
                "{test_fn_owned} timed out after {SCRIPT_CALL_TIMEOUT_SECS}s"
            )),
            Ok(Err(join_error)) => Err(format!("{test_fn_owned} worker panicked: {join_error}")),
            Ok(Ok(result)) => result.map(Some),
        }
    }

    /// The raw Rhai engine (used by tests and the evolution harness).
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// The capability token applied to every tool call.
    pub fn token(&self) -> &CapabilitySet {
        &self.env.token
    }

    /// Watch the scripts directory and hot-reload changed files.
    fn spawn_watcher(&mut self) -> crate::error::Result<()> {
        let scripts_dir = self.scripts_dir.clone();
        let registry = self.registry.clone();
        let handler = move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else {
                return;
            };
            let changed = event
                .paths
                .iter()
                .any(|p| p.extension().is_some_and(|e| e == "rhai"));
            if !changed {
                return;
            }
            let engine = Engine::new();
            match reload_from_disk(&engine, &scripts_dir, &registry) {
                Ok(()) => info!(target: "script", "hot reload completed"),
                Err(e) => warn!(target: "script", error = %e, "hot reload failed"),
            }
        };
        let mut watcher = notify::recommended_watcher(handler)
            .map_err(|e| Error::Internal(format!("failed to create file watcher: {e}")))?;
        watcher
            .watch(&self.scripts_dir, RecursiveMode::Recursive)
            .map_err(|e| Error::Internal(format!("failed to watch scripts dir: {e}")))?;
        *self._watcher.lock().unwrap_or_else(|p| p.into_inner()) = Some(watcher);
        Ok(())
    }
}

/// Register the sandboxed tools and logging helpers on `engine`.
fn register_builtins(engine: &mut Engine, env: &ToolEnv) {
    engine.register_fn("log_debug", |msg: String| {
        tracing::debug!(target: "script", "{msg}");
    });
    engine.register_fn("log_info", |msg: String| {
        tracing::info!(target: "script", "{msg}");
    });
    engine.register_fn("log_warn", |msg: String| {
        tracing::warn!(target: "script", "{msg}");
    });

    let e = env.clone();
    engine.register_fn("read_file", move |path: String| e.clone().read_file(path));
    let e = env.clone();
    engine.register_fn("write_file", move |path: String, content: String| {
        e.clone().write_file(path, content)
    });
    let e = env.clone();
    engine.register_fn("append_file", move |path: String, content: String| {
        e.clone().append_file(path, content)
    });
    let e = env.clone();
    engine.register_fn("list_dir", move |path: String| e.clone().list_dir(path));
    let e = env.clone();
    engine.register_fn("search_code", move |query: String, path: String| {
        e.clone().search_code(query, path)
    });
    let e = env.clone();
    engine.register_fn("exec_command", move |cmd: String, args: rhai::Array| {
        e.clone().exec_command(cmd, args)
    });
    let e = env.clone();
    engine.register_fn("git_add_commit", move |message: String| {
        e.clone().git_add_commit(message)
    });
    let e = env.clone();
    engine.register_fn("sleep", move |ms: i64| e.clone().sleep(ms));
    let e = env.clone();
    engine.register_fn("llm_query", move |prompt: String| {
        e.clone().llm_query(prompt)
    });
}

/// Recompile every script in the directory, keeping previous versions on
/// compile failure. Runs on the notify handler thread.
fn reload_from_disk(
    engine: &Engine,
    scripts_dir: &Path,
    registry: &ScriptCache,
) -> std::result::Result<(), String> {
    let entries = std::fs::read_dir(scripts_dir).map_err(|e| e.to_string())?;
    let mut first_error: Option<String> = None;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "rhai") {
            continue;
        }
        let name = script_name(&path);
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(e) => {
                first_error.get_or_insert_with(|| format!("{name}: {e}"));
                continue;
            }
        };
        match engine.compile_with_scope(&Scope::new(), &source) {
            Ok(ast) => {
                registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(name.clone(), Arc::new(ast));
                info!(target: "script", script = %name, "script hot-reloaded");
            }
            Err(e) => {
                warn!(target: "script", script = %name, error = %e, "hot reload rejected; keeping previous version");
                first_error.get_or_insert_with(|| format!("{name}: {e}"));
            }
        }
    }
    match first_error {
        Some(e) => Err(format!("{e} (previous versions kept)")),
        None => Ok(()),
    }
}

// ----------------------------------------------------------------------
// Conversions
// ----------------------------------------------------------------------

fn script_name(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn plan_to_dynamic(plan: &Plan) -> Dynamic {
    let steps: rhai::Array = plan
        .steps
        .iter()
        .map(|step| {
            let mut map = Map::new();
            map.insert("name".into(), Dynamic::from(step.name.clone()));
            map.insert("tool".into(), Dynamic::from(step.tool.clone()));
            map.insert("args".into(), json_to_dynamic(&step.args));
            Dynamic::from_map(map)
        })
        .collect();
    let mut plan_map = Map::new();
    plan_map.insert("steps".into(), Dynamic::from(steps));
    if let Some(verify) = &plan.verify {
        let mut verify_map = Map::new();
        verify_map.insert("cmd".into(), Dynamic::from(verify.cmd.clone()));
        verify_map.insert(
            "args".into(),
            Dynamic::from(
                verify
                    .args
                    .iter()
                    .cloned()
                    .map(Dynamic::from)
                    .collect::<rhai::Array>(),
            ),
        );
        plan_map.insert("verify".into(), Dynamic::from_map(verify_map));
    }
    Dynamic::from_map(plan_map)
}

fn json_to_dynamic(value: &Value) -> Dynamic {
    match value {
        Value::Null => Dynamic::UNIT,
        Value::Bool(b) => Dynamic::from(*b),
        Value::Number(n) => {
            Dynamic::from(n.as_i64().unwrap_or_else(|| n.as_u64().unwrap_or(0) as i64))
        }
        Value::String(s) => Dynamic::from(s.clone()),
        Value::Array(items) => {
            Dynamic::from(items.iter().map(json_to_dynamic).collect::<rhai::Array>())
        }
        Value::Object(entries) => {
            let mut map = Map::new();
            for (key, value) in entries {
                map.insert(key.clone().into(), json_to_dynamic(value));
            }
            Dynamic::from_map(map)
        }
    }
}

fn entries_to_dynamic(entries: &[crate::kernel::sandbox::FileEntry]) -> Dynamic {
    Dynamic::from(
        entries
            .iter()
            .map(|entry| {
                let mut map = Map::new();
                map.insert("name".into(), Dynamic::from(entry.name.clone()));
                map.insert("is_dir".into(), Dynamic::from(entry.is_dir));
                map.insert("size_bytes".into(), Dynamic::from(entry.size_bytes as i64));
                Dynamic::from_map(map)
            })
            .collect::<rhai::Array>(),
    )
}

fn hits_to_dynamic(hits: &[crate::kernel::sandbox::SearchHit]) -> Dynamic {
    Dynamic::from(
        hits.iter()
            .map(|hit| {
                let mut map = Map::new();
                map.insert("path".into(), Dynamic::from(hit.path.clone()));
                map.insert("line_num".into(), Dynamic::from(hit.line_num as i64));
                map.insert("line".into(), Dynamic::from(hit.line.clone()));
                Dynamic::from_map(map)
            })
            .collect::<rhai::Array>(),
    )
}

fn output_to_dynamic(output: &crate::kernel::sandbox::CommandOutput) -> Dynamic {
    let mut map = Map::new();
    map.insert("exit_code".into(), Dynamic::from(output.exit_code as i64));
    map.insert("stdout".into(), Dynamic::from(output.stdout.clone()));
    map.insert("stderr".into(), Dynamic::from(output.stderr.clone()));
    map.insert("timed_out".into(), Dynamic::from(output.timed_out));
    map.insert(
        "duration_ms".into(),
        Dynamic::from(output.duration_ms as i64),
    );
    Dynamic::from_map(map)
}

fn script_error(script: &str, e: Box<EvalAltResult>) -> String {
    format!("script {script} failed: {e}")
}

fn preview(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push_str("...");
        out
    }
}
