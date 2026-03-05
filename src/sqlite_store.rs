use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

const DB_REL_PATH: &str = ".psht/state.db";
const BUSY_TIMEOUT_MS: u64 = 5_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSpecRow {
    pub app_id: String,
    pub generation: i64,
    pub desired_state: String,
    pub source_kind: String,
    pub source_payload_json: String,
    pub runtime_payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppStatusRow {
    pub app_id: String,
    pub observed_generation: i64,
    pub phase: String,
    pub active_instance: Option<String>,
    pub candidate_instance: Option<String>,
    pub previous_instance: Option<String>,
    pub active_revision: Option<String>,
    pub candidate_revision: Option<String>,
    pub health_json: String,
    pub last_error_json: Option<String>,
    pub recovery_actions_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployIntentRow {
    pub intent_id: String,
    pub app_id: String,
    pub generation: i64,
    pub kind: String,
    pub payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppRuntimeStateRow {
    pub app_id: String,
    pub active_instance: String,
    pub previous_instance: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDeployStateRow {
    pub app_id: String,
    pub ref_name: String,
    pub sha: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingGitRequestRow {
    pub app_id: String,
    pub ref_name: String,
    pub sha: String,
    pub force: bool,
    pub request_id: Option<String>,
    pub interrupt_requested_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployInterruptRow {
    pub app_id: String,
    pub request_id: String,
    pub requested_at: i64,
    pub target_sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupJobRow {
    pub app_id: String,
    pub active_instance_at_schedule: String,
    pub scheduled_previous_instance: String,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub scheduled_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppLeaseRow {
    pub app_id: String,
    pub lease_owner: String,
    pub lease_epoch: i64,
    pub heartbeat_at_ms: i64,
    pub expires_at_ms: i64,
    pub intent_id: String,
    pub generation: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileAttemptRow {
    pub app_id: String,
    pub intent_id: String,
    pub generation: i64,
    pub step_name: String,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub result: String,
    pub detail_json: String,
}

fn home_dir() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/home/psht".to_string()))
}

fn db_path() -> PathBuf {
    home_dir().join(DB_REL_PATH)
}

fn fallback_db_path() -> PathBuf {
    PathBuf::from("/tmp").join(format!("psht-state-{}.db", std::process::id()))
}

fn now_unix_ms() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    now.as_millis() as i64
}

fn init_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS app_specs (
    app_id TEXT PRIMARY KEY,
    generation INTEGER NOT NULL,
    desired_state TEXT NOT NULL,
    source_kind TEXT NOT NULL,
    source_payload_json TEXT NOT NULL,
    runtime_payload_json TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS app_status (
    app_id TEXT PRIMARY KEY,
    observed_generation INTEGER NOT NULL,
    phase TEXT NOT NULL,
    active_instance TEXT,
    candidate_instance TEXT,
    previous_instance TEXT,
    active_revision TEXT,
    candidate_revision TEXT,
    health_json TEXT NOT NULL,
    last_error_json TEXT,
    recovery_actions_json TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS deploy_intents (
    intent_id TEXT PRIMARY KEY,
    app_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    kind TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    processed_at_ms INTEGER
);

CREATE TABLE IF NOT EXISTS reconcile_checkpoints (
    app_id TEXT PRIMARY KEY,
    generation INTEGER NOT NULL,
    plan_hash TEXT NOT NULL,
    op_index INTEGER NOT NULL,
    op_name TEXT NOT NULL,
    last_result_json TEXT,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS deploy_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    revision TEXT,
    outcome TEXT NOT NULL,
    summary TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS app_runtime_state (
    app_id TEXT PRIMARY KEY,
    active_instance TEXT NOT NULL,
    previous_instance TEXT,
    updated_at INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS git_deploy_state (
    app_id TEXT PRIMARY KEY,
    ref_name TEXT NOT NULL,
    sha TEXT NOT NULL,
    status TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS pending_git_requests (
    app_id TEXT PRIMARY KEY,
    ref_name TEXT NOT NULL,
    sha TEXT NOT NULL,
    force INTEGER NOT NULL,
    request_id TEXT,
    interrupt_requested_at INTEGER,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS deploy_interrupts (
    app_id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL,
    requested_at INTEGER NOT NULL,
    target_sha TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS cleanup_jobs (
    app_id TEXT PRIMARY KEY,
    active_instance_at_schedule TEXT NOT NULL,
    scheduled_previous_instance TEXT NOT NULL,
    attempts INTEGER NOT NULL,
    last_error TEXT,
    scheduled_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS app_leases (
    app_id TEXT PRIMARY KEY,
    lease_owner TEXT NOT NULL,
    lease_epoch INTEGER NOT NULL,
    heartbeat_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    intent_id TEXT NOT NULL,
    generation INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS reconcile_attempts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app_id TEXT NOT NULL,
    intent_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    step_name TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL,
    finished_at_ms INTEGER,
    result TEXT NOT NULL,
    detail_json TEXT NOT NULL
);
"#,
    )
    .map_err(|e| format!("failed to initialize sqlite schema: {e}"))?;

    conn.execute(
        "INSERT OR IGNORE INTO schema_migrations(version, applied_at_ms) VALUES(1, ?1)",
        params![now_unix_ms()],
    )
    .map_err(|e| format!("failed to record sqlite schema migration: {e}"))?;

    Ok(())
}

fn open_connection_at(path: &PathBuf) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }

    let conn = Connection::open(path)
        .map_err(|e| format!("failed to open sqlite state {}: {e}", path.display()))?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
        .map_err(|e| format!("failed to set sqlite busy timeout: {e}"))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| format!("failed to set sqlite journal mode: {e}"))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| format!("failed to set sqlite synchronous mode: {e}"))?;

    init_schema(&conn)?;
    Ok(conn)
}

fn open_connection() -> Result<Connection, String> {
    let primary = db_path();
    match open_connection_at(&primary) {
        Ok(conn) => Ok(conn),
        Err(primary_err) => {
            let fallback = fallback_db_path();
            open_connection_at(&fallback).map_err(|fallback_err| {
                format!(
                    "{primary_err}; fallback {} also failed: {fallback_err}",
                    fallback.display()
                )
            })
        }
    }
}

pub fn next_app_generation(app_id: &str) -> Result<i64, String> {
    let current = get_app_spec(app_id)?
        .map(|spec| spec.generation)
        .unwrap_or(0);
    Ok(current.saturating_add(1))
}

pub fn upsert_app_spec(row: &AppSpecRow) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "INSERT INTO app_specs(
            app_id, generation, desired_state, source_kind, source_payload_json, runtime_payload_json, updated_at_ms
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(app_id) DO UPDATE SET
            generation = excluded.generation,
            desired_state = excluded.desired_state,
            source_kind = excluded.source_kind,
            source_payload_json = excluded.source_payload_json,
            runtime_payload_json = excluded.runtime_payload_json,
            updated_at_ms = excluded.updated_at_ms",
        params![
            row.app_id,
            row.generation,
            row.desired_state,
            row.source_kind,
            row.source_payload_json,
            row.runtime_payload_json,
            now_unix_ms(),
        ],
    )
    .map_err(|e| format!("failed to upsert app spec for {}: {e}", row.app_id))?;
    Ok(())
}

pub fn get_app_spec(app_id: &str) -> Result<Option<AppSpecRow>, String> {
    let conn = open_connection()?;
    conn.query_row(
        "SELECT app_id, generation, desired_state, source_kind, source_payload_json, runtime_payload_json
         FROM app_specs WHERE app_id = ?1",
        params![app_id],
        |row| {
            Ok(AppSpecRow {
                app_id: row.get(0)?,
                generation: row.get(1)?,
                desired_state: row.get(2)?,
                source_kind: row.get(3)?,
                source_payload_json: row.get(4)?,
                runtime_payload_json: row.get(5)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("failed to read app spec for {app_id}: {e}"))
}

pub fn upsert_app_status(row: &AppStatusRow) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "INSERT INTO app_status(
            app_id, observed_generation, phase, active_instance, candidate_instance, previous_instance,
            active_revision, candidate_revision, health_json, last_error_json, recovery_actions_json, updated_at_ms
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(app_id) DO UPDATE SET
            observed_generation = excluded.observed_generation,
            phase = excluded.phase,
            active_instance = excluded.active_instance,
            candidate_instance = excluded.candidate_instance,
            previous_instance = excluded.previous_instance,
            active_revision = excluded.active_revision,
            candidate_revision = excluded.candidate_revision,
            health_json = excluded.health_json,
            last_error_json = excluded.last_error_json,
            recovery_actions_json = excluded.recovery_actions_json,
            updated_at_ms = excluded.updated_at_ms",
        params![
            row.app_id,
            row.observed_generation,
            row.phase,
            row.active_instance,
            row.candidate_instance,
            row.previous_instance,
            row.active_revision,
            row.candidate_revision,
            row.health_json,
            row.last_error_json,
            row.recovery_actions_json,
            now_unix_ms(),
        ],
    )
    .map_err(|e| format!("failed to upsert app status for {}: {e}", row.app_id))?;
    Ok(())
}

pub fn get_app_status(app_id: &str) -> Result<Option<AppStatusRow>, String> {
    let conn = open_connection()?;
    conn.query_row(
        "SELECT app_id, observed_generation, phase, active_instance, candidate_instance, previous_instance,
                active_revision, candidate_revision, health_json, last_error_json, recovery_actions_json
         FROM app_status WHERE app_id = ?1",
        params![app_id],
        |row| {
            Ok(AppStatusRow {
                app_id: row.get(0)?,
                observed_generation: row.get(1)?,
                phase: row.get(2)?,
                active_instance: row.get(3)?,
                candidate_instance: row.get(4)?,
                previous_instance: row.get(5)?,
                active_revision: row.get(6)?,
                candidate_revision: row.get(7)?,
                health_json: row.get(8)?,
                last_error_json: row.get(9)?,
                recovery_actions_json: row.get(10)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("failed to read app status for {app_id}: {e}"))
}

pub fn insert_deploy_intent(row: &DeployIntentRow) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "INSERT INTO deploy_intents(intent_id, app_id, generation, kind, payload_json, created_at_ms)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            row.intent_id,
            row.app_id,
            row.generation,
            row.kind,
            row.payload_json,
            now_unix_ms(),
        ],
    )
    .map_err(|e| format!("failed to insert deploy intent {}: {e}", row.intent_id))?;
    Ok(())
}

pub fn mark_deploy_intent_processed(intent_id: &str) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "UPDATE deploy_intents SET processed_at_ms = ?2 WHERE intent_id = ?1",
        params![intent_id, now_unix_ms()],
    )
    .map_err(|e| format!("failed to mark deploy intent processed {intent_id}: {e}"))?;
    Ok(())
}

pub fn upsert_reconcile_checkpoint(
    app_id: &str,
    generation: i64,
    plan_hash: &str,
    op_index: i64,
    op_name: &str,
    last_result_json: Option<&str>,
) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "INSERT INTO reconcile_checkpoints(app_id, generation, plan_hash, op_index, op_name, last_result_json, updated_at_ms)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(app_id) DO UPDATE SET
            generation = excluded.generation,
            plan_hash = excluded.plan_hash,
            op_index = excluded.op_index,
            op_name = excluded.op_name,
            last_result_json = excluded.last_result_json,
            updated_at_ms = excluded.updated_at_ms",
        params![app_id, generation, plan_hash, op_index, op_name, last_result_json, now_unix_ms()],
    )
    .map_err(|e| format!("failed to upsert reconcile checkpoint for {app_id}: {e}"))?;
    Ok(())
}

pub fn clear_reconcile_checkpoint(app_id: &str) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "DELETE FROM reconcile_checkpoints WHERE app_id = ?1",
        params![app_id],
    )
    .map_err(|e| format!("failed to clear reconcile checkpoint for {app_id}: {e}"))?;
    Ok(())
}

pub fn append_deploy_history(
    app_id: &str,
    generation: i64,
    revision: Option<&str>,
    outcome: &str,
    summary: &str,
) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "INSERT INTO deploy_history(app_id, generation, revision, outcome, summary, created_at_ms)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            app_id,
            generation,
            revision,
            outcome,
            summary,
            now_unix_ms()
        ],
    )
    .map_err(|e| format!("failed to append deploy history for {app_id}: {e}"))?;
    Ok(())
}

