use super::*;

pub(super) fn is_transient_deploy_app_for(app: &str, candidate: &str) -> bool {
    let build_prefix = format!("{app}-build-");
    if let Some(suffix) = candidate.strip_prefix(&build_prefix) {
        return !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit());
    }

    let prev_prefix = format!("{app}-prev-");
    if let Some(suffix) = candidate.strip_prefix(&prev_prefix) {
        return !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit());
    }

    let failed_prefix = format!("{app}-failed-");
    if let Some(suffix) = candidate.strip_prefix(&failed_prefix) {
        return !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit());
    }

    false
}

fn previous_cleanup_target_allowed(app: &str, candidate: &str) -> bool {
    candidate == app || is_transient_deploy_app_for(app, candidate)
}

fn clear_previous_runtime_reference_if_missing(app: &str) -> Result<(), String> {
    let Some(state) = read_app_runtime_state(app)? else {
        return Ok(());
    };

    let Some(previous_instance) = state.previous_instance.as_deref() else {
        return Ok(());
    };
    let Some(previous_app_ref) = app_ref_from_instance_name(previous_instance) else {
        return Ok(());
    };
    if container::exists(&previous_app_ref) {
        return Ok(());
    }

    let Some(active_app_ref) = app_ref_from_instance_name(&state.active_instance) else {
        return Ok(());
    };
    write_app_runtime_state(app, &active_app_ref, None)
}

fn record_cleanup_job_failure(
    app: &str,
    mut job: CleanupJobState,
    error_message: String,
) -> Result<(), String> {
    job.attempts = job.attempts.saturating_add(1);
    job.last_error = Some(error_message.clone());
    job.updated_at = now_unix_secs();
    write_cleanup_job(app, &job)?;
    Err(error_message)
}

pub fn cleanup_previous(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let Some(_cleanup_lock) = try_acquire_cleanup_lock(app)? else {
        return Ok(());
    };

    let Some(job) = read_cleanup_job(app)? else {
        return Ok(());
    };
    if job.app != app {
        let job_app = job.app.clone();
        return record_cleanup_job_failure(
            app,
            job,
            format!(
                "cleanup job app mismatch: expected '{app}', found '{}'",
                job_app
            ),
        );
    }

    let Some(scheduled_previous_app) = app_ref_from_instance_name(&job.scheduled_previous_instance)
    else {
        return record_cleanup_job_failure(
            app,
            job,
            "cleanup job has invalid scheduled previous instance".to_string(),
        );
    };
    if !previous_cleanup_target_allowed(app, &scheduled_previous_app) {
        clear_cleanup_job(app)?;
        return Ok(());
    }

    let active_app = resolve_active_app_ref(app)?;
    if active_app.as_deref() == Some(scheduled_previous_app.as_str()) {
        eprintln!(
            "       Skipping background cleanup because previous instance became active again"
        );
        clear_cleanup_job(app)?;
        return Ok(());
    }

    if !container::exists(&scheduled_previous_app) {
        clear_previous_runtime_reference_if_missing(app)?;
        clear_cleanup_job(app)?;
        return Ok(());
    }

    let project = current_project_name()?;
    if env::var_os("PSHT_SKIP_TAILSCALE").is_none() {
        let _ = container::exec_cmd(
            &scheduled_previous_app,
            "tailscale down >/dev/null 2>&1 || true",
        );
    }

    if let Err(err) = cleanup_container_for_rebuild(&scheduled_previous_app, &project) {
        return record_cleanup_job_failure(
            app,
            job,
            format!("failed to clean previous instance '{scheduled_previous_app}': {err}"),
        );
    }

    clear_previous_runtime_reference_if_missing(app)?;
    clear_cleanup_job(app)?;
    Ok(())
}

pub(super) fn cleanup_pending_detail(app: &str) -> Option<String> {
    let state = read_cleanup_job(app).ok().flatten()?;
    let mut detail = format!("cleanup pending (attempts: {})", state.attempts);
    if let Some(last_error) = state.last_error.as_deref() {
        detail.push_str(&format!(", last error: {last_error}"));
    }
    Some(detail)
}

fn start_deploy_log_session(app: &str) -> Option<deploy_log::DeployLogSession> {
    match deploy_log::start_for_app(app) {
        Ok(session) => Some(session),
        Err(err) => {
            std::eprintln!("       Warning: failed to initialize deploy logs: {err}");
            None
        }
    }
}

fn deploy_request_id() -> String {
    format!("{}-{}", now_unix_secs(), std::process::id())
}

pub(super) fn deploy_interrupted_error(phase: &str, state: &DeployInterruptState) -> String {
    format!(
        "{DEPLOY_INTERRUPT_ERR_PREFIX} (phase: {phase}, request: {}, requested_at: {})",
        state.request_id, state.requested_at
    )
}

pub(super) fn is_deploy_interrupted_error(err: &str) -> bool {
    err.starts_with(DEPLOY_INTERRUPT_ERR_PREFIX)
}

fn deploy_result_was_interrupted(result: &Result<(), String>) -> bool {
    matches!(result, Err(err) if is_deploy_interrupted_error(err))
}

fn deploy_interrupt_state_for_signal(app: &str) -> DeployInterruptState {
    let now = now_unix_secs();
    let target_sha = read_git_deploy_state(app)
        .ok()
        .flatten()
        .map(|state| state.sha)
        .unwrap_or_else(|| "unknown".to_string());
    DeployInterruptState {
        request_id: format!("signal-{}-{now}", process::id()),
        requested_at: now,
        target_sha,
    }
}

fn check_signal_interrupt_without_persist(app: &str, phase: &str) -> Result<(), String> {
    if !DEPLOY_INTERRUPT_SIGNAL_PENDING.load(Ordering::SeqCst) {
        return Ok(());
    }
    let state = deploy_interrupt_state_for_signal(app);
    Err(deploy_interrupted_error(phase, &state))
}

