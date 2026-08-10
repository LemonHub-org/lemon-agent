# Error Codes and Recovery Strategy

Every failure surfaced by the agent carries a stable code. The recovery
strategy below mirrors SPECS.md Appendix C and is enforced by the scheduler,
sandbox, and LLM gateway.

| Code | Class | Meaning | Recovery strategy |
|------|-------|---------|-------------------|
| E001 | FileNotFound | The requested file does not exist. | `read_file`/`list_dir` fail loudly; `write_file`/`append_file` create missing parents. |
| E002 | CommandTimeout | A sandboxed command exceeded its time limit. | Reported in `CommandOutput.timed_out`; retryable, child process is killed. |
| E003 | Llm | The LLM gateway failed (HTTP errors, malformed payloads, stream failure). | Transient failures (429, 5xx, network, timeouts) are retried with exponential backoff; client errors fail without retry. |
| E004 | Script | A Rhai script failed to compile or run. | The evolution engine is triggered: a candidate script is generated, compiled, validated in isolation, and rolled back on failure. |
| E005 | BudgetExhausted | Steps, input tokens, LLM calls, tool calls, or wall-clock limits reached. | The continuity terminates safely and a final report with usage is persisted. |
| E006 | CapabilityDenied | The caller lacks the required capability token. | Denied at the point of use; audited as a `ToolCall` error. |
| E007 | PathViolation | A path escapes the sandbox root. | Rejected before any filesystem access; audited. |
| E008 | Io | A filesystem operation failed. | Propagated with the path; atomic writes prevent partial state. |
| E009 | Database | The SQLite event store failed. | WAL mode and a 5s busy timeout; corruption or lock failures fail loudly. |
| E010 | InvalidConfig | Configuration is invalid. | All problems are reported at startup before any work begins. |
| E011 | InvalidInput | External input is invalid (malformed plans, injection attempts). | Rejected; plans are retried once and then the continuity fails. |
| E012 | Timeout | An asynchronous operation exceeded its time limit. | Retryable; bounding is enforced on every blocking operation. |
| E013 | RetryExhausted | A retryable operation failed after the configured attempts. | Reported with the attempt count; the caller decides recovery. |
| E014 | Http | A network request failed outside the LLM gateway. | Propagated with the underlying cause. |
| E015 | Json | Serialization or deserialization failed. | Propagated; persisted data is never silently reinterpreted. |
| E016 | AtomicWrite | An atomic file write failed. | Reported with the target path; temporary files are cleaned up. |
| E017 | EvolutionRejected | Evolution produced a rejected or invalid script. | The previous script version is restored and reloaded. |
| E018 | Internal | An invariant was violated (event log gaps, poisoned state). | The agent stops loudly; the log is never continued from inconsistent state. |

## Final reports

A `ContinuityFinished` event and the printed report always contain:

- the continuity ID,
- the terminal status (`completed`, `failed`, `budget_exhausted`, or `idle`),
- the step count, and
- a summary including the error code when the run failed.
