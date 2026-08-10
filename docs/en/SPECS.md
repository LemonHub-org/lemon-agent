# SPECS �?Lemon Agent Technical Specification

> **Version**: 0.2.0
> **Goal**: Build an AI agent written from scratch in Rust that supports
> unattended long-running autonomous programming and self-evolution.
> **Core principles**: minimal dependencies, high performance, safe isolation,
> hot evolution.

---

## 1. Project Overview

### 1.1 Goal

Create a long-running daemon that can:

- Accept programming tasks (through an initial instruction or an external interface)
- Continuously perform programming activities �?writing, testing, debugging,
  refactoring �?without human intervention
- Autonomously improve its own behavior logic based on execution feedback (evolution)
- Provide complete audit logs and recovery capabilities

### 1.2 Scope

- **Language**: Rust (stable, 2024 edition+)
- **Core capabilities**: file read/write, command execution, code search,
  version control (Git), LLM calls
- **Evolution scope**: limited to hot updates of the script layer (Rhai
  scripts); the Rust kernel never modifies itself
- **Runtime environment**: cross-platform
### 1.3 Non-goals
- No graphical interface or interactive terminal (monitoring is done through logs)
- No distributed clusters (single-machine design)

---

## 2. System Architecture

### 2.1 Layered Model

```
┌─────────────────────────────────────────�?�?     External trigger (initial task/API) �?└─────────────────┬───────────────────────�?                  �?┌─────────────────────────────────────────�?�?         Scheduler                      �? �?Rust core, immutable
�? - main loop (while-true)               �?�? - state machine (IDLE, PLANNING,       �?�?   EXECUTING, EVALUATING, EVOLVING)     �?�? - context management (sliding window)  �?�? - budget control (steps/tokens/calls)  �?└──────────────┬──────────────────────────�?               �?               �?┌─────────────────────────────────────────�?�?         Capability Layer               �? �?Rust core, security boundary
�? - capability tokens                    �?�? - sandbox executor                     �?�? - tools (fs, process, git)             �?└──────────────┬──────────────────────────�?               �?               �?┌─────────────────────────────────────────�?�?         Script Engine                  �? �?Rhai runtime, evolvable
�? - dynamically loads scripts/*.rhai     �?�? - exposes Rust tools as Rhai functions �?�? - hot reload                           �?└──────────────┬──────────────────────────�?               �?               �?┌─────────────────────────────────────────�?�?        Evolution Engine                �? �?partly script, partly Rust
�? - error capture �?improved script      �?�? - script replacement and validation    �?└─────────────────────────────────────────�?```

### 2.2 Data Storage
- **SQLite**: a single database file `agent.db`, used for event sourcing and
  state snapshots.
- **File system**: project code lives in the working directory; scripts live
  in the `scripts/` subdirectory.

---

## 3. Core Components

### 3.1 Event-Sourced Storage (Event Store)

**File**: `src/kernel/event_store.rs`

Uses SQLite to store an immutable event log, with snapshots to speed up
recovery.

#### Table Structure

```sql
-- Events table
CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    continuity_id TEXT NOT NULL,          -- the current continuity session ID
    seq INTEGER NOT NULL,                 -- sequence number, monotonically increasing
    timestamp INTEGER NOT NULL,           -- Unix milliseconds
    event_type TEXT NOT NULL,             -- e.g. "StepStarted", "ToolCall", "LLMResponse", "Error", "Heartbeat"
    payload JSON NOT NULL,                -- event-specific data
    UNIQUE(continuity_id, seq)
);

-- Snapshots table
CREATE TABLE snapshots (
    continuity_id TEXT PRIMARY KEY,
    state JSON NOT NULL,                  -- serialized full state (context, budget, etc.)
    seq INTEGER NOT NULL,                 -- the event sequence the snapshot corresponds to
    timestamp INTEGER NOT NULL
);

-- Index
CREATE INDEX idx_events_continuity ON events(continuity_id, seq);
```

#### Event Type Definitions (Rust enum)