pub fn upsert_app_runtime_state(row: &AppRuntimeStateRow) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "INSERT INTO app_runtime_state(app_id, active_instance, previous_instance, updated_at, updated_at_ms)
         VALUES(?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(app_id) DO UPDATE SET
            active_instance = excluded.active_instance,
            previous_instance = excluded.previous_instance,
            updated_at = excluded.updated_at,
            updated_at_ms = excluded.updated_at_ms",
        params![
            row.app_id,
            row.active_instance,
            row.previous_instance,
            row.updated_at,
            now_unix_ms(),
        ],
    )
    .map_err(|e| format!("failed to upsert runtime state for {}: {e}", row.app_id))?;
    Ok(())
}

pub fn get_app_runtime_state(app_id: &str) -> Result<Option<AppRuntimeStateRow>, String> {
    let conn = open_connection()?;
    conn.query_row(
        "SELECT app_id, active_instance, previous_instance, updated_at
         FROM app_runtime_state WHERE app_id = ?1",
        params![app_id],
        |row| {
            Ok(AppRuntimeStateRow {
                app_id: row.get(0)?,
                active_instance: row.get(1)?,
                previous_instance: row.get(2)?,
                updated_at: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("failed to read runtime state for {app_id}: {e}"))
}

pub fn list_app_runtime_states() -> Result<Vec<AppRuntimeStateRow>, String> {
    let conn = open_connection()?;
    let mut stmt = conn
        .prepare(
            "SELECT app_id, active_instance, previous_instance, updated_at
             FROM app_runtime_state
             ORDER BY app_id",
        )
        .map_err(|e| format!("failed to prepare runtime state list query: {e}"))?;

    let rows = stmt
        .query_map([], |row| {
            Ok(AppRuntimeStateRow {
                app_id: row.get(0)?,
                active_instance: row.get(1)?,
                previous_instance: row.get(2)?,
                updated_at: row.get(3)?,
            })
        })
        .map_err(|e| format!("failed to query runtime state list: {e}"))?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("failed to read runtime state row: {e}"))?);
    }
    Ok(out)
}

