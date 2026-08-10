# Code Style and Engineering Standards

This document defines the mandatory coding standards for Lemon Agent. All production code, tests, documentation, configuration examples, commit messages, logs, errors, and developer-facing text must follow these rules.

## 1. Language

- Use English everywhere in the codebase.
- Write identifiers, comments, documentation, log messages, error messages, test names, configuration keys, and examples in English.
- Do not mix languages in source files or developer documentation.
- Prefer clear, direct wording over abbreviations or internal jargon.

## 2. Production-Ready Code

- Write complete, production-ready implementations rather than prototypes or placeholders.
- Do not commit `TODO`, `FIXME`, stub implementations, mock return values, silent fallbacks, or commented-out code unless an issue reference and a concrete reason are included.
- Handle expected failures explicitly and preserve useful diagnostic context.
- Avoid `unwrap`, `expect`, `panic!`, and unreachable assumptions in production paths. Use them in tests only when they make the test clearer.
- Validate all external input at the system boundary.
- Treat filesystem access, subprocess execution, network calls, database operations, and LLM responses as fallible.
- Apply timeouts and resource limits to operations that may block or consume unbounded resources.
- Keep security checks in the Rust core and never rely on scripts or prompts as a security boundary.

## 3. Project Structure

- Organize code by responsibility and domain, not by convenience or file size alone.
- Keep the dependency direction explicit: high-level orchestration may depend on stable interfaces, while core security and persistence modules must not depend on higher-level policy code.
- Use small, cohesive modules with a clear public API.
- Keep implementation details private by default. Expose only what another module genuinely needs.
- Separate pure domain logic from I/O, persistence, networking, and process execution.
- Prefer composition and traits over global state or tightly coupled concrete implementations.
- Put shared test utilities in dedicated test-support modules rather than production modules.
- Do not create generic `utils`, `helpers`, or `common` modules when a precise domain name is available.

## 4. Readability and Naming

- Choose names that communicate intent without requiring comments.
- Use Rust naming conventions:
  - `snake_case` for functions, methods, modules, variables, and file names.
  - `PascalCase` for structs, enums, traits, and enum variants.
  - `SCREAMING_SNAKE_CASE` for constants and statics.
- Name booleans as predicates, such as `is_valid`, `has_capacity`, or `should_retry`.
- Include units in names when ambiguity is possible, such as `timeout_secs`, `size_bytes`, or `timestamp_ms`.
- Avoid unexplained abbreviations and one-letter names outside small local scopes.
- Keep functions focused on one responsibility. Extract code when a function mixes policy, I/O, transformation, and error handling.
- Prefer early returns to deeply nested control flow.
- Optimize for clarity first; optimize performance only with evidence or a clearly documented constraint.

## 5. Comments and Documentation

- Write comments only when they provide necessary technical context that the code cannot express clearly.
- Prefer explaining why a constraint, invariant, workaround, or safety decision exists. Do not narrate what the next line does.
- Keep comments concise, precise, and technically meaningful.
- Remove stale, redundant, speculative, decorative, and conversational comments.
- Do not use comments to compensate for unclear names or poor structure; improve the code instead.
- Document public APIs when their contract, invariants, failure modes, security implications, or side effects are not obvious.
- For unsafe code, document every required safety invariant immediately next to the unsafe block.
- For non-obvious workarounds, include the underlying cause and a stable reference when available.

## 6. Debuggability and Observability

- Make every important operation diagnosable without attaching a debugger.
- Use structured logging with stable field names rather than interpolated prose alone.
- Include relevant correlation fields such as `continuity_id`, `step_num`, `event_type`, `tool_name`, and elapsed time.
- Log state transitions, retries, timeouts, recoverable failures, budget exhaustion, evolution attempts, and rollback results.
- Use log levels consistently:
  - `error`: the current operation cannot complete or data integrity may be at risk.
  - `warn`: the operation recovered, degraded, or requires attention.
  - `info`: meaningful lifecycle and state changes.
  - `debug`: diagnostic detail useful during development or incident analysis.
  - `trace`: high-volume internal detail disabled in normal production runs.
- Never log API keys, authorization headers, capability secrets, raw credentials, or unredacted sensitive content.
- Preserve error sources and add context at each abstraction boundary.
- Prefer typed errors that callers can inspect over matching arbitrary error strings.
- Ensure failures identify the operation, relevant safe identifiers, and a useful cause.

## 7. Rust-Specific Rules

