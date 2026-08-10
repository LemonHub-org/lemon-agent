# Versioning and Migrations

Two persisted contracts need versioning discipline: the SQLite schema and the
Rhai script interface.

## SQLite schema

The schema version is stored in SQLite's `PRAGMA user_version` (currently
`1`). On open, the agent:

- refuses databases with `user_version` greater than the binary's
  `SCHEMA_VERSION` (a downgrade would silently discard data), and
- upgrades older databases by running the idempotent schema batch and bumping
  `user_version`.

Adding a migration:

1. Bump `SCHEMA_VERSION` in `src/kernel/event_store.rs`.
2. Add the DDL to the migration path in `migrate()` (the batch is idempotent,
   so `CREATE TABLE IF NOT EXISTS` and additive statements are safe).
3. Add a test for the old → new upgrade (see `older_schema_version_is_upgraded`).

## Script interface

The strategy script contract is the `execute_plan(plan)` entry point plus the
tool names documented in `SPECS.txt`:

- Scripts must export `fn execute_plan(plan)`; the engine refuses to load
  anything else.
- The optional self-test entry is `test_<script_name>()`; evolution requires
  it before a candidate may replace the live script.
- Scripts only call the registered tool functions; anything else fails to
  compile.

Changing the interface is a breaking change for:

- scripts shipped in the repo (`scripts/plan_and_execute.rhai`), which must be
  updated in the same release, and
- the evolution prompt in `src/evolution/mod.rs`, which must document the new
  contract so candidates conform.

Validate script compatibility after any interface change by running
`cargo test --test script_engine --test evolution`.
