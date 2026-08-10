# Lemon Agent Roadmap

- **Phase 0: Project initialization and technical baseline**
  - Establish the Rust 2024 project structure, dividing modules such as
    `kernel`, `scheduler`, `llm`, and `evolution`.
  - Introduce the minimal necessary dependencies and configure formatting,
    static analysis, unit tests, and CI.
  - Define a unified error type, configuration loading mechanism, and
    structured logging conventions.
  - Provide an example `config.toml` with command-line argument and
    environment variable overrides.
  - Completion criteria: the project passes `cargo build`, `cargo test`, and
    `cargo clippy` on the target platforms.

- **Phase 1: Event storage and state recovery (MVP foundation)**
  - Implement the SQLite `events` and `snapshots` tables with the necessary
    indexes.
  - Define and serialize all core event types, including task, step, LLM,
    tool, error, heartbeat, and evolution events.
  - Implement event appending, queries by continuity ID, snapshot creation,
    and event replay.
  - Implement startup recovery: load the newest snapshot and replay the
    following events to recover the unfinished task.
  - Add tests for sequence monotonicity, duplicate writes, crash recovery,
    and corrupted data handling.
  - Completion criteria: after the process exits at any point, a restart
    recovers a consistent state and continues execution.

- **Phase 2: Capability tokens, secure sandbox, and base tools**
  - Implement capability token validation for resource scope, read/write/
    execute permissions, and optional expiry.
  - Implement sandbox root confinement, path normalization, and directory
    traversal protection.
  - Implement `read_file`, `write_file`, `append_file`, `list_dir`, and
    `search_code`.
  - Implement `exec_command` with a command whitelist, argument sanitization,
    no-interaction restriction, and a 120-second timeout.
  - Implement restricted `git_add_commit`, `sleep`, and logging tools.
  - Write every tool call and its result as a `ToolCall` audit event, and
    avoid leaking secrets in logs.
  - Completion criteria: unauthorized paths, unauthorized commands, injection
    arguments, and timed-out operations are all rejected and leave auditable
    records.

- **Phase 3: LLM gateway and context management**
  - Implement an OpenAI-compatible API client with configurable model, base
    URL, key, and temperature.
  - Implement exponential backoff retries, request timeouts, error
    classification, and optional streaming responses.
  - Convert Rust tool definitions into the JSON Schema required for Function
    Calling, and parse tool call results.
  - Implement message context, a sliding window, token estimation, and
    summary compression when limits are exceeded.
  - Persist `LLMRequest` and `LLMResponse`, handling sensitive information
    and oversized content appropriately.
  - Completion criteria: plain chat, tool calls, retries, and context
    compression pass stable tests against a mock service.

- **Phase 4: Scheduler and end-to-end task loop (first usable version)**
  - Implement the `Idle`, `Planning`, `Executing`, `Evaluating`, `Evolving`,
    and `Terminated` states with legal transitions.
  - Implement budget control: caps on steps, input tokens, LLM calls, tool
    calls, and wall-clock time.
  - Check the budget before every step, and record the reason, generate a
    summary, and terminate safely when exhausted.
  - Wire together task input, LLM planning, tool execution, result
    verification, snapshots, and the final report.
  - Add timeouts to async operations, heartbeat events for executing steps,
    and brief anti-spin protection.
  - Support starting through a command-line initial task, returning to `Idle`
    after completion to await further tasks.
  - Completion criteria: the agent autonomously completes a small coding task
    in the sandbox, runs tests, and resumes unfinished tasks after a restart.

- **Phase 5: Rhai script engine and hot reload**
  - Scan and compile `scripts/*.rhai` at startup, caching valid ASTs.
  - Expose file, command, Git, LLM, delay, and logging tools to Rhai,
    constrained by capability tokens.
  - Define the script entry point, parameters, and the `Result<String, String>`
    return convention.
  - Implement hot reload with file watching; on compile failure keep the last
    usable AST.
  - Provide the `plan_and_execute.rhai` example plus script-level unit and
    integration tests.
  - Completion criteria: script modifications take effect without a restart,
    and broken scripts never break the current usable runtime.

- **Phase 6: Controlled autonomous evolution**
  - Implement configurable evolution triggers: script errors, tool failures,
    or poor evaluation results.
  - Collect the continuity ID, step number, error, script source, the most
    recent 10 messages, and the most recent 5 tool results.
  - Build a structured evolution prompt and generate candidate Rhai scripts
    through the LLM.
  - Implement backup of the old script, atomic write of the new script,
    recompilation, and isolated validation.
  - Automatically roll back on validation failure; record the script hashes,
    reasons, and evolution events for both success and failure.
  - Limit evolution attempts per continuity to 5 by default, and evaluate a
    static dangerous-operation check.
  - Completion criteria: deliberately injected script defects can be fixed and
    pass validation; failed candidates always roll back reliably.

- **Phase 7: Reliability, security hardening, and observability**
  - Cover abnormal exits, database locks, API rate limits, network
    interruptions, disk write failures, and hung subprocesses.
  - Verify that every external operation passes sandbox and capability checks
    with no bypass paths.
  - Complete structured logs, heartbeats, continuity IDs, step numbers,
    resource usage, and evolution result metrics.
  - Add long-running, fault-injection, recovery-consistency, budget-boundary,
    and cross-platform tests.
  - Align error classification, recovery strategies, and final report content
    with the E001–E005 error codes.
  - Completion criteria: passes a security review and 24-hour stability
    testing; no event loss or privilege escalation after faults.

- **Phase 8: Packaging, deployment, and v0.1.0 release**
  - Complete release-mode builds, configuration documentation, the running
    manual, and audit and recovery guides.
  - Provide a multi-stage Dockerfile packaging the binary, default scripts,
    and a minimal runtime environment.
  - Provide a Docker restart policy example and a systemd deployment example.
  - Verify build and core functionality compatibility on Linux, Windows, and
    macOS.
  - Establish a version migration strategy, especially for SQLite schema and
    script interface changes.
  - Completion criteria: a new environment can be deployed per the
    documentation and passes end-to-end task, crash recovery, and log audit
    acceptance.

- **Post-v0.1.0 roadmap**
  - Introduce multi-project workspace isolation and finer-grained capability
    tokens.
  - Use Tokio multi-threading for budget-constrained parallel subtasks;
    distributed execution is evaluated as a separate long-term project.
  - Extend tools such as database queries, controlled network requests, and
    Docker operations.
  - Automatically generate and run test suites to improve the reliability of
    evaluation and evolution.
  - Optimize prompts and script selection based on historical events while
    keeping everything explainable, rollback-safe, and auditable.
  - Explore dynamic-library-based Rust hot updates in fully isolated
    experiments only; not part of default production capabilities.

- **Cross-cutting quality gates**
  - Every phase includes unit tests, key integration tests, and corresponding
    documentation.
  - Security boundaries, event persistence, budget termination, and rollback
    mechanisms are not post-release additions.
  - Every external side effect must be traceable, every long-running
    operation must be timeout-bounded, and every automated change must be
    recoverable.
  - Prioritize a reliable single-machine, single-task, script-level evolution
    closed loop before extending concurrency and advanced tools.