- Use stable Rust with the edition selected by the project manifest.
- Follow idiomatic ownership and borrowing; avoid cloning merely to silence borrow-checker errors.
- Prefer explicit domain types over loosely related primitive values.
- Use enums for closed state sets and make invalid states difficult to represent.
- Mark return values with `#[must_use]` when silently ignoring them is likely to cause a bug.
- Keep async critical sections short and never hold a synchronous lock across `.await`.
- Use bounded channels and bounded concurrency unless an unbounded design is justified.
- Avoid `unsafe`. If it is unavoidable, isolate it behind a safe API and test its invariants.
- Keep feature flags additive, documented, and covered by CI when supported.

## 8. Error Handling and Recovery

- Use `Result` for operations that can fail and propagate errors intentionally.
- Define errors at appropriate domain boundaries; do not expose low-level implementation details unnecessarily.
- Add context without discarding the original error chain.
- Distinguish retryable, recoverable, terminal, validation, authorization, and budget errors where behavior differs.
- Make retries bounded and use backoff for external services.
- Ensure retrying an operation is safe, or explicitly implement idempotency protection.
- Do not swallow errors. If an error is intentionally ignored, document and log the reason at the appropriate level.
- Keep rollback and recovery paths as rigorously tested as success paths.

## 9. Security

- Apply least privilege by default.
- Normalize and validate paths before access, and verify that resolved paths remain inside the configured sandbox root.
- Never execute commands through an unrestricted shell when an argument-based process API is sufficient.
- Validate executable names and arguments independently against policy.
- Use atomic writes for state, configuration, and evolvable scripts where partial output would be unsafe.
- Redact secrets before persistence, logging, or inclusion in an LLM prompt.
- Treat tool output and LLM output as untrusted input.
- Enforce authorization in code at the point of use, not only at API entry points.

## 10. Tests

- Add or update tests for every behavior change and bug fix.
- Use unit tests for pure logic and focused contracts.
- Use integration tests for module boundaries, persistence, sandbox behavior, subprocesses, and recovery flows.
- Include negative tests for invalid input, denied capabilities, path traversal, timeouts, budget exhaustion, malformed responses, and rollback failure.
- Keep tests deterministic, isolated, and safe to run in parallel.
- Do not depend on live external services in the default test suite. Use controlled fakes or local test servers.
- Assert observable behavior and contracts rather than private implementation details.
- Give tests descriptive names that state the condition and expected outcome.
- A flaky test is a defect and must be fixed or removed with an explicit explanation.

## 11. Formatting and Static Analysis

- Always format Rust code with `cargo fmt` before considering work complete.
- Run `cargo clippy --all-targets --all-features -- -D warnings` and resolve all warnings.
- Run the complete relevant test suite with `cargo test --all-targets --all-features`.
- Keep Markdown, TOML, SQL, JSON, YAML, and Rhai files consistently formatted with the project's configured tools.
- Do not manually align code in ways that conflict with automated formatters.
- Do not suppress lints globally to avoid fixing local problems. Use the narrowest suppression and document the reason.
- Generated files must be clearly identified and must not be edited manually.

## 12. Dependencies

- Add dependencies only when they provide clear value that is not reasonably covered by the standard library or an existing dependency.
- Prefer maintained, well-audited crates with minimal required features.
- Disable default features when they introduce unnecessary capabilities or dependency weight.
- Keep dependencies pinned through `Cargo.lock` for reproducible application builds.
- Review security advisories, licensing, and transitive dependency impact before adoption.
- Do not introduce multiple crates for the same responsibility without a documented reason.

## 13. Configuration and Compatibility

- Use configuration for operational policy, not for hiding incomplete behavior.
- Provide secure and useful defaults.
- Validate configuration at startup and report all actionable validation failures clearly.
- Keep environment-variable and command-line overrides explicit and documented.
- Treat persisted event formats, database schemas, configuration keys, and script interfaces as versioned contracts.
- Provide migrations or explicit compatibility errors for breaking persisted-data changes.

## 14. Change Discipline

- Keep changes focused and avoid unrelated refactoring in the same change set.
- Preserve backward compatibility unless a breaking change is intentional and documented.
- Update documentation and examples whenever public behavior or configuration changes.
- Include tests that demonstrate a fixed defect and prevent regression.
- Do not leave the repository in a state that fails formatting, static analysis, compilation, or tests.

## 15. Definition of Done

A change is complete only when all applicable conditions are met:

- The implementation is complete, secure, and production-ready.
- All text added to the repository is in English.
- The code follows the intended module boundaries and has no unnecessary public surface.
- Necessary technical comments and public API documentation are accurate and concise.
- Errors contain actionable context, and relevant operations are observable through safe structured logs.
- Tests cover success, failure, and boundary conditions.
- Formatting, static analysis, compilation, and relevant tests pass.
- Documentation, configuration examples, and migrations are updated where required.
- No secrets, temporary debug output, placeholders, dead code, or unrelated artifacts remain.
