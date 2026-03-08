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
    if !is_transient_deploy_app_for(app, &scheduled_previous_app) {
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
            let _ = clear_deploy_interrupt(app);
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
            let _ = take_pending_git_request(app)?;
            let result = deploy_once(app, Some(target), true, false);
            let _ = clear_deploy_interrupt(app);
            return result;
        }

        if !lock_active
            && interrupt_is_ours
            && let Some(_lock) = try_acquire_deploy_lock(app)?
        {
            eprintln!(
                "-----> Active deploy did not acknowledge interrupt; taking over forced deploy"
            );
            if pending_is_ours {
                let _ = take_pending_git_request(app)?;
            }
            let result = deploy_once(app, Some(target), true, false);
            let _ = clear_deploy_interrupt(app);
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
                    read_git_deploy_state(app)
                        .ok()
                        .flatten()
                        .and_then(|state| {
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
    deploy_from_in_place(app, code_dir, force_fresh_setup_image)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InPlaceSetupAction {
    CreateFresh,
    ReconfigureExisting,
    ReuseExisting,
}

fn select_in_place_setup_action(
    container_exists: bool,
    remote_hash: &str,
    setup_hash: &str,
    force_fresh_setup_image: bool,
) -> InPlaceSetupAction {
    if !container_exists || force_fresh_setup_image {
        InPlaceSetupAction::CreateFresh
    } else if remote_hash == setup_hash {
        InPlaceSetupAction::ReuseExisting
    } else {
        InPlaceSetupAction::ReconfigureExisting
    }
}

fn deploy_from_in_place(
    app: &str,
    code_dir: &Path,
    force_fresh_setup_image: bool,
) -> Result<(), String> {
    let current_uid = run_cmd_capture("id", &["-u"])?;
    let current_project = format!("user-{}", current_uid.trim());
    if command_succeeds("incus", &["project", "show", &current_project]) {
        ensure_project_default_profile(&current_project)?;
    }
    init_stacks_in(&stacks_dir())?;
    check_deploy_interrupt(app, "in-place preflight")?;

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
    check_deploy_interrupt(app, "in-place detect and env checks")?;

    if code_dir.join("psht-stack.sh").exists() {
        eprintln!("       Using custom stack");
    }

    let (_stack, script_path) = resolve_stack(app, code_dir, config.stack())?;
    let hash = stack_hash(&script_path)?;
    let apt_fingerprint = apt_packages_fingerprint(&config.apt_packages);
    let setup_hash = setup_hash(&hash, apt_fingerprint.as_deref());
    let skip_tailscale = env::var_os("PSHT_SKIP_TAILSCALE").is_some();
    eprintln!("-----> Ensuring app storage volume");
    let (storage_pool, storage_volume) = ensure_app_storage_volume(app)?;
    let tailscale_volume = if skip_tailscale {
        None
    } else {
        eprintln!("-----> Ensuring tailscale state volume");
        Some(ensure_app_tailscale_volume(app)?)
    };

    let port = allocate_port(app);
    let mut tailnet_hostname = if skip_tailscale {
        None
    } else {
        tailscale::dns_name_in_container(app)
    };
    let container_exists = container::exists(app);
    let mut remote_hash = String::new();
    if container_exists {
        write_app_runtime_state_in_project(app, app, None, &current_project)?;
        check_deploy_interrupt(app, "in-place setup inspection")?;
        wait_for_container_operation_quiet(app, &current_project, Some(app))?;
        if !force_fresh_setup_image {
            if !container::is_running(app)? {
                eprintln!("-----> Starting existing container for setup inspection");
                container::start(app)?;
            }
            remote_hash = container::exec_output(app, "cat /etc/psht-setup-hash 2>/dev/null")
                .unwrap_or_default()
                .trim()
                .to_string();
        }
    }
    let setup_action = select_in_place_setup_action(
        container_exists,
        &remote_hash,
        &setup_hash,
        force_fresh_setup_image,
    );
    let needs_setup = !matches!(setup_action, InPlaceSetupAction::ReuseExisting);

    match setup_action {
        InPlaceSetupAction::CreateFresh => {
            if container_exists {
                eprintln!("-----> Fresh container requested; rebuilding container");
                cleanup_container_for_rebuild(app, &current_project)?;
            }
        }
        InPlaceSetupAction::ReconfigureExisting => {
            eprintln!("-----> Reconfiguring existing container");
            stop_app_process_on_port(app, port)?;
        }
        InPlaceSetupAction::ReuseExisting => {
            eprintln!("-----> Reusing container");
            stop_app_process_on_port(app, port)?;
        }
    };

    check_deploy_interrupt(app, "in-place setup evaluation")?;
    if needs_setup {
        check_deploy_interrupt(app, "in-place setup start")?;
        if matches!(setup_action, InPlaceSetupAction::CreateFresh) {
            wait_for_container_operation_quiet(app, &current_project, Some(app))?;
            eprintln!("-----> Creating container");
            eprintln!("       First run may take a while while Ubuntu image downloads");
            ensure_create_prereqs(&current_project)?;
            container::create_in_project(app, &current_project)?;
            write_app_runtime_state_in_project(app, app, None, &current_project)?;

            if skip_tailscale {
                eprintln!("-----> Skipping tailscale setup");
            } else {
                eprintln!("-----> Installing tailscale");
                tailscale::install_in_container(app)?;
            }
        } else if skip_tailscale {
            eprintln!("-----> Skipping tailscale setup");
        } else {
            eprintln!("-----> Ensuring tailscale is available");
            tailscale::install_in_container(app)?;
        }

        eprintln!("-----> Setting up runtime");
        container::push_file(app, &script_path.to_string_lossy(), "/tmp/setup.sh")?;
        run_setup_command_with_logging(
            app,
            "chmod +x /tmp/setup.sh && /tmp/setup.sh",
            "in-place runtime setup",
        )?;

        check_deploy_interrupt(app, "in-place package setup")?;
        install_apt_packages(app, &config.apt_packages)?;

        container::exec_cmd(
            app,
            &format!("echo -n '{setup_hash}' > /etc/psht-setup-hash"),
        )?;

        if skip_tailscale {
            eprintln!("-----> Skipping tailnet connection");
        } else {
            if let Some((tailscale_pool, tailscale_state_volume)) = tailscale_volume.as_ref() {
                container::ensure_tailscale_state_mount(
                    app,
                    tailscale_pool,
                    tailscale_state_volume,
                )?;
            }
            eprintln!("-----> Connecting to tailnet");
            check_deploy_interrupt(app, "in-place tailscale connection")?;
            tailnet_hostname = acquire_exact_tailscale_hostname_for_deploy(app, app)?;
        }

        let port = allocate_port(app);
        eprintln!("-----> Setting up port forwarding on :{port}");
        ensure_proxy_attached_with_recovery(app, &current_project, port, port)?;
    }

    check_deploy_interrupt(app, "in-place storage attach")?;
    container::ensure_storage_mount(app, &storage_pool, &storage_volume)?;

    eprintln!("-----> Pushing code to container");
    check_deploy_interrupt(app, "in-place code push")?;
    container::push_code(app, &code_dir.to_string_lossy())?;
    persist_start_command(app, &config.start_command)?;
    persist_required_env(app, &config.required_env)?;
    run_hook(app, "preinstall", config.preinstall_command.as_deref())?;

    if let Some(command) = app_workdir_command(&config.install_command) {
        eprintln!("-----> Installing dependencies");
        run_install_command_with_logging(app, &command, "dependency install")?;
    }

    run_hook(app, "postinstall", config.postinstall_command.as_deref())?;

    eprintln!("-----> Starting app");
    check_deploy_interrupt(app, "in-place app start")?;
    launch_app_process(app, port, &config.start_command, &app_env)?;

    if !skip_tailscale {
        tailnet_hostname = tailnet_hostname.or_else(|| tailscale::dns_name_in_container(app));
    }
    if !skip_tailscale && tailnet_hostname.is_some() {
        if let Err(e) = tailscale::expose_http_in_container(app, port) {
            eprintln!("       Warning: failed to expose tailnet HTTP on :80: {e}");
        }
    }

    check_deploy_interrupt(app, "in-place finalize")?;
    write_app_runtime_state(app, app, None)?;

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

    eprintln!("=====> App {app} deployed on port {port} (build {build_number})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_in_place_setup_action_prefers_create_when_missing() {
        assert_eq!(
            select_in_place_setup_action(false, "", "abc", false),
            InPlaceSetupAction::CreateFresh
        );
    }

    #[test]
    fn select_in_place_setup_action_prefers_create_when_forced() {
        assert_eq!(
            select_in_place_setup_action(true, "abc", "abc", true),
            InPlaceSetupAction::CreateFresh
        );
    }

    #[test]
    fn select_in_place_setup_action_reuses_matching_hash() {
        assert_eq!(
            select_in_place_setup_action(true, "abc", "abc", false),
            InPlaceSetupAction::ReuseExisting
        );
    }

    #[test]
    fn select_in_place_setup_action_reconfigures_hash_mismatch() {
        assert_eq!(
            select_in_place_setup_action(true, "", "abc", false),
            InPlaceSetupAction::ReconfigureExisting
        );
    }
}
