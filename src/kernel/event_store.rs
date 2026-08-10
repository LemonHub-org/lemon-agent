//! Event-sourced persistence backed by SQLite.
//!
//! All agent activity is appended to an immutable `events` log. Periodic
//! `snapshots` allow recovery: load the newest snapshot and replay the events
//! that follow it to reconstruct the in-memory state.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Error, Result};

/// Current schema version tracked via `PRAGMA user_version`.
const SCHEMA_VERSION: i64 = 1;

/// The outcome of a tool call as persisted in a `ToolCall` event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "outcome", content = "value", rename_all = "snake_case")]
pub enum ToolOutcome {
    Ok(Value),
    Err(String),
}

/// A structured function call requested by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Every event type the agent can persist.
///
/// Serialized with a `type` tag and a `timestamp` in Unix milliseconds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "PascalCase")]
pub enum EventType {
    /// A new continuity session was started.
    ContinuityStarted { initial_prompt: String },
    /// The agent entered a new step.
    StepStarted { step_num: usize },
    /// An LLM request was sent. `prompt_preview` is truncated for privacy.
    LlmRequest {
        prompt_preview: String,
        tools: Vec<String>,
    },
    /// The LLM responded with content and optional tool calls.
    LlmResponse {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    /// A sandboxed tool was invoked and produced an outcome.
    ToolCall {
        tool_name: String,
        args: Value,
        result: ToolOutcome,
    },
    /// An error was recorded. `recoverable` tells the scheduler whether the
    /// continuity can continue.
    Error { error: String, recoverable: bool },
    /// Periodic liveness marker while steps are running.
    Heartbeat { steps_since_last: usize },
    /// An evolution candidate replaced a script.
    EvolutionAttempt {
        script_path: String,
        old_hash: String,
        new_hash: String,
    },
    /// The outcome of an evolution attempt.
    EvolutionResult { success: bool, reason: String },
    /// The agent finished a step.
    StepFinished { step_num: usize },
    /// The continuity terminated. `status` is "completed", "budget_exhausted",
    /// or "failed"; `summary` is the final report text.
    ContinuityFinished { status: String, summary: String },
}

impl EventType {
    /// The human-readable event type name, e.g. "StepStarted".
    pub fn name(&self) -> &'static str {
        match self {
            EventType::ContinuityStarted { .. } => "ContinuityStarted",
            EventType::StepStarted { .. } => "StepStarted",
            EventType::LlmRequest { .. } => "LlmRequest",
            EventType::LlmResponse { .. } => "LlmResponse",
            EventType::ToolCall { .. } => "ToolCall",
            EventType::Error { .. } => "Error",
            EventType::Heartbeat { .. } => "Heartbeat",
            EventType::EvolutionAttempt { .. } => "EvolutionAttempt",
            EventType::EvolutionResult { .. } => "EvolutionResult",
            EventType::StepFinished { .. } => "StepFinished",
            EventType::ContinuityFinished { .. } => "ContinuityFinished",
        }
    }
}

/// A stored event with its sequence number and timestamp.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredEvent {
    pub continuity_id: String,
    pub seq: u64,
    pub timestamp_ms: u64,
    #[serde(flatten)]
    pub event: EventType,
}

/// A state snapshot associated with a specific event sequence number.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredSnapshot {
    pub continuity_id: String,
    pub seq: u64,
    pub timestamp_ms: u64,
    pub state: Value,
}

/// One row of the continuity overview used by the TUI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContinuitySummary {
    pub continuity_id: String,
    pub steps: usize,
    pub started_at_ms: u64,
    pub finished: bool,
}

/// The SQLite-backed event store.
///
/// All methods are synchronous and short-lived; callers must not hold the
/// internal lock across an await point.
#[derive(Debug)]
pub struct EventStore {
    conn: Mutex<Connection>,
}

