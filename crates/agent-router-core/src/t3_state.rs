//! Read-only observation of T3 Code threads, for reconciling rows launched on the t3 surface.
//!
//! T3 speaks orchestration protocol 2, whose read model lives in `~/.t3/userdata/statev2.sqlite`.
//! In that protocol a thread has runs rather than a session: every dispatched message becomes one
//! run, and the thread's status is its latest run's status, or `idle` when it has never run. The
//! router opens the file read-only and derives, one thread at a time, the same fields T3's own
//! thread shell (`orchestration.subscribeShell`) carries:
//!
//! - `orchestration_v2_projection_threads`: whether the thread exists (a deleted one is missing).
//! - `orchestration_v2_projection_runs`: the latest run that is not held in a queue (the shell's
//!   `status`), and whether any run is preparing, starting, or running (the shell's `activeRunId`).
//! - The provider session's `lastError`, else the failed latest run's root error item (the shell's
//!   `lastError`).
//!
//! The queries mirror the server's own SQL thread shell, minus the usage-limit presentation it
//! layers on top. Read-only SQLite rather than the WebSocket RPC snapshot: the RPC needs node, a T3
//! server that is up, and a short-lived auth session minted (and so written) through the `t3` CLI
//! for every `status` call, while this read needs none of them and cannot change T3 state.

use crate::error::Result;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a read waits on a T3 writer holding the lock before it gives up on that row.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

/// The T3 orchestration protocol 2 state database under a home directory, at the same base dir
/// `t3-thread` defaults to.
pub fn state_db_path(home: &Path) -> PathBuf {
    home.join(".t3/userdata/statev2.sqlite")
}

/// What T3 records about one thread's runs, passed through untranslated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct T3Thread {
    /// The latest unheld run's status (`queued`, `running`, `completed`, `failed`, ...), or None
    /// for a thread that has never run, which T3's shell reports as `idle`.
    pub run_status: Option<String>,
    /// Whether a run is preparing, starting, or running: the shell's `activeRunId` is set.
    pub active_run: bool,
    pub last_error: Option<String>,
    /// The newest of the thread's own update and its latest run's request, start, and completion,
    /// as T3 wrote them (ISO8601 UTC).
    pub last_activity_at: Option<String>,
}

/// An open, read-only handle on the T3 state database.
pub struct T3State {
    conn: Connection,
}

/// The latest run not waiting in a held queue, which is the run T3's shell presents as the
/// thread's status. `?1` is the thread id.
const LATEST_RUN: &str = "SELECT r.run_id FROM orchestration_v2_projection_runs r \
     WHERE r.thread_id = ?1 \
     AND NOT (r.status = 'queued' AND json_extract(r.payload_json, '$.queueHeld') IS 1) \
     ORDER BY r.ordinal DESC, r.run_id DESC LIMIT 1";

impl T3State {
    /// IMPURE: open the database read-only. A missing file is an error, never a created file.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "query_only", true)?;
        Ok(Self { conn })
    }

    /// IMPURE: the run state of one thread id, or None when T3 has no such (undeleted) thread.
    pub fn lookup(&self, thread_id: &str) -> Result<Option<T3Thread>> {
        let sql = format!(
            "SELECT latest.status, \
             EXISTS (SELECT 1 FROM orchestration_v2_projection_runs a \
                     WHERE a.thread_id = t.thread_id \
                     AND a.status IN ('preparing', 'starting', 'running')), \
             COALESCE( \
               (SELECT json_extract(s.payload_json, '$.lastError') \
                FROM orchestration_v2_projection_provider_sessions s \
                JOIN orchestration_v2_projection_provider_session_bindings b \
                  ON b.provider_session_id = s.provider_session_id \
                WHERE b.thread_id = t.thread_id \
                AND s.provider_instance_id IS t.provider_instance_id \
                ORDER BY s.updated_at DESC, s.provider_session_id DESC LIMIT 1), \
               (SELECT json_extract(i.payload_json, '$.failure.message') \
                FROM orchestration_v2_projection_turn_items i \
                WHERE latest.status = 'failed' AND i.thread_id = t.thread_id \
                AND i.run_id = latest.run_id AND i.type = 'error' AND i.status = 'failed' \
                AND i.node_id IS json_extract(latest.payload_json, '$.rootNodeId') \
                ORDER BY i.updated_at DESC, i.ordinal DESC LIMIT 1)), \
             t.updated_at, latest.requested_at, \
             json_extract(latest.payload_json, '$.startedAt'), latest.completed_at \
             FROM orchestration_v2_projection_threads t \
             LEFT JOIN orchestration_v2_projection_runs latest \
               ON latest.run_id = ({LATEST_RUN}) \
             WHERE t.thread_id = ?1 AND t.deleted_at IS NULL"
        );
        let row = self
            .conn
            .query_row(&sql, [thread_id], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    [
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ],
                ))
            })
            .optional()?;
        Ok(
            row.map(|(run_status, active_run, last_error, stamps)| T3Thread {
                run_status,
                active_run,
                last_error,
                // Same-format ISO8601 strings order lexicographically.
                last_activity_at: stamps.into_iter().flatten().max(),
            }),
        )
    }
}