```rust
pub enum EventType {
    ContinuityStarted { initial_prompt: String },
    StepStarted { step_num: usize },
    LLMRequest { prompt_preview: String, tools: Vec<String> },
    LLMResponse { content: String, tool_calls: Vec<ToolCall> },
    ToolCall { tool_name: String, args: Value, result: Result<Value, String> },
    Error { error: String, recoverable: bool },
    Heartbeat { steps_since_last: usize },
    EvolutionAttempt { script_path: String, old_hash: String, new_hash: String },
    EvolutionResult { success: bool, reason: String },
    StepFinished { step_num: usize },
    ContinuityFinished { status: String, summary: String },
}
```

**Recovery flow**:
1. Read the newest snapshot from `snapshots` (ordered by `seq` descending).
2. Read all events after that `seq` from `events`.
3. Replay the events in order to rebuild the in-memory state.

### 3.2 Scheduler

**Files**: `src/scheduler/mod.rs`, `src/scheduler/loop_runner.rs`

The main loop is a finite state machine; every state is persisted through the
event store.

#### State Definitions

```rust
pub enum AgentState {
    Idle,                     // waiting for a new task
    Planning,                 // generating a task plan (calls the LLM)
    Executing,                // executing tool calls or scripts
    Evaluating,               // validating results (e.g. running tests)
    Evolving,                 // attempting to evolve after a failure
    Terminated,               // task finished or budget exhausted
}
```

#### Main Loop Pseudocode

```rust
async fn run_loop(mut ctx: Context) -> Result<()> {
    while let Some(state) = ctx.current_state {
        budget_check(&ctx)?;                      // check the budget
        let (next_state, events) = match state {
            Idle => handle_idle(&mut ctx).await,
            Planning => handle_planning(&mut ctx).await,
            Executing => handle_executing(&mut ctx).await,
            Evaluating => handle_evaluating(&mut ctx).await,
            Evolving => handle_evolving(&mut ctx).await,
            Terminated => break,
        };
        append_events(events);                    // persist all events
        ctx = apply_state_transition(ctx, next_state);
        create_snapshot_if_needed(&ctx);          // every N steps or N minutes
        sleep(100ms).await;                       // prevent CPU spin
    }
    Ok(())
}
```

### 3.3 Context Management (Context Manager)

**File**: `src/scheduler/context.rs`

Manages the LLM conversation context with a **sliding window** keeping the
most recent `K` messages, combined with summary compression.

- Data structure: `Vec<Message>`, each message has a `role`
  (system/user/assistant/tool) and `content`.
- Limit: total tokens must not exceed `MAX_CONTEXT_TOKENS` (e.g. 128k).
- Compression strategy: when the token count exceeds the threshold, the LLM
  summarizes the earlier conversation, which is replaced with one system
  message.

### 3.4 Budget Controller

**File**: `src/scheduler/budget.rs`

Hard limits that prevent infinite loops and runaway costs.

```rust
pub struct Budget {
    pub max_steps: usize,           // default 200
    pub max_input_tokens: usize,    // default 100_000
    pub max_llm_calls: usize,       // default 50
    pub max_tool_calls: usize,      // default 200
    pub max_wall_clock_secs: u64,   // default 86_400 (24 hours)
}
```

Checkpoint: `check_budget()` is called before every step; if a limit is
exceeded, the `Terminated` state is triggered and the reason recorded.

### 3.5 Capabilities and Sandbox

**Files**: `src/kernel/capability.rs`, `src/kernel/sandbox.rs`

#### Capability Tokens

Every tool call must present a valid capability token. Token structure:

```rust
pub struct Capability {
    pub resource: Resource,        // e.g. "fs:/project", "process:allowed_commands"
    pub permissions: Permissions,  // read/write/execute
    pub expires_at: Option<Instant>,
}
```

The sandbox executor validates that the caller holds the corresponding token.
By default only the main loop holds the full token; subtasks can receive
restricted tokens.

#### Execution Sandbox