impl EventStore {
    /// Open (or create) the event store at `path`.
    pub fn open(path: &Path) -> Result<EventStore> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io(Some(parent.to_path_buf()), e))?;
        }
        let conn = Connection::open(path).map_err(Error::Database)?;
        migrate(&conn)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(Error::Database)?;
        conn.pragma_update(None, "busy_timeout", 5000)
            .map_err(Error::Database)?;
        conn.execute_batch(SCHEMA).map_err(Error::Database)?;
        Ok(EventStore {
            conn: Mutex::new(conn),
        })
    }

    /// The current schema version of an existing database file.
    pub fn schema_version(path: &Path) -> Result<i64> {
        let conn = Connection::open(path).map_err(Error::Database)?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(Error::Database)?;
        Ok(version)
    }

    /// Append a single event, returning its sequence number.
    pub fn append(&self, continuity_id: &str, event: &EventType) -> Result<u64> {
        let seq = self.append_many(continuity_id, std::slice::from_ref(event))?;
        Ok(seq[0])
    }

    /// Append events atomically, returning their sequence numbers.
    pub fn append_many(&self, continuity_id: &str, events: &[EventType]) -> Result<Vec<u64>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::Internal("event store mutex poisoned".to_string()))?;
        let mut seqs = Vec::with_capacity(events.len());
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(Error::Database)?;
        let result = (|| -> Result<Vec<u64>> {
            for event in events {
                let seq: i64 = conn
                    .query_row(
                        "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE continuity_id = ?1",
                        [continuity_id],
                        |row| row.get(0),
                    )
                    .map_err(Error::Database)?;
                let seq = seq as u64;
                let timestamp_ms = now_ms();
                conn.execute(
                    "INSERT INTO events (continuity_id, seq, timestamp, event_type, payload)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        continuity_id,
                        seq as i64,
                        timestamp_ms as i64,
                        event.name(),
                        serde_json::to_string(event).map_err(Error::Json)?
                    ],
                )
                .map_err(Error::Database)?;
                seqs.push(seq);
            }
            Ok(seqs)
        })();
        match result {
            Ok(seqs) => {
                conn.execute_batch("COMMIT").map_err(Error::Database)?;
                Ok(seqs)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// All events for `continuity_id` with `seq > after_seq`, in order.
    pub fn events_after(&self, continuity_id: &str, after_seq: u64) -> Result<Vec<StoredEvent>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::Internal("event store mutex poisoned".to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT continuity_id, seq, timestamp, payload
                 FROM events WHERE continuity_id = ?1 AND seq > ?2 ORDER BY seq",
            )
            .map_err(Error::Database)?;
        let rows = stmt
            .query_map(rusqlite::params![continuity_id, after_seq as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(Error::Database)?;
        let mut events = Vec::new();
        for row in rows {
            let (continuity_id, seq, timestamp_ms, payload) = row.map_err(Error::Database)?;
            let event: EventType = serde_json::from_str(&payload).map_err(Error::Json)?;
            events.push(StoredEvent {
                continuity_id,
                seq: seq as u64,
                timestamp_ms: timestamp_ms as u64,
                event,
            });
        }
        Ok(events)
    }

    /// The highest sequence number for a continuity, or 0 when absent.
    pub fn max_seq(&self, continuity_id: &str) -> Result<u64> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::Internal("event store mutex poisoned".to_string()))?;
        let seq: Option<i64> = conn
            .query_row(
                "SELECT MAX(seq) FROM events WHERE continuity_id = ?1",
                [continuity_id],
                |row| row.get(0),
            )
            .map_err(Error::Database)?;
        Ok(seq.unwrap_or(0) as u64)
    }

    /// The newest snapshot for `continuity_id`, if any.
    pub fn latest_snapshot(&self, continuity_id: &str) -> Result<Option<StoredSnapshot>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::Internal("event store mutex poisoned".to_string()))?;
        let snapshot: Option<(i64, i64, String)> = conn
            .query_row(
                "SELECT seq, timestamp, state FROM snapshots WHERE continuity_id = ?1",
                [continuity_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(Error::Database)?;
        Ok(snapshot.map(|(seq, timestamp_ms, state)| StoredSnapshot {
            continuity_id: continuity_id.to_string(),
            seq: seq as u64,
            timestamp_ms: timestamp_ms as u64,
            state: serde_json::from_str(&state)
                .map_err(Error::Json)
                .unwrap_or(json!({})),
        }))
    }

    /// Upsert the snapshot for `continuity_id`.
    pub fn save_snapshot(&self, continuity_id: &str, seq: u64, state: &Value) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::Internal("event store mutex poisoned".to_string()))?;
        conn.execute(
            "INSERT INTO snapshots (continuity_id, seq, timestamp, state)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(continuity_id) DO UPDATE SET
               seq = excluded.seq,
               timestamp = excluded.timestamp,
               state = excluded.state
             WHERE excluded.seq > snapshots.seq",
            rusqlite::params![
                continuity_id,
                seq as i64,
                now_ms() as i64,
                serde_json::to_string(state).map_err(Error::Json)?
            ],
        )
        .map_err(Error::Database)?;
        Ok(())
    }

    /// Continuity IDs that have not yet finished, ordered by newest start.
    pub fn incomplete_continuities(&self) -> Result<Vec<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::Internal("event store mutex poisoned".to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT e.continuity_id
                 FROM events e
                 WHERE NOT EXISTS (
                     SELECT 1 FROM events f
                     WHERE f.continuity_id = e.continuity_id
                       AND f.event_type = 'ContinuityFinished'
                 )
                 GROUP BY e.continuity_id
                 ORDER BY MAX(e.seq) DESC",
            )
            .map_err(Error::Database)?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(Error::Database)?;
        let mut out = Vec::new();
        for id in ids {
            out.push(id.map_err(Error::Database)?);
        }
        Ok(out)
    }

    /// A compact overview of every continuity, newest last-event first.
    pub fn continuity_summaries(&self) -> Result<Vec<ContinuitySummary>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::Internal("event store mutex poisoned".to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT e.continuity_id,
                        COUNT(CASE WHEN e.event_type = 'StepStarted' THEN 1 END),
                        MIN(e.timestamp),
                        EXISTS(SELECT 1 FROM events f
                               WHERE f.continuity_id = e.continuity_id
                                 AND f.event_type = 'ContinuityFinished')
                 FROM events e
                 GROUP BY e.continuity_id
                 ORDER BY MAX(e.seq) DESC",
            )
            .map_err(Error::Database)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(Error::Database)?;
        let mut summaries = Vec::new();
        for row in rows {
            let (continuity_id, steps, started_at_ms, finished) = row.map_err(Error::Database)?;
            summaries.push(ContinuitySummary {
                continuity_id,
                steps: steps.max(0) as usize,
                started_at_ms: started_at_ms.max(0) as u64,
                finished: finished != 0,
            });
        }
        Ok(summaries)
    }

    /// The newest snapshot across all continuities.
    pub fn newest_snapshot(&self) -> Result<Option<StoredSnapshot>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::Internal("event store mutex poisoned".to_string()))?;
        let snapshot: Option<(String, i64, i64, String)> = conn
            .query_row(
                "SELECT continuity_id, seq, timestamp, state FROM snapshots
                 ORDER BY timestamp DESC, seq DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(Error::Database)?;
        Ok(
            snapshot.map(|(continuity_id, seq, timestamp_ms, state)| StoredSnapshot {
                continuity_id,
                seq: seq as u64,
                timestamp_ms: timestamp_ms as u64,
                state: serde_json::from_str(&state)
                    .map_err(Error::Json)
                    .unwrap_or(json!({})),
            }),
        )
    }

    /// Verify that a continuity's event sequence is gapless from
    /// `expected_next` to its maximum. Fails loudly when any event was lost,
    /// so recovery never silently continues from an inconsistent log.
    pub fn verify_continuity(&self, continuity_id: &str, expected_next: u64) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::Internal("event store mutex poisoned".to_string()))?;
        let max_seq: Option<i64> = conn
            .query_row(
                "SELECT MAX(seq) FROM events WHERE continuity_id = ?1",
                [continuity_id],
                |row| row.get(0),
            )
            .map_err(Error::Database)?;
        let Some(max_seq) = max_seq else {
            return Ok(());
        };
        let max_seq = max_seq as u64;
        if expected_next > max_seq {
            return Err(Error::Internal(format!(
                "event log for {continuity_id} is behind the snapshot: snapshot seq {expected_next} > max seq {max_seq}"
            )));
        }
        if expected_next == max_seq {
            return Ok(());
        }
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE continuity_id = ?1 AND seq BETWEEN ?2 AND ?3",
                rusqlite::params![continuity_id, expected_next as i64, max_seq as i64],
                |row| row.get(0),
            )
            .map_err(Error::Database)?;
        let expected = max_seq - expected_next + 1;
        if count as u64 != expected {
            return Err(Error::Internal(format!(
                "event log for {continuity_id} has a gap: expected {} events in [{expected_next}, {max_seq}], found {count}",
                expected
            )));
        }
        Ok(())
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    continuity_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    timestamp INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    payload JSON NOT NULL,
    UNIQUE(continuity_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_events_continuity ON events(continuity_id, seq);
CREATE TABLE IF NOT EXISTS snapshots (
    continuity_id TEXT PRIMARY KEY,
    state JSON NOT NULL,
    seq INTEGER NOT NULL,
    timestamp INTEGER NOT NULL
);
"#;

/// Migrate a database to the current schema version.
///
/// Databases from a newer binary are refused: downgrading would silently
/// discard data the binary cannot interpret. Older versions are upgraded by
/// running every migration in order; the idempotent `SCHEMA` batch is safe to
/// apply repeatedly.
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(Error::Database)?;
    if version > SCHEMA_VERSION {
        return Err(Error::Internal(format!(
            "database schema version {version} is newer than this binary supports ({SCHEMA_VERSION}); refusing to open"
        )));
    }
    if version < SCHEMA_VERSION {
        tracing::info!(
            from = version,
            to = SCHEMA_VERSION,
            "migrating event store schema"
        );
        conn.execute_batch(SCHEMA).map_err(Error::Database)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(Error::Database)?;
    }
    Ok(())
}