fn check_signal_interrupt_with_persist(app: &str, phase: &str) -> Result<(), String> {
    if !DEPLOY_INTERRUPT_SIGNAL_PENDING.load(Ordering::SeqCst) {
        return Ok(());
    }

    if let Some(state) = read_deploy_interrupt(app)? {
        return Err(deploy_interrupted_error(phase, &state));
    }

    let state = deploy_interrupt_state_for_signal(app);
    if let Err(err) = request_deploy_interrupt(app, &state) {
        eprintln!("       Warning: failed to record signal interrupt state: {err}");
    }
    Err(deploy_interrupted_error(phase, &state))
}

pub(super) fn check_deploy_interrupt(app: &str, phase: &str) -> Result<(), String> {
    if let Err(err) = refresh_deploy_lock_heartbeat(app) {
        eprintln!("       Warning: failed to refresh deploy lock heartbeat: {err}");
    }
    refresh_active_reconcile_lease(app)
        .map_err(|err| format!("reconcile lease check failed during {phase}: {err}"))?;
    check_signal_interrupt_with_persist(app, phase)?;
    if let Some(state) = read_deploy_interrupt(app)? {
        return Err(deploy_interrupted_error(phase, &state));
    }
    Ok(())
}

pub(super) fn should_process_pending_request(
    active_target: Option<&GitCheckoutTarget>,
    pending: &PendingGitDeployRequest,
) -> bool {
    if pending.force {
        return true;
    }
    active_target.map(|target| target.sha.as_str()) != Some(pending.sha.as_str())
}

pub(super) fn pending_force_request_is_ours(
    pending_request: Option<&PendingGitDeployRequest>,
    request_id: &str,
    target_sha: &str,
) -> bool {
    matches!(
        pending_request,
        Some(request)
            if request.request_id.as_deref() == Some(request_id)
                || (request.request_id.is_none() && request.force && request.sha == target_sha)
    )
}

fn clear_owned_deploy_interrupt(app: &str, request_id: &str) -> Result<bool, String> {
    let Some(state) = read_deploy_interrupt(app)? else {
        return Ok(false);
    };
    if state.request_id != request_id {
        return Ok(false);
    }
    clear_deploy_interrupt(app)?;
    Ok(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ForcedDeployTakeoverPrep {
    cleared_interrupt: bool,
    cleared_pending_request: bool,
}

fn prepare_for_forced_deploy_takeover(
    app: &str,
    target: &GitCheckoutTarget,
    request_id: &str,
) -> Result<ForcedDeployTakeoverPrep, String> {
    let cleared_interrupt = clear_owned_deploy_interrupt(app, request_id)?;
    let pending_request = read_pending_git_request(app)?;
    let cleared_pending_request =
        pending_force_request_is_ours(pending_request.as_ref(), request_id, &target.sha);
    if cleared_pending_request {
        let _ = take_pending_git_request(app)?;
    }
    Ok(ForcedDeployTakeoverPrep {
        cleared_interrupt,
        cleared_pending_request,
    })
}

fn deploy_once(
    app: &str,
    target: Option<&GitCheckoutTarget>,
    force: bool,
    force_fresh_setup_image: bool,
) -> Result<(), String> {
    if let Some(target) = target {
        if !force {
            match git_target_already_succeeded(app, target) {
                Ok(true) => {
                    eprintln!("-----> Current git revision already deployed successfully");
                    return Ok(());
                }
                Ok(false) => {}
                Err(err) => {
                    eprintln!("       Warning: failed to read git deploy state: {err}");
                }
            }
        }
        if let Err(err) = write_git_deploy_state(app, target, GitDeployStatus::Pending) {
            eprintln!("       Warning: failed to record pending git deploy state: {err}");
        }
    }

    check_deploy_interrupt(app, "before checkout")?;
    eprintln!("-----> Checking out code");
    if let Some(target) = target {
        eprintln!("       Ref: {} ({})", target.ref_name, target.sha);
    }
    let result = (|| {
        let build_dir = checkout_code(app, target)?;
        check_deploy_interrupt(app, "after checkout")?;
        deploy_from(app, &build_dir, force_fresh_setup_image)
    })();

    match result {
        Ok(()) => {
            if let Some(target) = target {
                if let Err(err) = write_git_deploy_state(app, target, GitDeployStatus::Success) {
                    eprintln!(
                        "       Warning: failed to persist successful git deploy state: {err}"
                    );
                }
            } else if let Err(err) = clear_git_deploy_state(app) {
                eprintln!("       Warning: failed to clear git deploy state: {err}");
            }
            Ok(())
        }
        Err(err) => {
            if let Some(target) = target {
                let status = if is_deploy_interrupted_error(&err) {
                    GitDeployStatus::Interrupted
                } else {
                    GitDeployStatus::Failed
                };
                if let Err(write_err) = write_git_deploy_state(app, target, status) {
                    eprintln!(
                        "       Warning: failed to record failed git deploy state: {write_err}"
                    );
                }
            }
            Err(err)
        }
    }
}

pub(super) fn control_plane_snapshot(app: &str) -> RuntimeSnapshot {
    let active_revision = read_git_deploy_state(app)
        .ok()
        .flatten()
        .and_then(|state| (state.status == GitDeployStatus::Success).then_some(state.sha));
    app_state::control_plane_snapshot(app, active_revision)
}

fn normalized_desired_state(value: &str) -> &'static str {
    if value.eq_ignore_ascii_case(DESIRED_STATE_STOPPED) {
        DESIRED_STATE_STOPPED
    } else {
        DESIRED_STATE_RUNNING
    }
}

pub(super) fn app_desired_state(app: &str) -> Result<&'static str, String> {
    let desired = app_state::desired_state(app)?;
    Ok(match desired {
        DesiredState::Running => DESIRED_STATE_RUNNING,
        DesiredState::Stopped => DESIRED_STATE_STOPPED,
    })
}