All external operations (files, processes, network) go through the `Sandbox`
structure:

```rust
pub struct Sandbox {
    root_dir: PathBuf,            // allowed access root (the project directory)
    allowed_commands: HashSet<String>, // e.g. ["git", "cargo", "python"]
}
impl Sandbox {
    pub async fn read_file(&self, path: &Path) -> Result<String>;
    pub async fn write_file(&self, path: &Path, content: &str) -> Result<()>;
    pub async fn exec_command(&self, cmd: &str, args: &[String]) -> Result<CommandOutput>;
    // ... other tools
}
```

All operations are recorded in the audit log (through the `ToolCall` event).

### 3.6 LLM Gateway

**Files**: `src/llm/client.rs`, `src/llm/provider.rs`

Wraps HTTP requests and supports multiple providers through a pluggable
adapter: `openai` (OpenAI-compatible chat completions; also covers DeepSeek,
Ollama, vLLM, and any compatible gateway via `base_url`), `anthropic`
(Messages API), `gemini` (GenerateContent), and `custom` (an OpenAI-compatible
endpoint with configurable path, auth header, and extra headers).

- Uses the `reqwest` async client with retry (exponential backoff, at most 3 times).
- Supports streaming responses (optional) for real-time output.
- Supports Function Calling: converts Rust tools into each provider's schema.
- Providers own their wire formats; messages, tools, and responses are
  normalized to a single provider-agnostic model, so the scheduler, scripts,
  and evolution engine are provider-independent.

```rust
pub struct LLMClient {
    api_key: String,
    base_url: String,
    model: String,
    client: reqwest::Client,
    provider: Box<dyn LlmProvider>,
}

pub trait LlmProvider: Debug + Send + Sync {
    fn chat_path(&self, model: &str, stream: bool) -> String;
    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)>;
    fn build_body(&self, model: &str, max_output_tokens: u64, messages: &[Message],
                  tools: &[ToolDefinition], temperature: f64, stream: bool) -> Result<Value>;
    fn parse_completion(&self, body: &Value, model: &str) -> Result<LLMResponse>;
    fn stream_parser(&self) -> Box<dyn StreamParser>;
}
```

### 3.7 Script Engine

**File**: `src/evolution/script_engine.rs`

Based on the `Rhai` embedded scripting language; dynamically loads `.rhai`
files from the `scripts/` directory.

**Rust functions exposed to scripts** (via `register_fn`):

```rust
// All tool functions are wrapped as Rhai-callable
engine.register_fn("read_file", |path: String| { /* ... */ });
engine.register_fn("write_file", |path, content| { /* ... */ });
engine.register_fn("exec", |cmd, args| { /* ... */ });
engine.register_fn("llm_query", |prompt| { /* ... */ });
engine.register_fn("log", |msg: String| { tracing::info!("{}", msg) });
```

**Script loading**:

- At startup, scan all `.rhai` files under `scripts/`, compile them into an
  `AST` and cache.
- Hot reload: the `notify` crate watches file changes and recompiles
  automatically.

**Script conventions**:

- Each script file should export a main function, e.g. `execute_plan(plan)`.
- Scripts report success/failure by returning `Result<String, String>`
  (failures are raised with `throw`, which the engine converts into `Err`).

### 3.8 Evolution Engine

**File**: `src/evolution/mod.rs`

Handles script-level self-improvement.

#### Evolution Triggers

- A script returns an error (`Err`) after execution
- A tool call returns a failure (e.g. a test fails)
- The LLM evaluates the current strategy as poor

#### Evolution Flow

1. **Capture failure context**: collect the error message, the current script
   source, the most recent N conversation messages, and related events.
2. **Build the evolution prompt**: construct a system prompt asking the LLM to
   analyze the failure cause and provide improved script code.
3. **Call the LLM**: obtain the new script content via `llm_query`.
4. **Backup and atomic write**:
   - rename the original script to `.bak`
   - write the new content to the original file (atomic `write_file`)
   - trigger the script engine to reload that file