pub fn delete_app_runtime_state(app_id: &str) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "DELETE FROM app_runtime_state WHERE app_id = ?1",
        params![app_id],
    )
    .map_err(|e| format!("failed to delete runtime state for {app_id}: {e}"))?;
    Ok(())
}

pub fn upsert_git_deploy_state(row: &GitDeployStateRow) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "INSERT INTO git_deploy_state(app_id, ref_name, sha, status, updated_at_ms)
         VALUES(?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(app_id) DO UPDATE SET
            ref_name = excluded.ref_name,
            sha = excluded.sha,
            status = excluded.status,
            updated_at_ms = excluded.updated_at_ms",
        params![row.app_id, row.ref_name, row.sha, row.status, now_unix_ms()],
    )
    .map_err(|e| format!("failed to upsert git deploy state for {}: {e}", row.app_id))?;
    Ok(())
}

pub fn get_git_deploy_state(app_id: &str) -> Result<Option<GitDeployStateRow>, String> {
    let conn = open_connection()?;
    conn.query_row(
        "SELECT app_id, ref_name, sha, status
         FROM git_deploy_state WHERE app_id = ?1",
        params![app_id],
        |row| {
            Ok(GitDeployStateRow {
                app_id: row.get(0)?,
                ref_name: row.get(1)?,
                sha: row.get(2)?,
                status: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("failed to read git deploy state for {app_id}: {e}"))
}

pub fn delete_git_deploy_state(app_id: &str) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "DELETE FROM git_deploy_state WHERE app_id = ?1",
        params![app_id],
    )
    .map_err(|e| format!("failed to delete git deploy state for {app_id}: {e}"))?;
    Ok(())
}

pub fn upsert_pending_git_request(row: &PendingGitRequestRow) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "INSERT INTO pending_git_requests(
            app_id, ref_name, sha, force, request_id, interrupt_requested_at, updated_at_ms
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(app_id) DO UPDATE SET
            ref_name = excluded.ref_name,
            sha = excluded.sha,
            force = excluded.force,
            request_id = excluded.request_id,
            interrupt_requested_at = excluded.interrupt_requested_at,
            updated_at_ms = excluded.updated_at_ms",
        params![
            row.app_id,
            row.ref_name,
            row.sha,
            if row.force { 1i64 } else { 0i64 },
            row.request_id,
            row.interrupt_requested_at,
            now_unix_ms(),
        ],
    )
    .map_err(|e| {
        format!(
            "failed to upsert pending git request for {}: {e}",
            row.app_id
        )
    })?;
    Ok(())
}