/// Current wall-clock time in Unix milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp() -> (tempfile::TempDir, EventStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(&dir.path().join("test.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn sequences_are_monotonic_per_continuity() {
        let (_dir, store) = open_temp();
        let seqs = store
            .append_many(
                "c1",
                &[
                    EventType::ContinuityStarted {
                        initial_prompt: "task".into(),
                    },
                    EventType::StepStarted { step_num: 1 },
                    EventType::Heartbeat {
                        steps_since_last: 0,
                    },
                ],
            )
            .unwrap();
        assert_eq!(seqs, vec![1, 2, 3]);

        let seqs2 = store
            .append_many("c2", &[EventType::StepStarted { step_num: 1 }])
            .unwrap();
        assert_eq!(seqs2, vec![1]);
    }

    #[test]
    fn duplicate_sequence_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = EventStore::open(&path).unwrap();
        store
            .append("c1", &EventType::StepStarted { step_num: 1 })
            .unwrap();

        // A direct duplicate (continuity_id, seq) insert must violate UNIQUE.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let err = conn
            .execute(
                "INSERT INTO events (continuity_id, seq, timestamp, event_type, payload)
                 VALUES ('c1', 1, 0, 'StepStarted', '{}')",
                [],
            )
            .unwrap_err();
        assert_eq!(
            err,
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE),
                Some("UNIQUE constraint failed: events.continuity_id, events.seq".to_string())
            )
        );
    }

    #[test]
    fn events_are_persisted_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        {
            let store = EventStore::open(&path).unwrap();
            store
                .append_many(
                    "c1",
                    &[
                        EventType::ContinuityStarted {
                            initial_prompt: "hello".into(),
                        },
                        EventType::ToolCall {
                            tool_name: "read_file".into(),
                            args: json!({"path": "a.txt"}),
                            result: ToolOutcome::Ok(json!({"content": "x"})),
                        },
                    ],
                )
                .unwrap();
        }
        let store = EventStore::open(&path).unwrap();
        let events = store.events_after("c1", 0).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.name(), "ContinuityStarted");
        match &events[1].event {
            EventType::ToolCall {
                tool_name, result, ..
            } => {
                assert_eq!(tool_name, "read_file");
                assert_eq!(*result, ToolOutcome::Ok(json!({"content": "x"})));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn events_after_filters_by_sequence() {
        let (_dir, store) = open_temp();
        store
            .append_many(
                "c1",
                &[
                    EventType::StepStarted { step_num: 1 },
                    EventType::StepStarted { step_num: 2 },
                    EventType::StepStarted { step_num: 3 },
                ],
            )
            .unwrap();
        let after = store.events_after("c1", 1).unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].seq, 2);
        assert_eq!(after[1].seq, 3);
    }

    #[test]
    fn snapshot_roundtrip_and_overwrite() {
        let (_dir, store) = open_temp();
        assert!(store.latest_snapshot("c1").unwrap().is_none());

        store
            .save_snapshot("c1", 5, &json!({"state": "one"}))
            .unwrap();
        let snap = store.latest_snapshot("c1").unwrap().unwrap();
        assert_eq!(snap.seq, 5);
        assert_eq!(snap.state, json!({"state": "one"}));

        // Older snapshots must not overwrite newer ones.
        store
            .save_snapshot("c1", 3, &json!({"state": "old"}))
            .unwrap();
        let snap = store.latest_snapshot("c1").unwrap().unwrap();
        assert_eq!(snap.seq, 5);

        store
            .save_snapshot("c1", 9, &json!({"state": "two"}))
            .unwrap();
        let snap = store.latest_snapshot("c1").unwrap().unwrap();
        assert_eq!(snap.seq, 9);
        assert_eq!(snap.state, json!({"state": "two"}));
    }

    #[test]
    fn incomplete_continuities_exclude_finished() {
        let (_dir, store) = open_temp();
        store
            .append_many(
                "done",
                &[
                    EventType::ContinuityStarted {
                        initial_prompt: "a".into(),
                    },
                    EventType::ContinuityFinished {
                        status: "completed".into(),
                        summary: "ok".into(),
                    },
                ],
            )
            .unwrap();
        store
            .append(
                "pending",
                &EventType::ContinuityStarted {
                    initial_prompt: "b".into(),
                },
            )
            .unwrap();
        let pending = store.incomplete_continuities().unwrap();
        assert_eq!(pending, vec!["pending".to_string()]);
    }

    #[test]
    fn corrupted_database_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.db");
        std::fs::write(&path, b"this is not a sqlite database at all").unwrap();
        let err = EventStore::open(&path).unwrap_err();
        assert_eq!(err.code(), ErrorCode::Database);
    }

    #[test]
    fn snapshot_restores_recovery_point() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        {
            let store = EventStore::open(&path).unwrap();
            store
                .append_many("c1", &[EventType::StepStarted { step_num: 1 }])
                .unwrap();
            store
                .save_snapshot("c1", 1, &json!({"step_num": 1}))
                .unwrap();
            store
                .append_many("c1", &[EventType::StepStarted { step_num: 2 }])
                .unwrap();
        }
        let store = EventStore::open(&path).unwrap();
        let snap = store.latest_snapshot("c1").unwrap().unwrap();
        assert_eq!(snap.seq, 1);
        let replay = store.events_after("c1", snap.seq).unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].seq, 2);
        assert_eq!(replay[0].event.name(), "StepStarted");
    }

    #[test]
    fn newest_snapshot_across_continuities() {
        let (_dir, store) = open_temp();
        store.save_snapshot("a", 1, &json!({"x": 1})).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.save_snapshot("b", 2, &json!({"x": 2})).unwrap();
        let snap = store.newest_snapshot().unwrap().unwrap();
        assert_eq!(snap.continuity_id, "b");
    }

    #[test]
    fn gapless_logs_pass_verification() {
        let (_dir, store) = open_temp();
        store
            .append_many("c1", &[EventType::StepStarted { step_num: 1 }])
            .unwrap();
        store
            .append_many("c1", &[EventType::StepStarted { step_num: 2 }])
            .unwrap();
        assert!(store.verify_continuity("c1", 1).is_ok());
        assert!(store.verify_continuity("c1", 2).is_ok());
        assert!(
            store.verify_continuity("c1", 3).is_err(),
            "snapshot past the log must fail"
        );
    }

    #[test]
    fn missing_events_fail_verification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = EventStore::open(&path).unwrap();
        store
            .append_many("c1", &[EventType::StepStarted { step_num: 1 }])
            .unwrap();
        store
            .append_many("c1", &[EventType::StepStarted { step_num: 2 }])
            .unwrap();
        store
            .append_many("c1", &[EventType::StepStarted { step_num: 3 }])
            .unwrap();
        drop(store);

        // Simulate event loss by deleting the middle row directly.
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "DELETE FROM events WHERE continuity_id = 'c1' AND seq = 2",
            [],
        )
        .unwrap();
        drop(conn);

        let store = EventStore::open(&path).unwrap();
        let err = store.verify_continuity("c1", 1).unwrap_err();
        assert!(err.to_string().contains("gap"), "{err}");
        assert_eq!(err.code(), ErrorCode::Internal);
    }

    #[test]
    fn snapshot_behind_log_fails_verification() {
        let (_dir, store) = open_temp();
        store
            .append_many("c1", &[EventType::StepStarted { step_num: 1 }])
            .unwrap();
        store.save_snapshot("c1", 5, &json!({"x": 1})).unwrap();
        let err = store.verify_continuity("c1", 5).unwrap_err();
        assert!(err.to_string().contains("behind"), "{err}");
    }

    #[test]
    fn concurrent_appends_across_connections_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store_a = EventStore::open(&path).unwrap();
        let store_b = EventStore::open(&path).unwrap();
        store_a
            .append("c1", &EventType::StepStarted { step_num: 1 })
            .unwrap();
        store_b
            .append("c1", &EventType::StepStarted { step_num: 2 })
            .unwrap();
        store_a
            .append("c1", &EventType::StepStarted { step_num: 3 })
            .unwrap();
        let events = store_a.events_after("c1", 0).unwrap();
        assert_eq!(events.len(), 3);
        let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn opening_a_directory_as_db_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("not_a_file");
        std::fs::create_dir_all(&sub).unwrap();
        let err = EventStore::open(&sub).unwrap_err();
        assert_eq!(err.code(), ErrorCode::Database);
    }

    #[test]
    fn newer_schema_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.db");
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(conn);
        let err = EventStore::open(&path).unwrap_err();
        assert!(err.to_string().contains("newer"), "{err}");
        assert_eq!(err.code(), ErrorCode::Internal);
    }

    #[test]
    fn older_schema_version_is_upgraded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 0).unwrap();
        drop(conn);
        let store = EventStore::open(&path).unwrap();
        assert_eq!(EventStore::schema_version(&path).unwrap(), SCHEMA_VERSION);
        store
            .append("c1", &EventType::StepStarted { step_num: 1 })
            .unwrap();
        assert_eq!(store.events_after("c1", 0).unwrap().len(), 1);
    }

    use crate::error::ErrorCode;
}