pub(super) fn set_app_desired_state(app: &str, desired_state: &str) -> Result<(), String> {
    let desired_state = match normalized_desired_state(desired_state) {
        DESIRED_STATE_STOPPED => DesiredState::Stopped,
        _ => DesiredState::Running,
    };
    app_state::set_desired_state(app, desired_state)
}

fn queue_pending_git_deploy(
    app: &str,
    target: &GitCheckoutTarget,
    force: bool,
    request_id: Option<String>,
) -> Result<PendingGitDeployRequest, String> {
    let request = PendingGitDeployRequest::from_target(
        target,
        force,
        request_id.clone(),
        request_id.as_ref().map(|_| now_unix_secs()),
    );
    write_pending_git_request(app, &request)?;
    if let Err(err) = write_git_deploy_state(app, target, GitDeployStatus::Pending) {
        eprintln!("       Warning: failed to record pending git deploy state: {err}");
    }
    Ok(request)
}

fn wait_for_forced_pending_deploy_completion(
    app: &str,
    target: &GitCheckoutTarget,
    request_id: &str,
) -> Result<(), String> {
    let mut last_status_line = String::new();
    let started = Instant::now();
    let mut next_heartbeat = DEPLOY_INTERRUPT_WAIT_HEARTBEAT_SECS;

    loop {
        let lock_active = deploy_lock_path(app).exists();
        let pending_request = read_pending_git_request(app)?;
        let state = read_git_deploy_state(app)?;
        let interrupt_state = read_deploy_interrupt(app)?;
        let elapsed = started.elapsed().as_secs();
        let interrupt_is_ours = matches!(
            interrupt_state.as_ref(),
            Some(DeployInterruptState { request_id: id, .. }) if id == request_id
        );
        let pending_is_ours =
            pending_force_request_is_ours(pending_request.as_ref(), request_id, &target.sha);

        if matches!(
            state.as_ref(),
            Some(GitDeployState {
                sha,
                status: GitDeployStatus::Success,
                ..
            }) if sha == &target.sha
        ) && !pending_is_ours
        {
            eprintln!(
                "=====> Forced deploy complete for {} ({})",
                target.ref_name, target.sha
            );
            let _ = clear_owned_deploy_interrupt(app, request_id);
            return Ok(());
        }

        if lock_active
            && interrupt_is_ours
            && pending_is_ours
            && elapsed >= DEPLOY_FORCE_TAKEOVER_TIMEOUT_SECS
        {
            eprintln!("-----> Force timeout reached ({elapsed}s); escalating to lock takeover");
            let lock_path = deploy_lock_path(app);
            match read_deploy_lock_metadata(app)? {
                Some(metadata) => {
                    if let Some(pid) = metadata.pid {
                        let alive = pid_is_alive(pid);
                        eprintln!("       Lock holder pid {pid} (alive: {alive})");
                        if alive {
                            eprintln!("       Sending SIGKILL to lock holder pid {pid}");
                            send_kill_signal(pid)?;
                            let mut exited = false;
                            for _ in 0..DEPLOY_FORCE_KILL_WAIT_CHECKS {
                                if !pid_is_alive(pid) {
                                    exited = true;
                                    break;
                                }
                                thread::sleep(Duration::from_millis(DEPLOY_FORCE_KILL_WAIT_MS));
                            }
                            if !exited {
                                return Err(format!(
                                    "lock holder pid {pid} did not exit after SIGKILL"
                                ));
                            }
                        }
                    } else {
                        eprintln!(
                            "       Warning: deploy lock metadata missing pid; forcing lock takeover"
                        );
                    }
                }
                None => {
                    eprintln!("       Deploy lock disappeared during takeover escalation");
                }
            }
            clear_deploy_lock(app)?;
            let Some(_lock) = try_acquire_deploy_lock(app)? else {
                return Err(format!(
                    "failed to acquire deploy lock after forced takeover at {}; a concurrent deploy may still be running",
                    lock_path.display()
                ));
            };
            eprintln!("-----> Starting forced deploy takeover");
            let prep = prepare_for_forced_deploy_takeover(app, target, request_id)?;
            if prep.cleared_interrupt {
                eprintln!("       Cleared interrupt request for forced deploy takeover");
            }
            let result = deploy_once(app, Some(target), true, false);
            let _ = clear_owned_deploy_interrupt(app, request_id);
            return result;
        }

        if !lock_active
            && interrupt_is_ours
            && let Some(_lock) = try_acquire_deploy_lock(app)?
        {
            eprintln!(
                "-----> Active deploy did not acknowledge interrupt; taking over forced deploy"
            );
            let prep = prepare_for_forced_deploy_takeover(app, target, request_id)?;
            if prep.cleared_interrupt {
                eprintln!("       Cleared interrupt request for forced deploy takeover");
            }
            let result = deploy_once(app, Some(target), true, false);
            let _ = clear_owned_deploy_interrupt(app, request_id);
            return result;
        }

        if !lock_active
            && !matches!(pending_request.as_ref(), Some(request) if request.sha == target.sha)
            && matches!(
                state.as_ref(),
                Some(GitDeployState {
                    sha,
                    status: GitDeployStatus::Failed | GitDeployStatus::Interrupted,
                    ..
                }) if sha == &target.sha
            )
        {
            return Err(format!(
                "forced deploy did not complete successfully for {} ({})",
                target.ref_name, target.sha
            ));
        }

        let status_line = if let Some(request) = pending_request.as_ref() {
            let req_id = request.request_id.as_deref().unwrap_or("-");
            if request.request_id.as_deref() != Some(request_id) {
                format!(
                    "forced request superseded by newer pending request {req_id}; waiting for latest"
                )
            } else if lock_active {
                "interrupt requested; waiting for active deploy to stop".to_string()
            } else {
                "interrupt requested; waiting for forced deploy to start".to_string()
            }
        } else if lock_active {
            "forced pending picked up; waiting for deploy completion".to_string()
        } else {
            match state.as_ref() {
                Some(GitDeployState { sha, status, .. }) if sha == &target.sha => {
                    format!("last deploy status for target is {:?}", status).to_lowercase()
                }
                _ => "waiting for deploy scheduler state".to_string(),
            }
        };

        let status_line = if lock_active
            && elapsed >= DEPLOY_INTERRUPT_WAIT_HEARTBEAT_SECS.saturating_mul(3)
            && status_line == "interrupt requested; waiting for active deploy to stop"
        {
            "interrupt requested; waiting for active deploy to stop (it may be in a long-running command)".to_string()
        } else {
            status_line
        };
        if status_line != last_status_line || elapsed >= next_heartbeat {
            eprintln!("       {status_line} ({elapsed}s elapsed)");
            last_status_line = status_line;
            next_heartbeat = elapsed.saturating_add(DEPLOY_INTERRUPT_WAIT_HEARTBEAT_SECS);
        }

        thread::sleep(Duration::from_millis(DEPLOY_INTERRUPT_WAIT_POLL_MS));
    }
}

