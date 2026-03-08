use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde_json::Value;

use crate::control_plane::{self, AppPhase, RuntimeSnapshot};
use crate::sqlite_store;

#[derive(Debug, Clone)]
struct ActiveReconcileLease {
    owner: String,
    epoch: i64,
    intent_id: String,
    generation: i64,
    last_heartbeat: Instant,
}

static ACTIVE_RECONCILE_LEASES: OnceLock<Mutex<HashMap<String, ActiveReconcileLease>>> =
    OnceLock::new();

fn active_reconcile_leases() -> &'static Mutex<HashMap<String, ActiveReconcileLease>> {
    ACTIVE_RECONCILE_LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_active_reconcile_lease(app: &str, lease: ActiveReconcileLease) {
    if let Ok(mut map) = active_reconcile_leases().lock() {
        map.insert(app.to_string(), lease);
    }
}

fn active_reconcile_lease_for(app: &str) -> Option<ActiveReconcileLease> {
    active_reconcile_leases()
        .lock()
        .ok()
        .and_then(|map| map.get(app).cloned())
}

fn remove_active_reconcile_lease(app: &str) {
    if let Ok(mut map) = active_reconcile_leases().lock() {
        map.remove(app);
    }
}

pub fn append_attempt(app: &str, step_name: &str, result: &str, detail_json: Value) {
    let Some(active) = active_reconcile_lease_for(app) else {
        return;
    };
    let started = now_unix_ms();
    let row = sqlite_store::ReconcileAttemptRow {
        app_id: app.to_string(),
        intent_id: active.intent_id.clone(),
        generation: active.generation,
        step_name: step_name.to_string(),
        started_at_ms: started,
        finished_at_ms: Some(started),
        result: result.to_string(),
        detail_json: detail_json.to_string(),
    };
    if let Err(err) = sqlite_store::append_reconcile_attempt(&row) {
        eprintln!("       Warning: failed to append reconcile attempt: {err}");
    }
}

pub fn update_phase(
    app: &str,
    phase: AppPhase,
    snapshot: RuntimeSnapshot,
    last_error: Option<&str>,
) {
    let Some(active) = active_reconcile_lease_for(app) else {
        return;
    };
    let _ = control_plane::persist_phase(app, active.generation, phase, snapshot, last_error);
}

pub fn refresh(app: &str, ttl_ms: i64, heartbeat_secs: u64) -> Result<(), String> {
    let Some(active) = active_reconcile_lease_for(app) else {
        return Ok(());
    };
    if active.last_heartbeat.elapsed().as_secs() < heartbeat_secs {
        return Ok(());
    }

    sqlite_store::heartbeat_app_lease(app, &active.owner, active.epoch, ttl_ms)?;

    if let Ok(mut map) = active_reconcile_leases().lock()
        && let Some(entry) = map.get_mut(app)
    {
        entry.last_heartbeat = Instant::now();
    }
    Ok(())
}

pub struct ReconcileLeaseGuard {
    app: String,
    owner: String,
    epoch: i64,
}

impl Drop for ReconcileLeaseGuard {
    fn drop(&mut self) {
        remove_active_reconcile_lease(&self.app);
        if let Err(err) = sqlite_store::release_app_lease(&self.app, &self.owner, self.epoch) {
            std::eprintln!(
                "warning: failed to release reconcile lease for {}: {}",
                self.app,
                err
            );
        }
    }
}

pub fn acquire(
    app: &str,
    owner: &str,
    intent_id: &str,
    generation: i64,
    ttl_ms: i64,
) -> Result<ReconcileLeaseGuard, String> {
    let lease = sqlite_store::acquire_app_lease(app, owner, intent_id, generation, ttl_ms)?;

    register_active_reconcile_lease(
        app,
        ActiveReconcileLease {
            owner: lease.lease_owner.clone(),
            epoch: lease.lease_epoch,
            intent_id: lease.intent_id.clone(),
            generation: lease.generation,
            last_heartbeat: Instant::now(),
        },
    );

    Ok(ReconcileLeaseGuard {
        app: app.to_string(),
        owner: lease.lease_owner,
        epoch: lease.lease_epoch,
    })
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}
