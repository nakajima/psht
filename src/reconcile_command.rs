use std::time::Instant;

use crate::control_plane::{self, ReconcileIntentContext, RuntimeSnapshot};
use crate::stats;

pub struct ReconcileCommandRequest<'a> {
    pub kind: &'a str,
    pub source_kind: &'a str,
    pub source_payload_json: &'a str,
    pub force: bool,
    pub start_step_name: &'a str,
}

pub fn run<F, G, H>(
    app: &str,
    request: ReconcileCommandRequest<'_>,
    mut snapshot: G,
    operation: F,
    revision: H,
) -> Result<(), String>
where
    F: FnOnce(&ReconcileIntentContext) -> Result<(), String>,
    G: FnMut() -> RuntimeSnapshot,
    H: FnOnce() -> Option<String>,
{
    let started = Instant::now();
    let ctx = control_plane::begin_reconcile_intent(
        app,
        request.kind,
        request.source_kind,
        request.source_payload_json,
        snapshot(),
    )?;

    if let Err(err) = control_plane::reconcile_checkpoint(
        app,
        &ctx,
        1,
        request.start_step_name,
        Some("{\"ok\":true}"),
    ) {
        eprintln!("warning: failed to persist reconcile checkpoint: {err}");
    }

    let result = operation(&ctx);
    let revision = revision();

    stats::report_deploy_attempt(stats::DeployAttempt {
        app,
        kind: request.kind,
        generation: ctx.generation,
        attempt: 1,
        force: request.force,
        success: result.is_ok(),
        duration: started.elapsed(),
        error: result.as_ref().err().map(|err| err.as_str()),
    });

    if let Err(err) = control_plane::complete_reconcile_intent(
        app,
        &ctx,
        &result,
        revision.as_deref(),
        snapshot(),
    ) {
        eprintln!("warning: failed to finalize reconcile intent: {err}");
    }

    result
}