pub fn deploy(
    app: &str,
    git_ref: Option<&str>,
    git_sha: Option<&str>,
    force: bool,
) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    install_deploy_interrupt_signal_handlers();
    let source_payload = serde_json::json!({
        "mode": "git",
        "ref": git_ref,
        "sha": git_sha,
        "force": force,
    })
    .to_string();
    check_signal_interrupt_without_persist(app, "deploy scheduling")?;
    let result = reconcile_command::run(
        app,
        ReconcileCommandRequest {
            kind: "deploy",
            source_kind: "git",
            source_payload_json: &source_payload,
            force,
            start_step_name: "deploy-start",
        },
        || control_plane_snapshot(app),
        |ctx| deploy_impl(app, git_ref, git_sha, force, false, ctx),
        || {
            parse_git_checkout_target(git_ref, git_sha)
                .ok()
                .and_then(|target| target.map(|target| target.sha))
                .or_else(|| {
                    read_git_deploy_state(app).ok().flatten().and_then(|state| {
                        (state.status == GitDeployStatus::Success).then_some(state.sha)
                    })
                })
        },
    );

    result.map_err(|err| {
        if is_deploy_interrupted_error(&err) {
            strip_internal_deploy_error_markers(&err).to_string()
        } else {
            err
        }
    })
}

fn deploy_impl(
    app: &str,
    git_ref: Option<&str>,
    git_sha: Option<&str>,
    force: bool,
    force_fresh_setup_image: bool,
    ctx: &ReconcileIntentContext,
) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let _deploy_log_session = start_deploy_log_session(app);
    eprintln!("-----> Deploying {app}");
    warn_if_upgrade_available();
    let mut target = parse_git_checkout_target(git_ref, git_sha)?;

    let Some(_deploy_lock) = try_acquire_deploy_lock(app)? else {
        if let Some(target) = target.as_ref() {
            if force {
                let request_id = deploy_request_id();
                let request =
                    queue_pending_git_deploy(app, target, true, Some(request_id.clone()))?;
                request_deploy_interrupt(
                    app,
                    &DeployInterruptState {
                        request_id: request_id.clone(),
                        requested_at: now_unix_secs(),
                        target_sha: target.sha.clone(),
                    },
                )?;
                eprintln!(
                    "-----> Deploy already in progress; interrupt requested for forced deploy"
                );
                eprintln!("       Ref: {} ({})", request.ref_name, request.sha);
                return wait_for_forced_pending_deploy_completion(app, target, &request_id);
            }
            queue_pending_git_deploy(app, target, false, None)?;
            eprintln!("-----> Deploy already in progress; replaced pending deploy target");
            eprintln!("       Ref: {} ({})", target.ref_name, target.sha);
            return Ok(());
        }
        return Err(format!(
            "deploy already in progress for '{app}'; retry after it completes"
        ));
    };
    if let Err(err) = clear_deploy_interrupt(app) {
        eprintln!("       Warning: failed to clear pending deploy interrupt state: {err}");
    }
    let _reconcile_lease = acquire_reconcile_lease(app, ctx)?;

    let mut active_force = force;
    loop {
        let result = deploy_once(app, target.as_ref(), active_force, force_fresh_setup_image);
        let pending_request = take_pending_git_request(app)?;
        let interrupted = deploy_result_was_interrupted(&result);
        let Some(pending_request) = pending_request else {
            if interrupted {
                let _ = clear_deploy_interrupt(app);
            }
            return result;
        };
        if !should_process_pending_request(target.as_ref(), &pending_request) {
            return result;
        }

        if interrupted {
            if let Err(err) = clear_deploy_interrupt(app) {
                eprintln!("       Warning: failed to clear deploy interrupt state: {err}");
            }
            eprintln!("-----> Deploy interrupted; handing off to pending target");
        }

        eprintln!("-----> Processing pending deploy target");
        eprintln!(
            "       Ref: {} ({})",
            pending_request.ref_name, pending_request.sha
        );
        if pending_request.force {
            eprintln!("       Forced redeploy requested");
        }
        target = Some(pending_request.target());
        active_force = pending_request.force;
    }
}

pub fn push(app: &str, force: bool) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    install_deploy_interrupt_signal_handlers();
    let source_payload = serde_json::json!({
        "mode": "push",
        "force": force,
    })
    .to_string();
    check_signal_interrupt_without_persist(app, "push scheduling")?;
    reconcile_command::run(
        app,
        ReconcileCommandRequest {
            kind: "push",
            source_kind: "tar-stdin",
            source_payload_json: &source_payload,
            force,
            start_step_name: "push-start",
        },
        || control_plane_snapshot(app),
        |ctx| push_impl(app, force, ctx),
        || {
            read_git_deploy_state(app)
                .ok()
                .flatten()
                .and_then(|state| (state.status == GitDeployStatus::Success).then_some(state.sha))
        },
    )
}

