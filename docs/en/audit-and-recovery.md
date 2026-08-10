# Audit and Recovery

Everything the agent does is appended to `agent.db` as an immutable event log
(`events` table) with periodic state snapshots (`snapshots` table). This guide
shows how to audit a run and how to recover after a crash.

## Audit

The schema is stable and documented in `SPECS.md`. Useful queries with the
`sqlite3` CLI:

```sql
-- The full timeline of the newest continuity, newest first:
SELECT seq, event_type, timestamp, payload
FROM events
WHERE continuity_id = (SELECT continuity_id FROM events
                       ORDER BY id DESC LIMIT 1)
ORDER BY seq;

-- Every tool call in a continuity:
SELECT seq, payload FROM events
WHERE continuity_id = 'CONTINUITY_ID' AND event_type = 'ToolCall';

-- Failures and evolution outcomes:
SELECT seq, event_type, payload FROM events
WHERE continuity_id = 'CONTINUITY_ID'
  AND event_type IN ('Error', 'EvolutionAttempt', 'EvolutionResult');

-- Unfinished continuities (candidates for resume):
SELECT DISTINCT continuity_id FROM events e
WHERE NOT EXISTS (SELECT 1 FROM events f
                  WHERE f.continuity_id = e.continuity_id
                    AND f.event_type = 'ContinuityFinished');
```

Snapshot state is the serialized agent context: state machine position, plan,
conversation messages, and budget usage.

## Recovery

On startup the agent:

1. Finds the newest unfinished continuity.
2. Loads its latest snapshot.
3. Verifies the event log is gapless from the snapshot (`verify_continuity`).
4. Replays events to recount budget usage and evolution attempts.
5. Continues from the persisted state machine position.

Two special cases:

- **Crash before the first snapshot**: the task restarts from the
  `ContinuityStarted` event's initial prompt.
- **Corrupt or newer database**: the agent refuses to open it and exits with
  a clear error (see `migrations.md`). It never silently continues from
  inconsistent state.

## Guarantees

- Every external side effect (file, process, git, LLM) is an audited event.
- Sequence numbers are monotonic per continuity; a gap fails loudly on resume.
- Evolution candidates are validated in an isolated sandbox and rolled back
  on failure; `.bak` files never remain after a completed attempt.
- API keys and secrets never appear in events, logs, or prompt previews.
