use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::sqlite_store;

const DEFAULT_SOURCE_KIND: &str = "manual";
const EMPTY_JSON_OBJECT: &str = "{}";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesiredState {
    Running,
    Stopped,
}

impl DesiredState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
        }
    }

    pub fn from_persisted(raw: &str) -> Self {
        if raw.eq_ignore_ascii_case(Self::Stopped.as_str()) {
            Self::Stopped
        } else {
            Self::Running
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppPhase {
    Idle,
    Reconciling,
    Blocked,
    Degraded,
}

impl AppPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Reconciling => "Reconciling",
            Self::Blocked => "Blocked",
            Self::Degraded => "Degraded",
        }
    }

    fn from_persisted(raw: &str) -> Option<Self> {
        match raw {
            "Idle" => Some(Self::Idle),
            "Reconciling" => Some(Self::Reconciling),
            "Blocked" => Some(Self::Blocked),
            "Degraded" => Some(Self::Degraded),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AppRuntimeState {
    pub active_instance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_instance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_project: Option<String>,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthState {
    pub healthy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl HealthState {
    pub fn healthy() -> Self {
        Self {
            healthy: true,
            reason: None,
        }
    }

    pub fn unhealthy() -> Self {
        Self {
            healthy: false,
            reason: None,
        }
    }

    pub fn reconciling() -> Self {
        Self {
            healthy: false,
            reason: Some("reconciling".to_string()),
        }
    }

    pub fn blocked() -> Self {
        Self {
            healthy: false,
            reason: Some("blocked".to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorReport {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<i64>,
}

impl ErrorReport {
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            generation: None,
        }
    }

    pub fn with_generation(error: impl Into<String>, generation: i64) -> Self {
        Self {
            error: error.into(),
            generation: Some(generation),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    pub active_instance: Option<String>,
    pub candidate_instance: Option<String>,
    pub previous_instance: Option<String>,
    pub active_revision: Option<String>,
    pub candidate_revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppStatus {
    pub app_id: String,
    pub observed_generation: i64,
    pub phase: AppPhase,
    pub active_instance: Option<String>,
    pub candidate_instance: Option<String>,
    pub previous_instance: Option<String>,
    pub active_revision: Option<String>,
    pub candidate_revision: Option<String>,
    pub health: HealthState,
    pub last_error: Option<ErrorReport>,
    pub recovery_actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileIntentContext {
    pub intent_id: String,
    pub generation: i64,
    pub plan_hash: String,
    pub source_summary: String,
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0)
}

fn sqlite_i64_to_u64(value: i64, app: &str, field: &str) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| {
        format!("invalid sqlite value for {field} in app '{app}': expected non-negative integer")
    })
}

fn sqlite_u64_to_i64(value: u64, app: &str, field: &str) -> Result<i64, String> {
    i64::try_from(value)
        .map_err(|_| format!("value for {field} in app '{app}' exceeds sqlite integer range"))
}

fn serialize_json<T: Serialize>(value: &T, label: &str) -> Result<String, String> {
    serde_json::to_string(value).map_err(|e| format!("failed to serialize {label}: {e}"))
}

fn app_runtime_state_from_row(
    row: sqlite_store::AppRuntimeStateRow,
) -> Result<AppRuntimeState, String> {
    Ok(AppRuntimeState {
        active_instance: row.active_instance,
        previous_instance: row.previous_instance,
        runtime_project: row.runtime_project,
        updated_at: sqlite_i64_to_u64(row.updated_at, &row.app_id, "updated_at")?,
    })
}

fn upsert_status(status: &AppStatus) -> Result<(), String> {
    sqlite_store::upsert_app_status(&sqlite_store::AppStatusRow {
        app_id: status.app_id.clone(),
        observed_generation: status.observed_generation,
        phase: status.phase.as_str().to_string(),
        active_instance: status.active_instance.clone(),
        candidate_instance: status.candidate_instance.clone(),
        previous_instance: status.previous_instance.clone(),
        active_revision: status.active_revision.clone(),
        candidate_revision: status.candidate_revision.clone(),
        health_json: serialize_json(&status.health, "health state")?,
        last_error_json: status
            .last_error
            .as_ref()
            .map(|error| serialize_json(error, "error report"))
            .transpose()?,
        recovery_actions_json: serialize_json(&status.recovery_actions, "recovery actions")?,
        updated_at_ms: 0,
    })
}

fn degraded_recovery_actions() -> Vec<String> {
    vec![
        "inspect deploy logs".to_string(),
        "run `psht health`".to_string(),
        "retry deploy".to_string(),
    ]
}

pub fn read_app_runtime_state(app: &str) -> Result<Option<AppRuntimeState>, String> {
    sqlite_store::get_app_runtime_state(app)?
        .map(app_runtime_state_from_row)
        .transpose()
}

pub fn write_app_runtime_state(app: &str, state: &AppRuntimeState) -> Result<(), String> {
    sqlite_store::upsert_app_runtime_state(&sqlite_store::AppRuntimeStateRow {
        app_id: app.to_string(),
        active_instance: state.active_instance.clone(),
        previous_instance: state.previous_instance.clone(),
        runtime_project: state.runtime_project.clone(),
        updated_at: sqlite_u64_to_i64(state.updated_at, app, "updated_at")?,
    })
}

pub fn clear_app_runtime_state(app: &str) -> Result<(), String> {
    sqlite_store::delete_app_runtime_state(app)
}

pub fn read_all_app_runtime_states() -> Result<Vec<(String, AppRuntimeState)>, String> {
    let rows = sqlite_store::list_app_runtime_states()?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let app = row.app_id.clone();
        out.push((app, app_runtime_state_from_row(row)?));
    }
    Ok(out)
}

pub fn desired_state(app: &str) -> Result<DesiredState, String> {
    let desired = sqlite_store::get_app_spec(app)?
        .map(|spec| spec.desired_state)
        .unwrap_or_else(|| DesiredState::Running.as_str().to_string());
    Ok(DesiredState::from_persisted(&desired))
}

pub fn set_desired_state(app: &str, desired_state: DesiredState) -> Result<(), String> {
    let row = if let Some(mut existing) = sqlite_store::get_app_spec(app)? {
        existing.desired_state = desired_state.as_str().to_string();
        existing
    } else {
        sqlite_store::AppSpecRow {
            app_id: app.to_string(),
            generation: 0,
            desired_state: desired_state.as_str().to_string(),
            source_kind: DEFAULT_SOURCE_KIND.to_string(),
            source_payload_json: EMPTY_JSON_OBJECT.to_string(),
            runtime_payload_json: EMPTY_JSON_OBJECT.to_string(),
        }
    };
    sqlite_store::upsert_app_spec(&row)
}

pub fn begin_reconcile_intent(
    app: &str,
    kind: &str,
    source_kind: &str,
    source_payload_json: &str,
    mut snapshot: RuntimeSnapshot,
) -> Result<ReconcileIntentContext, String> {
    let generation = sqlite_store::next_app_generation(app)?;
    let spec = sqlite_store::AppSpecRow {
        app_id: app.to_string(),
        generation,
        desired_state: DesiredState::Running.as_str().to_string(),
        source_kind: source_kind.to_string(),
        source_payload_json: source_payload_json.to_string(),
        runtime_payload_json: EMPTY_JSON_OBJECT.to_string(),
    };
    sqlite_store::upsert_app_spec(&spec)?;

    let intent_id = format!("{}-{}-{generation}", now_unix_secs(), process::id());
    sqlite_store::insert_deploy_intent(&sqlite_store::DeployIntentRow {
        intent_id: intent_id.clone(),
        app_id: app.to_string(),
        generation,
        kind: kind.to_string(),
        payload_json: source_payload_json.to_string(),
    })?;

    let plan_hash = format!("{app}:{kind}:{generation}");
    sqlite_store::upsert_reconcile_checkpoint(
        app,
        generation,
        &plan_hash,
        0,
        "intent-accepted",
        Some("{\"ok\":true}"),
    )?;

    if snapshot.candidate_instance.is_none() {
        snapshot.candidate_instance =
            sqlite_store::get_app_status(app)?.and_then(|status| status.candidate_instance);
    }

    upsert_status(&AppStatus {
        app_id: app.to_string(),
        observed_generation: generation,
        phase: AppPhase::Reconciling,
        active_instance: snapshot.active_instance,
        candidate_instance: snapshot.candidate_instance,
        previous_instance: snapshot.previous_instance,
        active_revision: snapshot.active_revision,
        candidate_revision: snapshot.candidate_revision,
        health: HealthState::reconciling(),
        last_error: None,
        recovery_actions: Vec::new(),
    })?;

    Ok(ReconcileIntentContext {
        intent_id,
        generation,
        plan_hash,
        source_summary: source_payload_json.to_string(),
    })
}

pub fn reconcile_checkpoint(
    app: &str,
    ctx: &ReconcileIntentContext,
    op_index: i64,
    op_name: &str,
    last_result_json: Option<&str>,
) -> Result<(), String> {
    sqlite_store::upsert_reconcile_checkpoint(
        app,
        ctx.generation,
        &ctx.plan_hash,
        op_index,
        op_name,
        last_result_json,
    )
}

pub fn persist_phase(
    app: &str,
    generation: i64,
    phase: AppPhase,
    snapshot: RuntimeSnapshot,
    last_error: Option<&str>,
) -> Result<(), String> {
    let health = if phase == AppPhase::Blocked {
        HealthState::blocked()
    } else {
        HealthState::reconciling()
    };
    upsert_status(&AppStatus {
        app_id: app.to_string(),
        observed_generation: generation,
        phase,
        active_instance: snapshot.active_instance,
        candidate_instance: snapshot.candidate_instance,
        previous_instance: snapshot.previous_instance,
        active_revision: snapshot.active_revision,
        candidate_revision: snapshot.candidate_revision,
        health,
        last_error: last_error.map(ErrorReport::new),
        recovery_actions: Vec::new(),
    })
}

pub fn complete_reconcile_intent(
    app: &str,
    ctx: &ReconcileIntentContext,
    result: &Result<(), String>,
    revision: Option<&str>,
    snapshot: RuntimeSnapshot,
) -> Result<(), String> {
    let (phase, last_error, health, recovery_actions, outcome, summary) = match result {
        Ok(()) => (
            AppPhase::Idle,
            None,
            HealthState::healthy(),
            Vec::new(),
            "success".to_string(),
            format!("deploy succeeded ({})", ctx.source_summary),
        ),
        Err(err) => (
            AppPhase::Degraded,
            Some(ErrorReport::with_generation(err, ctx.generation)),
            HealthState::unhealthy(),
            degraded_recovery_actions(),
            "failed".to_string(),
            err.to_string(),
        ),
    };

    upsert_status(&AppStatus {
        app_id: app.to_string(),
        observed_generation: ctx.generation,
        phase,
        active_instance: snapshot.active_instance,
        candidate_instance: snapshot.candidate_instance,
        previous_instance: snapshot.previous_instance,
        active_revision: revision
            .map(|value| value.to_string())
            .or(snapshot.active_revision),
        candidate_revision: snapshot.candidate_revision,
        health,
        last_error,
        recovery_actions,
    })?;

    if result.is_ok() {
        sqlite_store::clear_reconcile_checkpoint(app)?;
    } else {
        sqlite_store::upsert_reconcile_checkpoint(
            app,
            ctx.generation,
            &ctx.plan_hash,
            999,
            "failed",
            Some(&serialize_json(
                &serde_json::json!({
                    "error": summary,
                }),
                "failed reconcile checkpoint",
            )?),
        )?;
    }

    sqlite_store::append_deploy_history(app, ctx.generation, revision, &outcome, &summary)?;
    sqlite_store::mark_deploy_intent_processed(&ctx.intent_id)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PhaseRepairDecision {
    phase: AppPhase,
    health: HealthState,
    last_error: Option<ErrorReport>,
    recovery_actions: Vec<String>,
    clear_checkpoint: bool,
}

fn stale_reconcile_error(
    checkpoint: Option<&sqlite_store::ReconcileCheckpointRow>,
    repair_error: Option<&str>,
) -> String {
    let base = match checkpoint {
        Some(checkpoint) => format!(
            "reconcile stalled without an active lease at step '{}'",
            checkpoint.op_name
        ),
        None => "reconcile stalled without an active lease".to_string(),
    };
    match repair_error {
        Some(err) if !err.trim().is_empty() => format!("{base}: {err}"),
        _ => base,
    }
}

fn phase_repair_decision(
    now_ms: i64,
    status: &sqlite_store::AppStatusRow,
    checkpoint: Option<&sqlite_store::ReconcileCheckpointRow>,
    lease: Option<&sqlite_store::AppLeaseRow>,
    converged: bool,
    repair_error: Option<&str>,
    stale_after_ms: i64,
) -> Option<PhaseRepairDecision> {
    let phase = AppPhase::from_persisted(&status.phase)?;
    if lease.is_some_and(|lease| lease.expires_at_ms > now_ms) {
        return None;
    }

    if phase == AppPhase::Degraded {
        return converged.then(|| PhaseRepairDecision {
            phase: AppPhase::Idle,
            health: HealthState::healthy(),
            last_error: None,
            recovery_actions: Vec::new(),
            clear_checkpoint: true,
        });
    }

    if !matches!(phase, AppPhase::Reconciling | AppPhase::Blocked) {
        return None;
    }

    let last_update_ms = checkpoint
        .map(|checkpoint| checkpoint.updated_at_ms)
        .unwrap_or(status.updated_at_ms)
        .max(status.updated_at_ms);
    if now_ms < last_update_ms.saturating_add(stale_after_ms.max(0)) {
        return None;
    }

    if converged {
        return Some(PhaseRepairDecision {
            phase: AppPhase::Idle,
            health: HealthState::healthy(),
            last_error: None,
            recovery_actions: Vec::new(),
            clear_checkpoint: true,
        });
    }

    Some(PhaseRepairDecision {
        phase: AppPhase::Degraded,
        health: HealthState::unhealthy(),
        last_error: Some(ErrorReport::with_generation(
            stale_reconcile_error(checkpoint, repair_error),
            status.observed_generation,
        )),
        recovery_actions: degraded_recovery_actions(),
        clear_checkpoint: false,
    })
}

fn repair_stale_phase_at(
    app: &str,
    snapshot: RuntimeSnapshot,
    converged: bool,
    repair_error: Option<&str>,
    stale_after_ms: i64,
    now_ms: i64,
) -> Result<bool, String> {
    let Some(status) = sqlite_store::get_app_status(app)? else {
        return Ok(false);
    };
    let checkpoint = sqlite_store::get_reconcile_checkpoint(app)?;
    let lease = sqlite_store::get_app_lease(app)?;
    let Some(decision) = phase_repair_decision(
        now_ms,
        &status,
        checkpoint.as_ref(),
        lease.as_ref(),
        converged,
        repair_error,
        stale_after_ms,
    ) else {
        return Ok(false);
    };

    upsert_status(&AppStatus {
        app_id: app.to_string(),
        observed_generation: status.observed_generation,
        phase: decision.phase,
        active_instance: snapshot.active_instance.or(status.active_instance.clone()),
        candidate_instance: if decision.clear_checkpoint {
            None
        } else {
            snapshot
                .candidate_instance
                .or(status.candidate_instance.clone())
        },
        previous_instance: snapshot
            .previous_instance
            .or(status.previous_instance.clone()),
        active_revision: snapshot.active_revision.or(status.active_revision.clone()),
        candidate_revision: if decision.clear_checkpoint {
            None
        } else {
            snapshot
                .candidate_revision
                .or(status.candidate_revision.clone())
        },
        health: decision.health,
        last_error: decision.last_error,
        recovery_actions: decision.recovery_actions,
    })?;

    if decision.clear_checkpoint {
        sqlite_store::clear_reconcile_checkpoint(app)?;
    }

    Ok(true)
}

pub fn repair_stale_phase(
    app: &str,
    snapshot: RuntimeSnapshot,
    converged: bool,
    repair_error: Option<&str>,
    stale_after_ms: i64,
) -> Result<bool, String> {
    repair_stale_phase_at(
        app,
        snapshot,
        converged,
        repair_error,
        stale_after_ms,
        now_unix_ms(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite_store;

    fn unique_app(prefix: &str) -> String {
        format!("{prefix}-{}-{}", std::process::id(), now_unix_ms())
    }

    #[test]
    fn desired_state_normalizes_unknown_values_to_running() {
        assert_eq!(
            DesiredState::from_persisted("stopped"),
            DesiredState::Stopped
        );
        assert_eq!(
            DesiredState::from_persisted("running"),
            DesiredState::Running
        );
        assert_eq!(DesiredState::from_persisted("weird"), DesiredState::Running);
    }

    #[test]
    fn phase_strings_match_persisted_values() {
        assert_eq!(AppPhase::Idle.as_str(), "Idle");
        assert_eq!(AppPhase::Reconciling.as_str(), "Reconciling");
        assert_eq!(AppPhase::Blocked.as_str(), "Blocked");
        assert_eq!(AppPhase::Degraded.as_str(), "Degraded");
        assert_eq!(AppPhase::from_persisted("Idle"), Some(AppPhase::Idle));
        assert_eq!(AppPhase::from_persisted("weird"), None);
    }

    #[test]
    fn health_states_encode_expected_reason() {
        assert_eq!(HealthState::healthy().reason, None);
        assert_eq!(
            HealthState::reconciling().reason.as_deref(),
            Some("reconciling")
        );
        assert_eq!(HealthState::blocked().reason.as_deref(), Some("blocked"));
    }

    #[test]
    fn stale_reconciling_phase_repairs_to_idle_when_converged() {
        let app = unique_app("phase-repair-idle");

        sqlite_store::upsert_app_status(&sqlite_store::AppStatusRow {
            app_id: app.clone(),
            observed_generation: 7,
            phase: AppPhase::Reconciling.as_str().to_string(),
            active_instance: Some("psht-demo".to_string()),
            candidate_instance: Some("psht-demo-build-1".to_string()),
            previous_instance: None,
            active_revision: Some("abc".to_string()),
            candidate_revision: Some("def".to_string()),
            health_json: "{\"healthy\":false,\"reason\":\"reconciling\"}".to_string(),
            last_error_json: None,
            recovery_actions_json: "[]".to_string(),
            updated_at_ms: 0,
        })
        .unwrap();
        sqlite_store::upsert_reconcile_checkpoint(&app, 7, "plan-1", 3, "start", Some("{}"))
            .unwrap();

        let status = sqlite_store::get_app_status(&app).unwrap().unwrap();
        let repaired = repair_stale_phase_at(
            &app,
            RuntimeSnapshot {
                active_instance: Some("psht-demo".to_string()),
                active_revision: Some("abc".to_string()),
                ..RuntimeSnapshot::default()
            },
            true,
            None,
            50,
            status.updated_at_ms + 100,
        )
        .unwrap();

        assert!(repaired);
        let status = sqlite_store::get_app_status(&app).unwrap().unwrap();
        assert_eq!(status.phase, AppPhase::Idle.as_str());
        assert!(status.candidate_instance.is_none());
        assert!(
            sqlite_store::get_reconcile_checkpoint(&app)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn stale_reconciling_phase_repairs_to_degraded_when_not_converged() {
        let app = unique_app("phase-repair-degraded");

        sqlite_store::upsert_app_status(&sqlite_store::AppStatusRow {
            app_id: app.clone(),
            observed_generation: 9,
            phase: AppPhase::Reconciling.as_str().to_string(),
            active_instance: Some("psht-demo".to_string()),
            candidate_instance: Some("psht-demo-build-2".to_string()),
            previous_instance: None,
            active_revision: Some("abc".to_string()),
            candidate_revision: Some("def".to_string()),
            health_json: "{\"healthy\":false,\"reason\":\"reconciling\"}".to_string(),
            last_error_json: None,
            recovery_actions_json: "[]".to_string(),
            updated_at_ms: 0,
        })
        .unwrap();
        sqlite_store::upsert_reconcile_checkpoint(
            &app,
            9,
            "plan-2",
            4,
            "wait-for-operation",
            Some("{}"),
        )
        .unwrap();

        let status = sqlite_store::get_app_status(&app).unwrap().unwrap();
        let repaired = repair_stale_phase_at(
            &app,
            RuntimeSnapshot::default(),
            false,
            Some("supervision could not recover app"),
            50,
            status.updated_at_ms + 100,
        )
        .unwrap();

        assert!(repaired);
        let status = sqlite_store::get_app_status(&app).unwrap().unwrap();
        assert_eq!(status.phase, AppPhase::Degraded.as_str());
        assert!(
            status
                .last_error_json
                .as_deref()
                .unwrap()
                .contains("wait-for-operation")
        );
        assert!(
            sqlite_store::get_reconcile_checkpoint(&app)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn degraded_phase_repairs_to_idle_when_converged() {
        let app = unique_app("phase-repair-healed");

        sqlite_store::upsert_app_status(&sqlite_store::AppStatusRow {
            app_id: app.clone(),
            observed_generation: 11,
            phase: AppPhase::Degraded.as_str().to_string(),
            active_instance: Some("psht-demo".to_string()),
            candidate_instance: Some("psht-demo-build-3".to_string()),
            previous_instance: None,
            active_revision: Some("abc".to_string()),
            candidate_revision: Some("def".to_string()),
            health_json: "{\"healthy\":false}".to_string(),
            last_error_json: Some("{\"error\":\"boom\"}".to_string()),
            recovery_actions_json: "[\"retry\"]".to_string(),
            updated_at_ms: 0,
        })
        .unwrap();
        sqlite_store::upsert_reconcile_checkpoint(&app, 11, "plan-3", 999, "failed", Some("{}"))
            .unwrap();

        let repaired = repair_stale_phase_at(
            &app,
            RuntimeSnapshot {
                active_instance: Some("psht-demo".to_string()),
                active_revision: Some("abc".to_string()),
                ..RuntimeSnapshot::default()
            },
            true,
            None,
            50,
            now_unix_ms(),
        )
        .unwrap();

        assert!(repaired);
        let status = sqlite_store::get_app_status(&app).unwrap().unwrap();
        assert_eq!(status.phase, AppPhase::Idle.as_str());
        assert!(status.last_error_json.is_none());
        assert!(
            sqlite_store::get_reconcile_checkpoint(&app)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn recent_reconciling_phase_is_not_repaired_before_timeout() {
        let app = unique_app("phase-repair-recent");

        sqlite_store::upsert_app_status(&sqlite_store::AppStatusRow {
            app_id: app.clone(),
            observed_generation: 13,
            phase: AppPhase::Reconciling.as_str().to_string(),
            active_instance: Some("psht-demo".to_string()),
            candidate_instance: None,
            previous_instance: None,
            active_revision: Some("abc".to_string()),
            candidate_revision: None,
            health_json: "{\"healthy\":false,\"reason\":\"reconciling\"}".to_string(),
            last_error_json: None,
            recovery_actions_json: "[]".to_string(),
            updated_at_ms: 0,
        })
        .unwrap();

        let status = sqlite_store::get_app_status(&app).unwrap().unwrap();
        let repaired = repair_stale_phase_at(
            &app,
            RuntimeSnapshot::default(),
            true,
            None,
            1_000,
            status.updated_at_ms + 100,
        )
        .unwrap();

        assert!(!repaired);
        let status = sqlite_store::get_app_status(&app).unwrap().unwrap();
        assert_eq!(status.phase, AppPhase::Reconciling.as_str());
    }

    #[test]
    fn live_lease_prevents_phase_repair() {
        let app = unique_app("phase-repair-lease");

        sqlite_store::upsert_app_status(&sqlite_store::AppStatusRow {
            app_id: app.clone(),
            observed_generation: 15,
            phase: AppPhase::Reconciling.as_str().to_string(),
            active_instance: Some("psht-demo".to_string()),
            candidate_instance: Some("psht-demo-build-4".to_string()),
            previous_instance: None,
            active_revision: Some("abc".to_string()),
            candidate_revision: Some("def".to_string()),
            health_json: "{\"healthy\":false,\"reason\":\"reconciling\"}".to_string(),
            last_error_json: None,
            recovery_actions_json: "[]".to_string(),
            updated_at_ms: 0,
        })
        .unwrap();
        let lease =
            sqlite_store::acquire_app_lease(&app, "owner-a", "intent-a", 15, 5_000).unwrap();

        let repaired = repair_stale_phase_at(
            &app,
            RuntimeSnapshot::default(),
            true,
            None,
            0,
            lease.expires_at_ms - 1,
        )
        .unwrap();

        assert!(!repaired);
        let status = sqlite_store::get_app_status(&app).unwrap().unwrap();
        assert_eq!(status.phase, AppPhase::Reconciling.as_str());
    }
}