fn push_impl(app: &str, force: bool, ctx: &ReconcileIntentContext) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let _deploy_log_session = start_deploy_log_session(app);
    eprintln!("-----> Deploying {app}");
    warn_if_upgrade_available();
    let Some(_deploy_lock) = try_acquire_deploy_lock(app)? else {
        return Err(format!(
            "deploy already in progress for '{app}'; retry after it completes"
        ));
    };
    let _reconcile_lease = acquire_reconcile_lease(app, ctx)?;

    let code_dir = home_dir().join(app);

    if code_dir.exists() {
        fs::remove_dir_all(&code_dir).map_err(|e| format!("failed to clean code dir: {e}"))?;
    }
    fs::create_dir_all(&code_dir).map_err(|e| format!("failed to create code dir: {e}"))?;

    eprintln!("-----> Receiving code");
    let status = Command::new("tar")
        .args(["xz", "-C"])
        .arg(&code_dir)
        .stdin(std::process::Stdio::inherit())
        .status()
        .map_err(|e| format!("failed to extract tar: {e}"))?;
    if !status.success() {
        return Err("tar extraction failed".to_string());
    }

    let candidate_hash = binary_payload_hash(&code_dir)?;
    if let Some(hash) = candidate_hash.as_deref() {
        if resolve_active_app_ref(app)?.is_some() && read_binary_hash(app).as_deref() == Some(hash)
        {
            if force {
                eprintln!("-----> Binary unchanged ({hash}), forcing deploy");
            } else {
                eprintln!("-----> Binary unchanged ({hash}), skipping deploy");
                return Ok(());
            }
        }
    }

    deploy_from(app, &code_dir, false)?;
    if let Err(err) = clear_git_deploy_state(app) {
        eprintln!("       Warning: failed to clear git deploy state: {err}");
    }
    Ok(())
}

fn deploy_from(app: &str, code_dir: &Path, force_fresh_setup_image: bool) -> Result<(), String> {
    deploy_zero_downtime(app, code_dir, force_fresh_setup_image)
}

const CANDIDATE_READY_TIMEOUT_SECS: u64 = 60;
const DEPLOY_PROGRESS_HEARTBEAT_SECS: u64 = 10;
const APP_PROCESS_EARLY_EXIT_CHECK_GRACE_SECS: u64 = 3;

fn deploy_instance_id() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis()
        .to_string()
}

fn candidate_app_name(app: &str, deploy_id: &str) -> String {
    format!("{app}-build-{deploy_id}")
}

fn failed_candidate_app_name(app: &str, deploy_id: &str) -> String {
    format!("{app}-failed-{deploy_id}")
}

fn spawn_cleanup_previous_worker(app: &str) -> Result<(), String> {
    let exe = current_psht_binary()?;
    Command::new(exe)
        .args(["cleanup", "previous", app])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn background cleanup worker: {e}"))?;
    Ok(())
}

fn queue_previous_cleanup(
    app: &str,
    active_app_ref: &str,
    previous_app_ref: &str,
) -> Result<(), String> {
    let now = now_unix_secs();
    write_cleanup_job(
        app,
        &CleanupJobState {
            app: app.to_string(),
            active_instance_at_schedule: instance_name_from_app_ref(active_app_ref),
            scheduled_previous_instance: instance_name_from_app_ref(previous_app_ref),
            attempts: 0,
            last_error: None,
            scheduled_at: now,
            updated_at: now,
        },
    )
}

fn queue_previous_cleanup_in_background(app: &str, active_app_ref: &str, previous_app_ref: &str) {
    if let Err(err) = queue_previous_cleanup(app, active_app_ref, previous_app_ref) {
        eprintln!("       Warning: failed to schedule background cleanup: {err}");
        return;
    }
    eprintln!("       Cleaning previous active container in background");
    if let Err(err) = spawn_cleanup_previous_worker(app) {
        eprintln!("       Warning: failed to start background cleanup worker: {err}");
    }
}

