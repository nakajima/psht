use super::*;

pub fn ps() -> Result<(), String> {
    let rows = ps_rows()?;
    if rows.is_empty() {
        println!("No apps running.");
        return Ok(());
    }
    println!("{:<20} {:<10}", "APP", "STATUS");
    for row in rows {
        println!("{:<20} {:<10}", row.app, row.status);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PsRow {
    pub(super) app: String,
    pub(super) active_app: Option<String>,
    pub(super) container_status: String,
    pub(super) status: String,
}

pub(super) fn ps_rows() -> Result<Vec<PsRow>, String> {
    let containers = container::list()?;
    let apps = app_targets_from_runtime_state(&containers)?;
    let mut rows = Vec::with_capacity(apps.len());
    for (app, active_app, container_status) in apps {
        let container_state = ps_container_state(&container_status);
        let service_ready = match container_state {
            PsContainerState::Running => match active_app.as_deref() {
                Some(active_app) => {
                    let port = allocate_port(&app);
                    match probe_app_service(active_app, port) {
                        Ok(probe) => Some(probe.is_ready()),
                        Err(err) => {
                            eprintln!(
                                "       Warning: failed to check app service for {app}: {err}"
                            );
                            Some(false)
                        }
                    }
                }
                None => None,
            },
            PsContainerState::Stopped | PsContainerState::Missing => None,
        };
        let status = ps_status_from_parts(container_state, service_ready).to_string();
        rows.push(PsRow {
            app,
            active_app,
            container_status,
            status,
        });
    }
    Ok(rows)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PsContainerState {
    Running,
    Stopped,
    Missing,
}

pub(super) fn ps_container_state(status: &str) -> PsContainerState {
    if status.eq_ignore_ascii_case("missing") {
        PsContainerState::Missing
    } else if status.eq_ignore_ascii_case("running") {
        PsContainerState::Running
    } else {
        PsContainerState::Stopped
    }
}

pub(super) fn ps_status_from_parts(
    container_state: PsContainerState,
    process_running: Option<bool>,
) -> &'static str {
    match container_state {
        PsContainerState::Missing => "Missing",
        PsContainerState::Stopped => "Stopped",
        PsContainerState::Running => {
            if process_running == Some(true) {
                "Running"
            } else {
                "Down"
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AppHealthReport {
    pub(super) app: String,
    pub(super) healthy: bool,
    pub(super) details: Vec<String>,
}

pub(super) fn is_transient_deploy_app_name(app: &str) -> bool {
    app_name::is_transient_deploy_app_name(app)
}

pub(super) fn canonical_app_name_from_container(container_name: &str) -> Option<String> {
    let app = container_name.strip_prefix("psht-")?;
    if app.is_empty() || is_transient_deploy_app_name(app) {
        return None;
    }
    Some(app.to_string())
}

pub(super) fn app_targets_from_runtime_state(
    containers: &[container::ContainerInfo],
) -> Result<Vec<(String, Option<String>, String)>, String> {
    let mut status_by_name = BTreeMap::new();
    for container in containers {
        status_by_name.insert(container.name.clone(), container.status.clone());
    }

    let mut targets: BTreeMap<String, (Option<String>, String)> = BTreeMap::new();
    for (app, state) in read_managed_app_runtime_states()? {
        let mut active_app_ref = app_ref_from_instance_name(&state.active_instance);
        if active_app_ref.is_none() {
            active_app_ref = resolve_active_app_ref(&app)?;
        }

        if let Some(active_app) = active_app_ref.as_deref() {
            let active_instance = instance_name_from_app_ref(active_app);
            if let Some(status) = status_by_name.get(&active_instance) {
                targets.insert(app, (Some(active_app.to_string()), status.clone()));
                continue;
            }
        }

        if let Some(active_app) = resolve_active_app_ref(&app)? {
            let active_instance = instance_name_from_app_ref(&active_app);
            if let Some(status) = status_by_name.get(&active_instance) {
                targets.insert(app, (Some(active_app), status.clone()));
                continue;
            }
        }

        targets.insert(app, (None, "Missing".to_string()));
    }

    for container in containers {
        let Some(app) = canonical_app_name_from_container(&container.name) else {
            continue;
        };
        targets
            .entry(app.clone())
            .or_insert((Some(app), container.status.clone()));
    }

    Ok(targets
        .into_iter()
        .map(|(app, (active_app, status))| (app, active_app, status))
        .collect())
}

pub(super) fn check_app_health(
    app: &str,
    active_app: &str,
    container_status: &str,
) -> AppHealthReport {
    let mut details = Vec::new();
    let mut healthy = true;
    let active_instance = instance_name_from_app_ref(active_app);
    details.push(format!("active instance: {active_instance}"));

    if container_status.eq_ignore_ascii_case("running") {
        details.push("container running".to_string());
    } else {
        details.push(format!("container status is {container_status}"));
        return AppHealthReport {
            app: app.to_string(),
            healthy: false,
            details,
        };
    }

    let port = allocate_port(app);
    match probe_app_service(active_app, port) {
        Ok(probe) => {
            if probe.is_active() {
                details.push("app service active".to_string());
            } else {
                healthy = false;
                details.push(format!("app service not ready: {}", probe.detail(port)));
            }

            if probe.port_listening && probe.listener_matches_service {
                details.push(format!("tcp :{port} listening"));
            } else if probe.port_listening {
                healthy = false;
                details.push(format!(
                    "tcp :{port} listener is not owned by psht-app.service"
                ));
            } else {
                healthy = false;
                details.push(format!("tcp :{port} is not listening"));
            }
        }
        Err(err) => {
            healthy = false;
            details.push(format!("failed to check app service: {err}"));
        }
    }

    match read_start_command(active_app) {
        Ok(command) => details.push(format!("start command: {}", command.trim())),
        Err(err) => {
            healthy = false;
            details.push(err);
        }
    }

    match read_required_env(active_app) {
        Ok(required_env) => match read_env_vars(app) {
            Ok(vars) => {
                if let Err(err) = ensure_required_env_present(&required_env, &vars) {
                    healthy = false;
                    details.push(err);
                } else if required_env.is_empty() {
                    details.push("required env: none".to_string());
                } else {
                    details.push(format!(
                        "required env present ({})",
                        required_env.join(", ")
                    ));
                }
            }
            Err(err) => {
                healthy = false;
                details.push(format!("failed to read env vars: {err}"));
            }
        },
        Err(err) => {
            healthy = false;
            details.push(format!("required env metadata error: {err}"));
        }
    }

    if let Some(detail) = cleanup_pending_detail(app) {
        details.push(detail);
    }

    AppHealthReport {
        app: app.to_string(),
        healthy,
        details,
    }
}

pub(super) fn should_delegate_health_to_psht(
    uid: &str,
    already_delegated: bool,
    psht_user_exists: bool,
) -> bool {
    uid.trim() == "0" && !already_delegated && psht_user_exists
}

fn delegate_health_to_psht_user() -> Result<(), String> {
    let exe =
        env::current_exe().map_err(|e| format!("failed to resolve current executable: {e}"))?;
    let status = Command::new("sudo")
        .args(["-u", "psht", "-H"])
        .arg(exe)
        .arg("health")
        .env(HEALTH_DELEGATED_ENV, "1")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| format!("failed to run delegated health check: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("delegated health check failed".to_string())
    }
}

pub fn health() -> Result<(), String> {
    let health_started = Instant::now();
    let uid = run_cmd_capture("id", &["-u"]).unwrap_or_else(|_| "?".to_string());
    if should_delegate_health_to_psht(
        &uid,
        env::var_os(HEALTH_DELEGATED_ENV).is_some(),
        command_succeeds("id", &["psht"]),
    ) {
        return delegate_health_to_psht_user();
    }

    eprintln!("-----> Checking app health");
    let containers = container::list()?;
    let apps = app_targets_from_runtime_state(&containers)?;
    let app_count = apps.len();
    if apps.is_empty() {
        println!("No deployed apps found.");
        stats::report_health_summary(stats::HealthSummary {
            app_count: 0,
            unhealthy_count: 0,
            duration: health_started.elapsed(),
        });
        return Ok(());
    }

    println!("{:<20} {:<10} DETAILS", "APP", "STATUS");
    let mut unhealthy = Vec::new();

    for (app, active_app, status) in apps {
        let report = if let Some(active_app) = active_app.as_deref() {
            check_app_health(&app, active_app, &status)
        } else {
            AppHealthReport {
                app: app.clone(),
                healthy: false,
                details: vec!["active container missing".to_string()],
            }
        };
        let health_status = if report.healthy { "ok" } else { "unhealthy" };
        println!(
            "{:<20} {:<10} {}",
            report.app,
            health_status,
            report.details.join("; ")
        );
        if !report.healthy {
            unhealthy.push(report.app);
        }
    }

    let unhealthy_count = unhealthy.len();
    stats::report_health_summary(stats::HealthSummary {
        app_count,
        unhealthy_count,
        duration: health_started.elapsed(),
    });
    if unhealthy_count == 0 {
        eprintln!("=====> All app containers are healthy");
        return Ok(());
    }

    Err(format!(
        "{} app(s) unhealthy: {}",
        unhealthy_count,
        unhealthy.join(", ")
    ))
}

pub(super) fn normalize_candidate_app_ref(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(app_ref) = app_ref_from_instance_name(trimmed) {
        return Some(app_ref);
    }
    Some(trimmed.trim_start_matches("psht-").to_string())
}

pub fn debug_resources(app: Option<&str>, candidate: Option<&str>) -> Result<(), String> {
    if let Some(app) = app {
        app_name::validate_app_name(app)?;
    }
    let project = current_project_name().ok();
    let mut candidate_app = candidate.and_then(normalize_candidate_app_ref);

    if candidate_app.is_none()
        && let Some(app) = app
    {
        if let Some(status) = sqlite_store::get_app_status(app)? {
            for instance in [
                status.candidate_instance,
                status.active_instance,
                status.previous_instance,
            ]
            .into_iter()
            .flatten()
            {
                if let Some(app_ref) = normalize_candidate_app_ref(&instance) {
                    candidate_app = Some(app_ref);
                    break;
                }
            }
        }
        if candidate_app.is_none() {
            candidate_app = resolve_active_app_ref(app)?;
        }
    }

    let snapshot = collect_resource_diagnostics(project.as_deref(), candidate_app.as_deref());
    println!("{snapshot}");
    Ok(())
}

pub fn logs(app: &str, follow: bool) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let deploy_lines = deploy_log::recent_entries(
        app,
        LOGS_DEPLOY_HISTORY_FILES,
        LOGS_DEPLOY_HISTORY_LINES_PER_FILE,
    )?;
    if !deploy_lines.is_empty() {
        for line in &deploy_lines {
            println!("{line}");
        }
    }

    let active_app = resolve_active_app_ref(app)?;
    if follow {
        let Some(active_app) = active_app else {
            if deploy_lines.is_empty() {
                return Err(format!("app '{app}' not found"));
            }
            return Ok(());
        };
        return container::logs(&active_app, true);
    }

    let Some(active_app) = active_app else {
        if deploy_lines.is_empty() {
            return Err(format!("app '{app}' not found"));
        }
        return Ok(());
    };

    let app_log = container::exec_output(
        &active_app,
        &format!("if [ -f {APP_PROCESS_LOG_PATH} ]; then cat {APP_PROCESS_LOG_PATH}; fi"),
    )
    .map_err(|e| format!("failed to read app log from '{active_app}': {e}"))?;
    for line in app_log.lines() {
        if line.trim().is_empty() {
            continue;
        }
        println!("{} [app] {}", deploy_log::timestamp_now(), line);
    }
    Ok(())
}

fn doctor_check(label: &str, ok: bool, failed: &mut bool) {
    if ok {
        println!("  [ok] {label}");
    } else {
        println!("  [FAIL] {label}");
        *failed = true;
    }
}

pub fn doctor() -> Result<(), String> {
    let expected_version = env!("CARGO_PKG_VERSION");
    let mut failed = false;
    let psht_home = psht_user_home_dir();

    println!("Installation:");
    let psht_user_shell = psht_user_shell_path();
    doctor_check(
        "psht user shell path exists",
        psht_user_shell
            .as_ref()
            .map(|path| path.is_file())
            .unwrap_or(false),
        &mut failed,
    );
    let psht_cli_path = psht_home.join("bin/psht");
    doctor_check(
        "$PSHT_HOME/bin/psht executable",
        psht_cli_path.is_file() && path_is_world_executable(&psht_cli_path).unwrap_or(false),
        &mut failed,
    );
    if let Some(shell) = psht_user_shell.as_ref() {
        doctor_check(
            &format!("psht version {expected_version}"),
            binary_matches_version(shell, expected_version),
            &mut failed,
        );
    } else {
        doctor_check(
            &format!("psht version {expected_version}"),
            false,
            &mut failed,
        );
    }

    println!();
    println!("System:");
    doctor_check(
        "psht user exists",
        command_succeeds("id", &["psht"]),
        &mut failed,
    );
    if let Some(shell) = psht_user_shell.as_ref() {
        let shell_s = shell.to_string_lossy().to_string();
        let shell_ok = run_cmd_capture("getent", &["passwd", "psht"])
            .ok()
            .map(|line| line.trim_end().ends_with(&format!(":{shell_s}")))
            .unwrap_or(false);
        doctor_check(
            &format!("psht user shell is {shell_s}"),
            shell_ok,
            &mut failed,
        );
        let in_etc_shells = fs::read_to_string("/etc/shells")
            .ok()
            .map(|contents| contents.lines().any(|line| line.trim() == shell_s))
            .unwrap_or(false);
        doctor_check(
            &format!("{shell_s} listed in /etc/shells"),
            in_etc_shells,
            &mut failed,
        );
    } else {
        doctor_check("psht user shell configured", false, &mut failed);
        doctor_check("psht user shell listed in /etc/shells", false, &mut failed);
    }
    let in_incus_group = run_cmd_capture("id", &["-nG", "psht"])
        .ok()
        .map(|groups| groups.split_whitespace().any(|group| group == "incus"))
        .unwrap_or(false);
    doctor_check("psht user in incus group", in_incus_group, &mut failed);

    println!();
    println!("Incus:");
    doctor_check("incus installed", command_exists("incus"), &mut failed);
    doctor_check(
        "incus responsive",
        command_succeeds("incus", &["info"]),
        &mut failed,
    );

    if env::var_os("PSHT_SKIP_TAILSCALE").is_none() {
        println!();
        println!("Tailscale:");
        doctor_check(
            "tailscale installed",
            command_exists("tailscale"),
            &mut failed,
        );
        doctor_check(
            "tailscale connected",
            command_succeeds("tailscale", &["status"]),
            &mut failed,
        );
        doctor_check(
            "tailscale SSH enabled",
            tailscale_ssh_enabled().unwrap_or(false),
            &mut failed,
        );
        let oauth_config = psht_home.join(".config/tailscale-oauth");
        let oauth_exists = oauth_config.is_file();
        doctor_check(
            "$PSHT_HOME/.config/tailscale-oauth exists",
            oauth_exists,
            &mut failed,
        );
        if oauth_exists {
            let oauth_check = check_tailscale_oauth_permissions(&oauth_config);
            let token_ok = oauth_check.token_error.is_none();
            doctor_check("tailscale OAuth token fetch", token_ok, &mut failed);
            if let Some(err) = oauth_check.token_error.as_deref() {
                println!("         reason: {err}");
                println!("         fix: {TAILSCALE_OAUTH_SCOPE_HINT}");
            }

            let read_ok = token_ok && oauth_check.devices_read_error.is_none();
            doctor_check("tailscale device api read permission", read_ok, &mut failed);
            if !read_ok {
                let reason = oauth_check
                    .devices_read_error
                    .as_deref()
                    .or(oauth_check.token_error.as_deref())
                    .unwrap_or("unknown");
                println!("         reason: {reason}");
                println!("         fix: {TAILSCALE_OAUTH_SCOPE_HINT}");
            }

            let write_ok = token_ok && oauth_check.devices_write_error.is_none();
            doctor_check(
                "tailscale device api write permission",
                write_ok,
                &mut failed,
            );
            if !write_ok {
                let reason = oauth_check
                    .devices_write_error
                    .as_deref()
                    .or(oauth_check.token_error.as_deref())
                    .unwrap_or("unknown");
                println!("         reason: {reason}");
                println!("         fix: {TAILSCALE_OAUTH_SCOPE_HINT}");
            }
        } else {
            doctor_check("tailscale OAuth token fetch", false, &mut failed);
            doctor_check("tailscale device api read permission", false, &mut failed);
            doctor_check("tailscale device api write permission", false, &mut failed);
            println!("         reason: tailscale OAuth config is missing");
            println!("         fix: {TAILSCALE_OAUTH_SCOPE_HINT}");
            println!("         configure at: {TAILSCALE_OAUTH_SETTINGS_URL}");
        }
    }

    println!();
    println!("Directories & stacks:");
    let repos = psht_home.join("repos");
    let builds = psht_home.join("builds");
    let stacks = psht_home.join("stacks");
    doctor_check("$PSHT_HOME/repos exists", repos.is_dir(), &mut failed);
    doctor_check("$PSHT_HOME/builds exists", builds.is_dir(), &mut failed);
    doctor_check("$PSHT_HOME/stacks exists", stacks.is_dir(), &mut failed);
    let stacks_populated = stacks
        .read_dir()
        .ok()
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .any(|path| path.extension().and_then(|ext| ext.to_str()) == Some("sh"))
        })
        .unwrap_or(false);
    doctor_check("stacks populated", stacks_populated, &mut failed);

    println!();
    if failed {
        println!("Some checks failed.");
        Err("doctor checks failed".to_string())
    } else {
        println!("All checks passed.");
        Ok(())
    }
}