pub fn get_pending_git_request(app_id: &str) -> Result<Option<PendingGitRequestRow>, String> {
    let conn = open_connection()?;
    conn.query_row(
        "SELECT app_id, ref_name, sha, force, request_id, interrupt_requested_at
         FROM pending_git_requests WHERE app_id = ?1",
        params![app_id],
        |row| {
            let force_raw: i64 = row.get(3)?;
            Ok(PendingGitRequestRow {
                app_id: row.get(0)?,
                ref_name: row.get(1)?,
                sha: row.get(2)?,
                force: force_raw != 0,
                request_id: row.get(4)?,
                interrupt_requested_at: row.get(5)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("failed to read pending git request for {app_id}: {e}"))
}

pub fn take_pending_git_request(app_id: &str) -> Result<Option<PendingGitRequestRow>, String> {
    let mut conn = open_connection()?;
    let tx = conn
        .transaction()
        .map_err(|e| format!("failed to begin sqlite transaction for {app_id}: {e}"))?;

    let request = tx
        .query_row(
            "SELECT app_id, ref_name, sha, force, request_id, interrupt_requested_at
             FROM pending_git_requests WHERE app_id = ?1",
            params![app_id],
            |row| {
                let force_raw: i64 = row.get(3)?;
                Ok(PendingGitRequestRow {
                    app_id: row.get(0)?,
                    ref_name: row.get(1)?,
                    sha: row.get(2)?,
                    force: force_raw != 0,
                    request_id: row.get(4)?,
                    interrupt_requested_at: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(|e| format!("failed to read pending git request for {app_id}: {e}"))?;

    if request.is_some() {
        tx.execute(
            "DELETE FROM pending_git_requests WHERE app_id = ?1",
            params![app_id],
        )
        .map_err(|e| format!("failed to delete pending git request for {app_id}: {e}"))?;
    }

    tx.commit()
        .map_err(|e| format!("failed to commit sqlite transaction for {app_id}: {e}"))?;

    Ok(request)
}

pub fn upsert_deploy_interrupt(row: &DeployInterruptRow) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "INSERT INTO deploy_interrupts(app_id, request_id, requested_at, target_sha, updated_at_ms)
         VALUES(?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(app_id) DO UPDATE SET
            request_id = excluded.request_id,
            requested_at = excluded.requested_at,
            target_sha = excluded.target_sha,
            updated_at_ms = excluded.updated_at_ms",
        params![
            row.app_id,
            row.request_id,
            row.requested_at,
            row.target_sha,
            now_unix_ms(),
        ],
    )
    .map_err(|e| format!("failed to upsert deploy interrupt for {}: {e}", row.app_id))?;
    Ok(())
}

pub fn get_deploy_interrupt(app_id: &str) -> Result<Option<DeployInterruptRow>, String> {
    let conn = open_connection()?;
    conn.query_row(
        "SELECT app_id, request_id, requested_at, target_sha
         FROM deploy_interrupts WHERE app_id = ?1",
        params![app_id],
        |row| {
            Ok(DeployInterruptRow {
                app_id: row.get(0)?,
                request_id: row.get(1)?,
                requested_at: row.get(2)?,
                target_sha: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("failed to read deploy interrupt for {app_id}: {e}"))
}

pub fn delete_deploy_interrupt(app_id: &str) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "DELETE FROM deploy_interrupts WHERE app_id = ?1",
        params![app_id],
    )
    .map_err(|e| format!("failed to delete deploy interrupt for {app_id}: {e}"))?;
    Ok(())
}

pub fn upsert_cleanup_job(row: &CleanupJobRow) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "INSERT INTO cleanup_jobs(
            app_id, active_instance_at_schedule, scheduled_previous_instance,
            attempts, last_error, scheduled_at, updated_at, updated_at_ms
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(app_id) DO UPDATE SET
            active_instance_at_schedule = excluded.active_instance_at_schedule,
            scheduled_previous_instance = excluded.scheduled_previous_instance,
            attempts = excluded.attempts,
            last_error = excluded.last_error,
            scheduled_at = excluded.scheduled_at,
            updated_at = excluded.updated_at,
            updated_at_ms = excluded.updated_at_ms",
        params![
            row.app_id,
            row.active_instance_at_schedule,
            row.scheduled_previous_instance,
            row.attempts,
            row.last_error,
            row.scheduled_at,
            row.updated_at,
            now_unix_ms(),
        ],
    )
    .map_err(|e| format!("failed to upsert cleanup job for {}: {e}", row.app_id))?;
    Ok(())
}

pub fn get_cleanup_job(app_id: &str) -> Result<Option<CleanupJobRow>, String> {
    let conn = open_connection()?;
    conn.query_row(
        "SELECT app_id, active_instance_at_schedule, scheduled_previous_instance,
                attempts, last_error, scheduled_at, updated_at
         FROM cleanup_jobs WHERE app_id = ?1",
        params![app_id],
        |row| {
            Ok(CleanupJobRow {
                app_id: row.get(0)?,
                active_instance_at_schedule: row.get(1)?,
                scheduled_previous_instance: row.get(2)?,
                attempts: row.get(3)?,
                last_error: row.get(4)?,
                scheduled_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("failed to read cleanup job for {app_id}: {e}"))
}

pub fn delete_cleanup_job(app_id: &str) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "DELETE FROM cleanup_jobs WHERE app_id = ?1",
        params![app_id],
    )
    .map_err(|e| format!("failed to delete cleanup job for {app_id}: {e}"))?;
    Ok(())
}

#[allow(dead_code)]
pub fn get_app_lease(app_id: &str) -> Result<Option<AppLeaseRow>, String> {
    let conn = open_connection()?;
    conn.query_row(
        "SELECT app_id, lease_owner, lease_epoch, heartbeat_at_ms, expires_at_ms, intent_id, generation
         FROM app_leases WHERE app_id = ?1",
        params![app_id],
        |row| {
            Ok(AppLeaseRow {
                app_id: row.get(0)?,
                lease_owner: row.get(1)?,
                lease_epoch: row.get(2)?,
                heartbeat_at_ms: row.get(3)?,
                expires_at_ms: row.get(4)?,
                intent_id: row.get(5)?,
                generation: row.get(6)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("failed to read app lease for {app_id}: {e}"))
}

pub fn acquire_app_lease(
    app_id: &str,
    lease_owner: &str,
    intent_id: &str,
    generation: i64,
    ttl_ms: i64,
) -> Result<AppLeaseRow, String> {
    let mut conn = open_connection()?;
    let tx = conn
        .transaction()
        .map_err(|e| format!("failed to begin sqlite transaction for lease {app_id}: {e}"))?;

    let existing = tx
        .query_row(
            "SELECT app_id, lease_owner, lease_epoch, heartbeat_at_ms, expires_at_ms, intent_id, generation
             FROM app_leases WHERE app_id = ?1",
            params![app_id],
            |row| {
                Ok(AppLeaseRow {
                    app_id: row.get(0)?,
                    lease_owner: row.get(1)?,
                    lease_epoch: row.get(2)?,
                    heartbeat_at_ms: row.get(3)?,
                    expires_at_ms: row.get(4)?,
                    intent_id: row.get(5)?,
                    generation: row.get(6)?,
                })
            },
        )
        .optional()
        .map_err(|e| format!("failed to read existing app lease for {app_id}: {e}"))?;

    let now_ms = now_unix_ms();
    if let Some(existing) = existing.as_ref()
        && existing.expires_at_ms > now_ms
        && existing.lease_owner != lease_owner
    {
        return Err(format!(
            "reconcile lease for '{app_id}' is held by '{}' until {}",
            existing.lease_owner, existing.expires_at_ms
        ));
    }

    let lease_epoch = existing
        .as_ref()
        .map(|lease| lease.lease_epoch.saturating_add(1))
        .unwrap_or(1);
    let effective_ttl_ms = ttl_ms.max(1_000);
    let expires_at_ms = now_ms.saturating_add(effective_ttl_ms);
    tx.execute(
        "INSERT INTO app_leases(
            app_id, lease_owner, lease_epoch, heartbeat_at_ms, expires_at_ms, intent_id, generation
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(app_id) DO UPDATE SET
            lease_owner = excluded.lease_owner,
            lease_epoch = excluded.lease_epoch,
            heartbeat_at_ms = excluded.heartbeat_at_ms,
            expires_at_ms = excluded.expires_at_ms,
            intent_id = excluded.intent_id,
            generation = excluded.generation",
        params![
            app_id,
            lease_owner,
            lease_epoch,
            now_ms,
            expires_at_ms,
            intent_id,
            generation
        ],
    )
    .map_err(|e| format!("failed to acquire app lease for {app_id}: {e}"))?;

    tx.commit()
        .map_err(|e| format!("failed to commit app lease for {app_id}: {e}"))?;

    Ok(AppLeaseRow {
        app_id: app_id.to_string(),
        lease_owner: lease_owner.to_string(),
        lease_epoch,
        heartbeat_at_ms: now_ms,
        expires_at_ms,
        intent_id: intent_id.to_string(),
        generation,
    })
}

pub fn heartbeat_app_lease(
    app_id: &str,
    lease_owner: &str,
    lease_epoch: i64,
    ttl_ms: i64,
) -> Result<(), String> {
    let conn = open_connection()?;
    let now_ms = now_unix_ms();
    let effective_ttl_ms = ttl_ms.max(1_000);
    let updated = conn
        .execute(
            "UPDATE app_leases
             SET heartbeat_at_ms = ?4, expires_at_ms = ?5
             WHERE app_id = ?1 AND lease_owner = ?2 AND lease_epoch = ?3",
            params![
                app_id,
                lease_owner,
                lease_epoch,
                now_ms,
                now_ms.saturating_add(effective_ttl_ms)
            ],
        )
        .map_err(|e| format!("failed to heartbeat app lease for {app_id}: {e}"))?;
    if updated == 0 {
        return Err(format!(
            "reconcile lease lost for '{app_id}' (owner={lease_owner}, epoch={lease_epoch})"
        ));
    }
    Ok(())
}

pub fn release_app_lease(app_id: &str, lease_owner: &str, lease_epoch: i64) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "DELETE FROM app_leases WHERE app_id = ?1 AND lease_owner = ?2 AND lease_epoch = ?3",
        params![app_id, lease_owner, lease_epoch],
    )
    .map_err(|e| format!("failed to release app lease for {app_id}: {e}"))?;
    Ok(())
}

pub fn append_reconcile_attempt(row: &ReconcileAttemptRow) -> Result<(), String> {
    let conn = open_connection()?;
    conn.execute(
        "INSERT INTO reconcile_attempts(
            app_id, intent_id, generation, step_name, started_at_ms, finished_at_ms, result, detail_json
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            row.app_id,
            row.intent_id,
            row.generation,
            row.step_name,
            row.started_at_ms,
            row.finished_at_ms,
            row.result,
            row.detail_json
        ],
    )
    .map_err(|e| format!("failed to append reconcile attempt for {}: {e}", row.app_id))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_app(prefix: &str) -> String {
        format!("{prefix}-{}-{}", std::process::id(), now_unix_ms())
    }

    #[test]
    fn app_spec_and_status_round_trip() {
        let app = unique_app("sqlite-roundtrip-app");

        let generation = next_app_generation(&app).unwrap();
        upsert_app_spec(&AppSpecRow {
            app_id: app.clone(),
            generation,
            desired_state: "running".to_string(),
            source_kind: "git".to_string(),
            source_payload_json: "{\"sha\":\"abc\"}".to_string(),
            runtime_payload_json: "{}".to_string(),
        })
        .unwrap();
        let spec = get_app_spec(&app).unwrap().unwrap();
        assert_eq!(spec.generation, generation);
        assert_eq!(spec.source_kind, "git");

        upsert_app_status(&AppStatusRow {
            app_id: app.clone(),
            observed_generation: generation,
            phase: "Idle".to_string(),
            active_instance: Some("psht-demo".to_string()),
            candidate_instance: None,
            previous_instance: None,
            active_revision: Some("abc".to_string()),
            candidate_revision: None,
            health_json: "{\"healthy\":true}".to_string(),
            last_error_json: None,
            recovery_actions_json: "[]".to_string(),
        })
        .unwrap();
        let status = get_app_status(&app).unwrap().unwrap();
        assert_eq!(status.phase, "Idle");
        assert_eq!(status.active_revision.as_deref(), Some("abc"));
    }

    #[test]
    fn runtime_state_round_trip() {
        let app = unique_app("sqlite-runtime");
        delete_app_runtime_state(&app).unwrap();

        upsert_app_runtime_state(&AppRuntimeStateRow {
            app_id: app.clone(),
            active_instance: "psht-demo-build-1".to_string(),
            previous_instance: Some("psht-demo".to_string()),
            updated_at: 123,
        })
        .unwrap();

        let got = get_app_runtime_state(&app).unwrap().unwrap();
        assert_eq!(got.active_instance, "psht-demo-build-1");
        assert_eq!(got.previous_instance.as_deref(), Some("psht-demo"));

        let listed = list_app_runtime_states().unwrap();
        assert!(listed.iter().any(|entry| entry.app_id == app));

        delete_app_runtime_state(&app).unwrap();
        assert!(get_app_runtime_state(&app).unwrap().is_none());
    }

    #[test]
    fn deploy_related_state_round_trip() {
        let app = unique_app("sqlite-deploy");

        upsert_git_deploy_state(&GitDeployStateRow {
            app_id: app.clone(),
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
            status: "success".to_string(),
        })
        .unwrap();
        let deploy = get_git_deploy_state(&app).unwrap().unwrap();
        assert_eq!(deploy.status, "success");
        delete_git_deploy_state(&app).unwrap();

        upsert_pending_git_request(&PendingGitRequestRow {
            app_id: app.clone(),
            ref_name: "refs/heads/main".to_string(),
            sha: "cafebabe".to_string(),
            force: true,
            request_id: Some("req-1".to_string()),
            interrupt_requested_at: Some(100),
        })
        .unwrap();
        let pending = take_pending_git_request(&app).unwrap().unwrap();
        assert_eq!(pending.sha, "cafebabe");
        assert!(pending.force);
        assert!(take_pending_git_request(&app).unwrap().is_none());

        upsert_deploy_interrupt(&DeployInterruptRow {
            app_id: app.clone(),
            request_id: "req-2".to_string(),
            requested_at: 101,
            target_sha: "beadfeed".to_string(),
        })
        .unwrap();
        let interrupt = get_deploy_interrupt(&app).unwrap().unwrap();
        assert_eq!(interrupt.request_id, "req-2");
        delete_deploy_interrupt(&app).unwrap();

        upsert_cleanup_job(&CleanupJobRow {
            app_id: app.clone(),
            active_instance_at_schedule: "psht-demo-build-2".to_string(),
            scheduled_previous_instance: "psht-demo-build-1".to_string(),
            attempts: 1,
            last_error: Some("busy".to_string()),
            scheduled_at: 200,
            updated_at: 201,
        })
        .unwrap();
        let cleanup = get_cleanup_job(&app).unwrap().unwrap();
        assert_eq!(cleanup.attempts, 1);
        delete_cleanup_job(&app).unwrap();
        assert!(get_cleanup_job(&app).unwrap().is_none());
    }

    #[test]
    fn app_lease_acquire_heartbeat_release_round_trip() {
        let app = unique_app("sqlite-lease");
        let lease = acquire_app_lease(&app, "owner-a", "intent-a", 1, 5_000).unwrap();
        assert_eq!(lease.lease_owner, "owner-a");
        assert_eq!(lease.intent_id, "intent-a");

        let loaded = get_app_lease(&app).unwrap().unwrap();
        assert_eq!(loaded.lease_owner, "owner-a");
        assert_eq!(loaded.lease_epoch, lease.lease_epoch);

        heartbeat_app_lease(&app, "owner-a", lease.lease_epoch, 5_000).unwrap();
        release_app_lease(&app, "owner-a", lease.lease_epoch).unwrap();
        assert!(get_app_lease(&app).unwrap().is_none());
    }

    #[test]
    fn app_lease_blocks_other_owner_until_expired() {
        let app = unique_app("sqlite-lease-block");
        let _ = acquire_app_lease(&app, "owner-a", "intent-a", 1, 5_000).unwrap();
        let err = acquire_app_lease(&app, "owner-b", "intent-b", 2, 5_000).unwrap_err();
        assert!(err.contains("held by 'owner-a'"));
    }

    #[test]
    fn reconcile_attempt_append_succeeds() {
        let app = unique_app("sqlite-attempt");
        append_reconcile_attempt(&ReconcileAttemptRow {
            app_id: app,
            intent_id: "intent-1".to_string(),
            generation: 1,
            step_name: "wait-for-operation".to_string(),
            started_at_ms: 1_000,
            finished_at_ms: Some(1_100),
            result: "blocked".to_string(),
            detail_json: "{\"ops\":1}".to_string(),
        })
        .unwrap();
    }
}