fn wait_for_tcp_listener(
    app: &str,
    port: u16,
    timeout_secs: u64,
    label: &str,
) -> Result<(), String> {
    let started = Instant::now();
    let mut next_heartbeat = DEPLOY_PROGRESS_HEARTBEAT_SECS;

    loop {
        let output = container::exec_output(
            app,
            &format!(
                "if ss -ltn \"sport = :{port}\" 2>/dev/null | grep -q LISTEN; then echo ready; fi; true"
            ),
        )?;
        if output.trim() == "ready" {
            return Ok(());
        }

        let elapsed = started.elapsed().as_secs();
        if elapsed >= APP_PROCESS_EARLY_EXIT_CHECK_GRACE_SECS && !app_process_is_running(app)? {
            let mut message = format!(
                "{label} failed before TCP :{port} became ready because app process exited"
            );
            if let Ok(command) = read_start_command(app) {
                message.push_str(&format!("\nStart command: {}", command.trim()));
            }
            if let Some(log_excerpt) = app_log_tail(app, APP_LOG_TAIL_LINES) {
                message.push_str("\nLast app log lines:\n");
                message.push_str(&log_excerpt);
            }
            return Err(message);
        }

        if elapsed >= timeout_secs {
            return Err(format!(
                "{label} timed out after {timeout_secs}s waiting for TCP :{port}"
            ));
        }

        if elapsed >= next_heartbeat {
            eprintln!("       Still waiting for TCP :{port} ({elapsed}s elapsed)");
            next_heartbeat += DEPLOY_PROGRESS_HEARTBEAT_SECS;
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn preserve_failed_candidate(app: &str, candidate_app: &str, deploy_id: &str, project: &str) {
    if !container::exists(candidate_app) {
        return;
    }

    let failed_app = failed_candidate_app_name(app, deploy_id);
    if container::exists(&failed_app)
        && let Err(err) = cleanup_container_for_rebuild(&failed_app, project)
    {
        eprintln!("       Warning: failed to clear older failed candidate: {err}");
    }

    let candidate_instance = instance_name_from_app_ref(candidate_app);
    let failed_instance = instance_name_from_app_ref(&failed_app);

    let _ = container::remove_proxy_in_project(&candidate_instance, project);
    let _ = container::exec_cmd(candidate_app, "tailscale down >/dev/null 2>&1 || true");
    let _ = container::exec_cmd(
        candidate_app,
        "systemctl stop tailscaled >/dev/null 2>&1 || true",
    );
    let _ = container::remove_storage_mount(candidate_app);
    let _ = container::remove_tailscale_state_mount(candidate_app);
    if container::is_running(candidate_app).unwrap_or(false) {
        let _ = container::stop(candidate_app);
    }

    match container::rename_instance_in_project(&candidate_instance, &failed_instance, project) {
        Ok(()) => eprintln!("       Preserved failed candidate as {failed_app}"),
        Err(err) => eprintln!("       Warning: failed to preserve failed candidate: {err}"),
    }
}

fn disconnect_active_tailnet_for_cutover(app: &str, active_app: &str) -> Result<(), String> {
    let device_id = read_tailscale_self_snapshot(active_app)
        .ok()
        .and_then(|snapshot| {
            snapshot.device_id.clone().or_else(|| {
                resolve_tailscale_device_id_from_tailnet(&snapshot)
                    .ok()
                    .flatten()
            })
        });

    let _ = container::exec_cmd(active_app, "tailscale down >/dev/null 2>&1 || true");
    let _ = container::exec_cmd(
        active_app,
        "systemctl stop tailscaled >/dev/null 2>&1 || true",
    );

    let Some(device_id) = device_id else {
        return Ok(());
    };

    let token = tailscale::tailnet_access_token()?;
    tailscale::delete_tailnet_device(&token, &device_id)?;
    let _ = sqlite_store::retire_owned_tailscale_device(app, &device_id);
    Ok(())
}

fn restore_active_tailnet_after_rollback(app: &str, active_app: &str, port: u16) {
    eprintln!("       Restoring previous tailnet identity");
    match acquire_exact_tailscale_hostname_for_deploy(active_app, app) {
        Ok(name) => {
            if name.is_some()
                && let Err(err) = tailscale::expose_http_in_container(active_app, port)
            {
                eprintln!("       Warning: failed to re-expose previous tailnet HTTP: {err}");
            }
        }
        Err(err) => {
            eprintln!("       Warning: failed to restore previous tailnet identity: {err}");
        }
    }
}

fn deploy_zero_downtime(
    app: &str,
    code_dir: &Path,
    _force_fresh_setup_image: bool,
) -> Result<(), String> {
    let current_project = current_project_name()?;
    if command_succeeds("incus", &["project", "show", &current_project]) {
        ensure_project_default_profile(&current_project)?;
    }
    init_stacks_in(&stacks_dir())?;
    check_deploy_interrupt(app, "deploy preflight")?;

    eprintln!("-----> Detecting app type");
    let config = detect::detect(code_dir)?;
    eprintln!("       Detected: {:?}", config.app_type);
    let app_env = read_env_vars(app)?;
    ensure_required_env_present(&config.required_env, &app_env)?;
    let binary_hash = if matches!(config.app_type, detect::AppType::Binary) {
        binary_payload_hash(code_dir)?
    } else {
        None
    };
    check_deploy_interrupt(app, "deploy detect and env checks")?;

    if code_dir.join("psht-stack.sh").exists() {
        eprintln!("       Using custom stack");
    }

    let (_stack, script_path) = resolve_stack(app, code_dir, config.stack())?;
    let hash = stack_hash(&script_path)?;
    let apt_fingerprint = apt_packages_fingerprint(&config.apt_packages);
    let setup_hash = setup_hash(&hash, apt_fingerprint.as_deref());
    let skip_tailscale = env::var_os("PSHT_SKIP_TAILSCALE").is_some();
    let port = allocate_port(app);

    eprintln!("-----> Ensuring app storage volume");
    let (storage_pool, storage_volume) = ensure_app_storage_volume(app)?;
    let old_active_app = resolve_active_app_ref(app)?;
    let deploy_id = deploy_instance_id();
    let candidate_app = candidate_app_name(app, &deploy_id);
    let failed_app = failed_candidate_app_name(app, &deploy_id);

    if container::exists(&candidate_app) {
        let _ = cleanup_container_for_rebuild(&candidate_app, &current_project);
    }
    if container::exists(&failed_app) {
        let _ = cleanup_container_for_rebuild(&failed_app, &current_project);
    }

    if let Some(old_active_app) = old_active_app.as_deref() {
        wait_for_container_operation_quiet(old_active_app, &current_project, Some(app))?;
    }

    eprintln!("-----> Preparing candidate container");
    eprintln!("       Candidate: {candidate_app}");
    if old_active_app.is_some() {
        eprintln!("       Traffic remains on current container");
    }

    let build_candidate_result = (|| -> Result<(), String> {
        check_deploy_interrupt(app, "candidate create")?;
        ensure_create_prereqs(&current_project)?;
        eprintln!("-----> Creating candidate container");
        eprintln!("       First run may take a while while Ubuntu image downloads");
        container::create_in_project(&candidate_app, &current_project)?;

        if skip_tailscale {
            eprintln!("-----> Skipping tailscale setup in candidate");
        } else {
            eprintln!("-----> Installing tailscale in candidate");
            check_deploy_interrupt(app, "candidate tailscale install")?;
            tailscale::install_in_container(&candidate_app)?;
        }

        eprintln!("-----> Setting up candidate runtime");
        container::push_file(
            &candidate_app,
            &script_path.to_string_lossy(),
            "/tmp/setup.sh",
        )?;
        run_setup_command_with_logging(
            &candidate_app,
            "chmod +x /tmp/setup.sh && /tmp/setup.sh",
            "candidate runtime setup",
        )?;

        check_deploy_interrupt(app, "candidate package setup")?;
        install_apt_packages(&candidate_app, &config.apt_packages)?;
        container::exec_cmd(
            &candidate_app,
            &format!("echo -n '{setup_hash}' > /etc/psht-setup-hash"),
        )?;

        eprintln!("-----> Attaching storage to candidate container");
        check_deploy_interrupt(app, "candidate storage attach")?;
        container::ensure_storage_mount(&candidate_app, &storage_pool, &storage_volume)?;

        eprintln!("-----> Pushing code to candidate");
        check_deploy_interrupt(app, "candidate code push")?;
        container::push_code(&candidate_app, &code_dir.to_string_lossy())?;
        persist_start_command(&candidate_app, &config.start_command)?;
        persist_required_env(&candidate_app, &config.required_env)?;
        run_hook(
            &candidate_app,
            "preinstall",
            config.preinstall_command.as_deref(),
        )?;

        if let Some(command) = app_workdir_command(&config.install_command) {
            eprintln!("-----> Installing candidate dependencies");
            run_install_command_with_logging(
                &candidate_app,
                &command,
                "candidate dependency install",
            )?;
        }

        run_hook(
            &candidate_app,
            "postinstall",
            config.postinstall_command.as_deref(),
        )?;

        eprintln!("-----> Starting candidate");
        check_deploy_interrupt(app, "candidate app start")?;
        launch_app_process(&candidate_app, port, &config.start_command, &app_env)?;

        eprintln!("-----> Waiting for candidate readiness");
        wait_for_tcp_listener(
            &candidate_app,
            port,
            CANDIDATE_READY_TIMEOUT_SECS,
            "candidate readiness",
        )?;
        Ok(())
    })();

    if let Err(err) = build_candidate_result {
        if is_deploy_interrupted_error(&err) {
            let _ = cleanup_container_for_rebuild(&candidate_app, &current_project);
        } else {
            preserve_failed_candidate(app, &candidate_app, &deploy_id, &current_project);
        }
        return Err(err);
    }

    let mut tailnet_hostname = None;
    let mut candidate_tailnet_ready = false;
    let mut proxy_points_to_candidate = false;
    let mut old_tailnet_disconnected = false;

    if !skip_tailscale {
        if let Some(old_active_app) = old_active_app.as_deref() {
            eprintln!("-----> Preparing tailnet cutover");
            let tailnet_cutover = (|| -> Result<(), String> {
                check_deploy_interrupt(app, "tailnet cutover")?;
                disconnect_active_tailnet_for_cutover(app, old_active_app)?;
                old_tailnet_disconnected = true;
                tailnet_hostname =
                    acquire_exact_tailscale_hostname_for_deploy(&candidate_app, app)?;
                candidate_tailnet_ready = tailnet_hostname.is_some();
                if candidate_tailnet_ready
                    && let Err(err) = tailscale::expose_http_in_container(&candidate_app, port)
                {
                    eprintln!("       Warning: failed to expose tailnet HTTP on :80: {err}");
                }
                Ok(())
            })();

            if let Err(err) = tailnet_cutover {
                if old_tailnet_disconnected {
                    restore_active_tailnet_after_rollback(app, old_active_app, port);
                }
                if is_deploy_interrupted_error(&err) {
                    let _ = cleanup_container_for_rebuild(&candidate_app, &current_project);
                } else {
                    preserve_failed_candidate(app, &candidate_app, &deploy_id, &current_project);
                }
                return Err(err);
            }
        } else {
            eprintln!("-----> Connecting candidate to tailnet");
            check_deploy_interrupt(app, "candidate tailscale connection")?;
            tailnet_hostname = acquire_exact_tailscale_hostname_for_deploy(&candidate_app, app)?;
            candidate_tailnet_ready = tailnet_hostname.is_some();
            if candidate_tailnet_ready
                && let Err(err) = tailscale::expose_http_in_container(&candidate_app, port)
            {
                eprintln!("       Warning: failed to expose tailnet HTTP on :80: {err}");
            }
        }
    }

    let cutover_result = (|| -> Result<(), String> {
        eprintln!("-----> Switching traffic");
        check_deploy_interrupt(app, "proxy cutover")?;
        ensure_proxy_attached_with_recovery(&candidate_app, &current_project, port, port)?;
        proxy_points_to_candidate = true;

        check_deploy_interrupt(app, "runtime state cutover")?;
        write_app_runtime_state(app, &candidate_app, old_active_app.as_deref())?;

        if let Some(old_active_app) = old_active_app.as_deref() {
            eprintln!("       Stopping previous app process");
            if let Err(err) = stop_app_process_on_port(old_active_app, port) {
                eprintln!("       Warning: failed to stop previous app process: {err}");
            }
        }

        Ok(())
    })();

    if let Err(err) = cutover_result {
        if let Some(old_active_app) = old_active_app.as_deref() {
            let _ =
                ensure_proxy_attached_with_recovery(old_active_app, &current_project, port, port);
            if candidate_tailnet_ready {
                let _ =
                    container::exec_cmd(&candidate_app, "tailscale down >/dev/null 2>&1 || true");
                let _ = container::exec_cmd(
                    &candidate_app,
                    "systemctl stop tailscaled >/dev/null 2>&1 || true",
                );
            }
            if old_tailnet_disconnected || candidate_tailnet_ready {
                restore_active_tailnet_after_rollback(app, old_active_app, port);
            }
        } else if proxy_points_to_candidate {
            let candidate_instance = instance_name_from_app_ref(&candidate_app);
            let _ = container::remove_proxy_in_project(&candidate_instance, &current_project);
        }

        if is_deploy_interrupted_error(&err) {
            let _ = cleanup_container_for_rebuild(&candidate_app, &current_project);
            return Err(err);
        }

        preserve_failed_candidate(app, &candidate_app, &deploy_id, &current_project);
        if old_active_app.is_some() {
            return Err(format!(
                "deploy cutover failed and rollback was applied: {err}"
            ));
        }
        return Err(err);
    }

    if let Some(old_active_app) = old_active_app.as_deref() {
        queue_previous_cleanup_in_background(app, &candidate_app, old_active_app);
    }

    let build_number = increment_build_number(app)?;

    if let Some(name) = tailnet_hostname {
        eprintln!("       Tailnet: http://{name} (also http://{name}:{port})");
    }

    if let Some(hash) = binary_hash {
        if let Err(e) = write_binary_hash(app, &hash) {
            eprintln!("       Warning: failed to persist binary hash: {e}");
        }
    } else if let Err(e) = clear_binary_hash(app) {
        eprintln!("       Warning: failed to clear binary hash: {e}");
    }

    eprintln!("-----> Verifying live endpoint");
    wait_for_tcp_listener(
        &candidate_app,
        port,
        CANDIDATE_READY_TIMEOUT_SECS,
        "post-cutover verification",
    )?;

    eprintln!("=====> App {app} deployed on port {port} (build {build_number})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_test_app(prefix: &str) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}-{}-{now}", std::process::id())
    }

    fn main_target(sha: &str) -> GitCheckoutTarget {
        GitCheckoutTarget {
            ref_name: "refs/heads/main".to_string(),
            sha: sha.to_string(),
        }
    }

    fn sqlite_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn candidate_app_name_uses_build_suffix() {
        assert_eq!(candidate_app_name("demo", "42"), "demo-build-42");
    }

    #[test]
    fn failed_candidate_app_name_uses_failed_suffix() {
        assert_eq!(failed_candidate_app_name("demo", "42"), "demo-failed-42");
    }

    #[test]
    fn previous_cleanup_target_allows_canonical_previous_instance() {
        assert!(previous_cleanup_target_allowed("demo", "demo"));
    }

    #[test]
    fn previous_cleanup_target_allows_transient_previous_instance() {
        assert!(previous_cleanup_target_allowed("demo", "demo-build-42"));
        assert!(previous_cleanup_target_allowed("demo", "demo-prev-42"));
        assert!(previous_cleanup_target_allowed("demo", "demo-failed-42"));
        assert!(!previous_cleanup_target_allowed("demo", "other-build-42"));
    }

    #[test]
    fn forced_takeover_prep_clears_owned_interrupt_and_pending_request() {
        let _guard = sqlite_test_guard();
        let app = unique_test_app("takeover-owned");
        let request_id = "req-123";
        let target = main_target("deadbeef");
        let pending = PendingGitDeployRequest::from_target(
            &target,
            true,
            Some(request_id.to_string()),
            Some(123),
        );
        write_pending_git_request(&app, &pending).unwrap();
        request_deploy_interrupt(
            &app,
            &DeployInterruptState {
                request_id: request_id.to_string(),
                requested_at: 123,
                target_sha: target.sha.clone(),
            },
        )
        .unwrap();

        let prep = prepare_for_forced_deploy_takeover(&app, &target, request_id).unwrap();

        assert_eq!(
            prep,
            ForcedDeployTakeoverPrep {
                cleared_interrupt: true,
                cleared_pending_request: true,
            }
        );
        assert!(read_deploy_interrupt(&app).unwrap().is_none());
        assert!(read_pending_git_request(&app).unwrap().is_none());
    }

    #[test]
    fn forced_takeover_prep_preserves_foreign_interrupt_and_pending_request() {
        let _guard = sqlite_test_guard();
        let app = unique_test_app("takeover-foreign");
        let target = main_target("deadbeef");
        let pending = PendingGitDeployRequest::from_target(
            &target,
            true,
            Some("req-999".to_string()),
            Some(456),
        );
        write_pending_git_request(&app, &pending).unwrap();
        request_deploy_interrupt(
            &app,
            &DeployInterruptState {
                request_id: "req-999".to_string(),
                requested_at: 456,
                target_sha: target.sha.clone(),
            },
        )
        .unwrap();

        let prep = prepare_for_forced_deploy_takeover(&app, &target, "req-123").unwrap();

        assert_eq!(
            prep,
            ForcedDeployTakeoverPrep {
                cleared_interrupt: false,
                cleared_pending_request: false,
            }
        );
        assert_eq!(
            read_deploy_interrupt(&app).unwrap().unwrap(),
            DeployInterruptState {
                request_id: "req-999".to_string(),
                requested_at: 456,
                target_sha: target.sha.clone(),
            }
        );
        assert_eq!(read_pending_git_request(&app).unwrap().unwrap(), pending);
    }

    #[test]
    fn forced_takeover_prep_claims_legacy_force_request_without_request_id() {
        let _guard = sqlite_test_guard();
        let app = unique_test_app("takeover-legacy");
        let request_id = "req-123";
        let target = main_target("deadbeef");
        let pending = PendingGitDeployRequest::from_target(&target, true, None, None);
        write_pending_git_request(&app, &pending).unwrap();
        request_deploy_interrupt(
            &app,
            &DeployInterruptState {
                request_id: request_id.to_string(),
                requested_at: 123,
                target_sha: target.sha.clone(),
            },
        )
        .unwrap();

        let prep = prepare_for_forced_deploy_takeover(&app, &target, request_id).unwrap();

        assert_eq!(
            prep,
            ForcedDeployTakeoverPrep {
                cleared_interrupt: true,
                cleared_pending_request: true,
            }
        );
        assert!(read_deploy_interrupt(&app).unwrap().is_none());
        assert!(read_pending_git_request(&app).unwrap().is_none());
    }
}
