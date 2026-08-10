# Lemon Agent

Lemon Agent is an unattended, long-running autonomous programming agent built
in Rust. It accepts a programming goal and continuously plans, modifies code,
runs commands, tests, debugs, and evaluates results �?and improves its own
Rhai execution scripts within a safe boundary.

> **Status: v0.1.0 released.** The full single-machine loop is working:
> planning, script-driven execution, verification, event-sourced persistence
> with crash recovery, hot-reloadable Rhai strategies, and validated
> self-evolution with rollback. See [ROADMAP.md](./ROADMAP.md) for the phase
> record and [running.md](./running.md) to deploy.

## Quick start

```bash
cargo build --release
export AGENT_API_KEY="sk-..."
./target/release/lemon-agent --config config.toml --task "implement fibonacci and test it"
# prints: status: completed / continuity: <id> / steps: N / summary: ...
```

Docker (`docker build -t lemon-agent .`) and systemd
(`deploy/lemon-agent.service`) deployments are provided. Details in
[running.md](running.md).

## What the finished Lemon Agent does

Lemon Agent runs as a single-machine daemon. You provide a working directory,
configuration, and an initial task; it then drives the task autonomously until
verification passes, a budget limit is reached, or it hits a problem it cannot
safely recover from.

A typical run looks like:

```bash
./target/release/agent \
  --config config.toml \
  --task "add rate limiting to this Rust project and add tests"
```

During the run, the agent will:

1. Understand the task and produce an executable plan.
2. Search and read code within the restricted working directory.
3. Call authorized file, process, Git, and LLM tools.
4. Modify code and run formatting, compilation, and test commands.
5. Fix, refactor, or finish based on verification results.
6. Persist every step for auditing and crash recovery.
7. Improve the Rhai strategy scripts when needed, rolling back automatically
   when validation fails.

## Core capabilities

- **Autonomous task loop**: drives tasks through the
  `Planning �?Executing �?Evaluating �?Evolving` state machine.
- **Code operations**: safely read, write, append, list, and search files in
  the working directory.
- **Commands and tests**: run whitelisted non-interactive commands such as
  `git`, `cargo`, `rustc`, and `python3`.
- **Version control**: stage and commit verified changes within the
  restricted repository scope.
- **LLM tool calls**: connect to OpenAI-compatible APIs with structured tool
  definitions, retries, and optional streaming.
- **Context compression**: sliding window plus summaries keep long sessions
  within context limits.
- **Budget control**: caps on steps, tokens, LLM calls, tool calls, and total
  runtime prevent runaway loops and cost overruns.
- **Event sourcing**: tasks, steps, tools, LLM calls, errors, heartbeats, and
  evolution results are written to SQLite.
- **Crash recovery**: state is restored from the newest snapshot plus
  following events, continuing unfinished continuities.
- **Script hot reload**: `scripts/*.rhai` is loaded dynamically; strategy
  updates take effect without a restart.
- **Controlled evolution**: candidate strategy scripts are generated from
  failure context and only replace the live script after compilation and
  validation.

## System shape

```text
Task input / external API
        �?        �?Rust scheduler ── state machine, context, budget, recovery
        �?        �?Capability & sandbox layer ── capability tokens, path confinement, command whitelist, audit
        �?        �?Rhai script engine ── hot-reloadable execution strategy
        �?        �?Evolution engine ── failure analysis, candidate generation, validation & rollback

The SQLite event store runs through every layer, holding events and snapshots.
```

The Rust kernel owns the non-bypassable scheduling, security, persistence, and
budget constraints; the Rhai layer owns the replaceable high-level execution
strategy. The agent can evolve script behavior but never modifies the Rust
security kernel itself.

## Security boundary

Autonomous operation does not mean unlimited permissions. The system follows
the principle of least privilege by default:

- All file access is confined to the configured `root_dir`, with directory
  traversal blocked.
- Writing files, executing commands, and calling external capabilities
  require capability token validation.