5. **Validate**: re-run the previously failed task (or a set of tests) with
   the new script.
6. **Rollback**: if validation fails, restore the backup file and record the
   failure.
7. **Record events**: generate `EvolutionAttempt` and `EvolutionResult`
   events.

**Safety measures**:
- Validate that the new script contains no dangerous operations (such as file
  deletion) through static analysis.
- Limit the number of evolution attempts (e.g. at most 5 per task).

---

## 4. Tool Inventory

All tools are implemented through the `Sandbox` and exposed as Rhai
functions.

| Tool | Rhai signature | Safety limits | Notes |
|---------|----------|---------|------|
| `read_file` | `fn read_file(path) -> String` | Path must be inside `root_dir`; file < 10MB | async |
| `write_file` | `fn write_file(path, content)` | Path limits; atomic write; < 10MB | async |
| `append_file` | `fn append_file(path, content)` | same as above | async |
| `list_dir` | `fn list_dir(path) -> Array` | Path limits | async |
| `search_code` | `fn search_code(query, path) -> Array` | Uses the `ignore` crate; text files only | async |
| `exec_command` | `fn exec_command(cmd, args) -> Object` | Command whitelist; no interaction; 120s timeout | async |
| `git_add_commit` | `fn git_add_commit(message) -> String` | Must be a Git repository; branch restrictions | async |
| `llm_query` | `fn llm_query(prompt) -> String` | Budget-restricted | async |
| `sleep` | `fn sleep(ms)` | For delays | async |
| `log_debug` | `fn log_debug(msg)` | Logging only | sync |

**Note**: the result of every tool execution is recorded as a `ToolCall`
event for auditing.

---

## 5. Data Flow and Lifecycle

A typical task execution:

1. **Startup**: the agent loads a snapshot or starts in the `Idle` state,
   waiting for a task input (via a command-line argument or FIFO file).
2. **Planning**: calls the LLM with the system instructions (including the
   available tools) and the user requirement to obtain a plan (a list of
   steps).
3. **Executing**: executes each plan step in order. Each step may invoke one
   or more tools or call script functions.
4. **Evaluating**: after the plan completes, runs the verification (such as a
   test suite). On failure, evolution is triggered.
5. **Evolving**: if validation fails, the evolution flow modifies the relevant
   script, then steps 3�? repeat.
6. **Termination**: when validation passes or the budget is exhausted, a
   final report is generated, a snapshot is saved, and the agent enters
   `Idle` waiting for a new task.

All state transitions are persisted through the event store to ensure crash
recovery.

---

## 6. Security and Reliability Design

### 6.1 Heartbeat and Watchdog
- **Internal**: a `Heartbeat` event is written after every step.
- **External**: every async operation (LLM calls, tool execution) is wrapped
  with `tokio::time::timeout`; timeouts are treated as failures and recorded.
- **System level**: systemd or Docker restart policies can monitor process
  liveness.

### 6.2 Recovery Mechanism
- On startup, automatically recover the newest snapshot + events from SQLite.
- If a continuity ID exists and is unfinished, continue it; otherwise create
  a new continuity.

### 6.3 Least Privilege
- By default only basic file-read permissions are granted.
- Writing files and executing commands require capability validation.

### 6.4 Input Validation
- All external input (file paths, command arguments) is sanitized to prevent
  injection.

---

## 7. Evolution Mechanism: Detailed Flow

**Trigger**: a script execution returns `Err`, or a tool call returns `Err`
(configurable).

**Steps**:
1. **Collect context**:
   - current continuity ID, step number
   - the error message
   - the source of the failing script (read from file)
   - the most recent 10 conversation messages
   - the most recent 5 tool call results
2. **Build the evolution prompt**: a structured prompt asking the LLM to fix
   the script.
3. **Call the LLM**: obtain the new script source via `llm_query`.
4. **Backup**: rename `scripts/<script_name>.rhai` to
   `<script_name>.rhai.bak`.
5. **Write the new script**: atomic write.
6. **Reload**: notify the script engine to recompile the script.
7. **Validate**: call a test entry of the new script (such as
   `test_<script_name>()`) or re-run the failed step via the sandbox.
8. **Handle the result**:
   - Success: keep the new script, record `EvolutionResult{success: true}`.
   - Failure: restore the backup, record
     `EvolutionResult{success: false, reason}`, and possibly lower the
     script's trust score.
9. **Limits**: at most `MAX_EVOLUTION_ATTEMPTS` (default 5) evolution attempts
   per continuity.

---

## 8. Configuration and Runtime Parameters

The configuration file is TOML (`config.toml`) and can be overridden by
command-line arguments.

```toml
[agent]
work_dir = "./workspace"
scripts_dir = "./scripts"
max_steps = 200
max_input_tokens = 100000
max_llm_calls = 50
max_evolution_attempts = 5
heartbeat_interval_secs = 60
snapshot_interval_steps = 10

[llm]
provider = "openai"
api_key = "your-api-key"
base_url = "https://api.openai.com/v1"
model = "gpt-4"
temperature = 0.7
max_output_tokens = 4096

[sandbox]
root_dir = "./workspace"
allowed_commands = ["git", "cargo", "python3", "rustc", "ls"]

[logging]
level = "info"
file = "agent.log"
```

Environment variables can override: `AGENT_API_KEY`, `AGENT_LLM_BASE_URL`,
and others.

---

## 9. Build and Deployment

### 9.1 Compilation
```bash
cargo build --release
```

### 9.2 Running
```bash
./target/release/agent --config config.toml --task "Write a Rust function to compute fibonacci"
```

### 9.3 Docker Image
A Dockerfile is provided, built with `rust:alpine`, copying the binary and the
scripts directory.

### 9.4 Log Monitoring
`tracing` outputs structured logs, integrable with `tracing-subscriber` for
file or stdout output.

---

## 10. Known Limitations and Future Extensions

### 10.1 Current Limitations
- Single-threaded async execution only.
- Evolution is limited to the script layer; the Rust kernel cannot be
  modified.
- No multi-project isolation; all tasks share the working directory.
- Depends on LLM stability; API rate limits or errors can interrupt runs.

### 10.2 Future Plans
- **Distributed execution**: Tokio multi-threading for parallel subtasks.
- **Richer tools**: database queries, network requests, Docker operations.
- **Deep-learning assistance**: fine-tune prompts or select optimal scripts
  based on historical events.
- **Self-testing**: automatically generate and run test suites.
- **Rust source-level evolution (exploratory)**: in fully controlled
  experiments, let the agent generate Rust code and hot-update functionality
  through dynamically loaded libraries (`libloading`).

---

## Appendix A: Example Rhai Script

`scripts/plan_and_execute.rhai`:

```rhai
// Receives a plan description and executes a series of steps
fn execute_plan(plan) {
    let steps = plan.steps;
    for step in steps {
        log_debug("Executing step: " + step.name);
        switch step.tool {
            "read_file" => {
                let content = read_file(step.args.path);
                // store content in context
            },
            "write_file" => {
                write_file(step.args.path, step.args.content);
            },
            "llm_query" => {
                let response = llm_query(step.args.prompt);
                // handle response
            },
            // ... other tools
        }
    }
    "Plan executed successfully"
}
```

---

## Appendix B: Event Store Example

```json
// An example event (StepStarted)
{
  "type": "StepStarted",
  "step_num": 12,
  "timestamp": 1712345678901
}
```

---

## Appendix C: Error Codes and Recovery Strategies

| Error code | Description | Recovery strategy |
|-------|------|---------|
| E001 | File not found | Try to create or skip |
| E002 | Command execution timeout | Increase the timeout or retry |
| E003 | LLM returned an error | Retry or degrade the model |
| E004 | Script syntax error | Trigger evolution to fix the script |
| E005 | Budget exhausted | Terminate the task and generate a report |