- External commands must be whitelisted, non-interactive, and timeout-bounded.
- Every external side effect becomes a queryable audit event.
- Sensitive configuration such as API keys never appears in ordinary logs or
  LLM prompt previews.
- Every async operation has a timeout; every task has a hard budget.
- Newly generated Rhai scripts must compile and pass validation; on failure
  the previous version is restored.
- Evolution attempts per continuity are limited (5 by default).

## Recoverable and auditable runtime

Lemon Agent keeps an immutable event log and periodic state snapshots in
`agent.db`. Even if the process is killed, a restart can:

- find the most recent unfinished continuity,
- restore context, state, and budget usage from the snapshot,
- replay the events after the snapshot to rebuild a consistent in-memory
  state,
- continue from a safe step boundary.

Structured logs expose current state, steps, tool calls, resource usage, and
final results �?no GUI or interactive terminal required.

## Project layout

```text
.
├── src/
�?  ├── kernel/
�?  �?  ├── capability.rs
�?  �?  ├── event_store.rs
�?  �?  └── sandbox.rs
�?  ├── scheduler/
�?  �?  ├── budget.rs
�?  �?  ├── context.rs
�?  �?  ├── loop_runner.rs
�?  �?  ├── mod.rs
�?  �?  └── plan.rs
�?  ├── llm/
�?  �?  └── client.rs
�?  ├── evolution/
�?  �?  ├── mod.rs
�?  �?  └── script_engine.rs
�?  └── main.rs
├── scripts/
�?  └── plan_and_execute.rhai
├── docs/
�?  ├── en/          # English documentation
�?  └── zh-CN/       # Chinese documentation
├── deploy/          # systemd and Docker Compose examples
├── Dockerfile
├── config.toml
├── agent.db
└── README.md        # bilingual entry point
```

`workspace/` is the project directory the agent is allowed to operate on,
`scripts/` holds the hot-updatable behavior strategies, and `agent.db` holds
events and snapshots.

## Example configuration

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
base_url = "https://api.openai.com/v1"
model = "gpt-4"
temperature = 0.7

[sandbox]
root_dir = "./workspace"
allowed_commands = ["git", "cargo", "python3", "rustc"]

[logging]
level = "info"
file = "agent.log"
```

The LLM key is provided through the `AGENT_API_KEY` environment variable to
keep it out of the committed configuration file.

## v0.1.0 completion criteria

The first stable release:

- builds and runs on Linux, Windows, and macOS,
- completes a small real coding task in the sandbox and passes its tests,
- provides a complete audit record for every step and external side effect,
- recovers and continues unfinished tasks after an abnormal exit,
- stops safely with a report on budget exhaustion, external timeouts, or
  unrecoverable errors,
- hot-reloads Rhai strategy scripts and reliably validates or rolls back
  autonomous improvements,
- passes a security review, fault-injection tests, and at least 24 hours of
  stability testing.

> v0.1.0 acceptance status: all project gates (fmt / clippy / test) pass;
> sandbox end-to-end tasks, crash recovery, budget boundaries, script hot
> reload, evolution fixes and rollback, and stability cycle tests are
> automated. The 24-hour soak test ships as an `#[ignore]` test
> (`cargo test --test stability -- --ignored`).

## Explicit non-goals

v0.1.0 does not provide:

- a graphical interface or interactive terminal,
- distributed clusters or parallel multi-agent scheduling,
- strong isolation in shared multi-project environments,
- autonomous modification of the Rust kernel source,
- unrestricted network, shell, or host access.

These limits let the first release focus on one goal: a reliable, secure,
recoverable, auditable single-machine autonomous programming loop.

## Documentation

- [Technical specification](./SPECS.md)
- [Project roadmap](./ROADMAP.md)
- [Coding standards](./CODESTYLE.md)
- [Running guide](./running.md)
- [Audit and recovery](./audit-and-recovery.md)
- [Error codes](./error-codes.md)
- [Versioning and migrations](./migrations.md)
