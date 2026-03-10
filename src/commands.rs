use std::collections::{BTreeMap, BTreeSet, HashMap, hash_map::DefaultHasher};
use std::env;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::app_name;
use crate::app_state;
use crate::container;
use crate::control_plane::{
    AppPhase, AppRuntimeState, DesiredState, ReconcileIntentContext, RuntimeSnapshot,
};
use crate::deploy_log;
use crate::deploy_state::{
    CleanupJobState, DeployInterruptState, DeployLockMetadata, GitCheckoutTarget, GitDeployState,
    GitDeployStatus, PendingGitDeployRequest,
};
use crate::detect;
use crate::git::GIT_LOCAL_ENV_VARS;
use crate::reconcile_command::{self, ReconcileCommandRequest};
use crate::reconcile_runtime;
use crate::runtime_graph;
use crate::sqlite_store;
use crate::stats;
use crate::tailscale;

const STACKS: &[(&str, &str)] = &[
    ("binary", include_str!("../stacks/binary.sh")),
    ("bun", include_str!("../stacks/bun.sh")),
    ("go", include_str!("../stacks/go.sh")),
    ("node", include_str!("../stacks/node.sh")),
    ("python", include_str!("../stacks/python.sh")),
    ("rust", include_str!("../stacks/rust.sh")),
    ("static", include_str!("../stacks/static.sh")),
];

const DEFAULT_FORGE_URL: &str = "https://github.com/nakajima/psht";
const START_COMMAND_PATH: &str = "/etc/psht-start-command";
const REQUIRED_ENV_PATH: &str = "/etc/psht-required-env";
const SSH_LOGIN_ENV_PATH: &str = "/etc/profile.d/psht-env.sh";
const APP_SERVICE_NAME: &str = "psht-app.service";
const APP_SERVICE_UNIT_PATH: &str = "/etc/systemd/system/psht-app.service";
const APP_SERVICE_RUNNER_PATH: &str = "/usr/local/bin/psht-app-runner";
const APP_PROCESS_PID_PATH: &str = "/var/psht/app.pid";
const APP_PROCESS_LOG_PATH: &str = "/var/psht/app.log";
const INSTALL_LOG_PATH: &str = "/var/psht/install.log";
const SETUP_LOG_PATH: &str = "/var/psht/setup.log";
const APP_PROCESS_POLL_SLEEP: &str = "0.2";
const APP_PROCESS_STOP_TERM_CHECKS: u32 = 40;
const APP_PROCESS_STOP_KILL_CHECKS: u32 = 10;
const APP_PROCESS_START_WAIT_CHECKS: u32 = 25;
const APP_LOG_TAIL_LINES: u32 = 40;
const INSTALL_LOG_TAIL_LINES: u32 = 80;
const SETUP_LOG_TAIL_LINES: u32 = 120;
const APT_INSTALL_MAX_ATTEMPTS: u32 = 3;
const APT_INSTALL_RETRY_SLEEP_SECS: u32 = 2;
const CONTAINER_OP_INITIAL_WAIT_CHECKS: u32 = 6;
const CONTAINER_OP_RECHECK_WAIT_CHECKS: u32 = 6;
const CONTAINER_OP_WAIT_SLEEP_MS: u64 = 500;
const CONTAINER_OP_HEARTBEAT_SECS: u64 = 5;
const CONTAINER_DELETE_RETRY_CHECKS: u32 = 20;
const DEPLOY_LOCK_STALE_SECS: u64 = 6 * 60 * 60;
const UPGRADE_CHECK_TTL_SECS: u64 = 6 * 60 * 60;
const TAILSCALE_ONLINE_WAIT_SECS: u64 = 20;
const TAILSCALE_ONLINE_WAIT_POLL_MS: u64 = 500;
const TAILSCALE_EXACT_HOSTNAME_TOTAL_TIMEOUT_SECS: u64 = 120;
const TAILSCALE_OAUTH_SETTINGS_URL: &str = "https://login.tailscale.com/admin/settings/oauth";
const TAILSCALE_OAUTH_SCOPE_HINT: &str =
    "Keys: Write (tag:psht), Devices: Core Read, Devices: Core Write";
const TAILSCALE_CLEANUP_PERMISSION_DENIED_MARKER: &str = "__tailscale_cleanup_permission_denied__";
const DEPLOY_INTERRUPT_WAIT_POLL_MS: u64 = 1000;
const DEPLOY_INTERRUPT_WAIT_HEARTBEAT_SECS: u64 = 5;
const DEPLOY_FORCE_TAKEOVER_TIMEOUT_SECS: u64 = 30;
const DEPLOY_FORCE_KILL_WAIT_MS: u64 = 150;
const DEPLOY_FORCE_KILL_WAIT_CHECKS: u32 = 8;
const DEPLOY_INTERRUPT_ERR_PREFIX: &str = "deploy interrupted";
const HEALTH_DELEGATED_ENV: &str = "PSHT_HEALTH_DELEGATED";
const LOGS_DEPLOY_HISTORY_FILES: usize = 12;
const LOGS_DEPLOY_HISTORY_LINES_PER_FILE: usize = 400;
const TAILSCALE_STATE_SEED_PATH: &str = "/var/lib/psht-tailscale-state";
const INCUS_METADATA_TIMEOUT_SECS: u64 = 20;
const TAILSCALE_HOSTNAME_ACQUIRE_RETRY_SLEEP_MS: u64 = 1000;
const RECONCILE_LEASE_TTL_SECS: u64 = 30;
const RECONCILE_LEASE_HEARTBEAT_SECS: u64 = 5;
const TAKEOVER_RETRY_MS: u64 = 1000;
const TAKEOVER_MAX_CANCEL_PER_CYCLE: usize = 20;
const BUSY_OP_POLICY_ENV: &str = "PSHT_BUSY_OP_POLICY";
const BLOCKED_OP_BUDGET_SECS: u64 = 15;
const RESOURCE_DIAGNOSTIC_TIMEOUT_SECS: u64 = 8;
const DEPLOY_ERR_CACHED_SETUP_FAILURE_MARKER: &str = "__psht_cached_setup_failure__:";
const DEPLOY_ERR_FRESH_SETUP_FAILURE_MARKER: &str = "__psht_fresh_setup_failure__:";
const DEPLOY_ERR_SETUP_TRANSIENT_MARKER: &str = "__psht_setup_transient_failure__:";
const DEPLOY_ERR_SETUP_NONRETRYABLE_MARKER: &str = "__psht_setup_nonretryable_failure__:";
const SUPERVISE_SERVICE_PATH: &str = "/etc/systemd/system/psht-supervise.service";
const LEGACY_SUPERVISE_TIMER_PATH: &str = "/etc/systemd/system/psht-supervise.timer";
const WEB_SERVICE_NAME: &str = "psht-web.service";
const WEB_SERVICE_PATH: &str = "/etc/systemd/system/psht-web.service";
const SUPERVISE_DAEMON_LOCK_APP: &str = "__supervise-daemon__";
const SUPERVISE_DAEMON_INTERVAL_SECS: u64 = 30;
const SUPERVISE_DAEMON_ERROR_BACKOFF_SECS: u64 = 5;
const DESIRED_STATE_RUNNING: &str = "running";
const DESIRED_STATE_STOPPED: &str = "stopped";

macro_rules! eprintln {
    () => {
        std::eprintln!()
    };
    ($($arg:tt)*) => {{
        let rendered = format!($($arg)*);
        std::eprintln!("{}", rendered);
        deploy_log::append("deploy", &rendered);
    }};
}

mod admin_commands;
mod deploy_commands;
mod lifecycle_commands;
mod observability_commands;
mod web_ui;

use self::deploy_commands::{
    check_deploy_interrupt, control_plane_snapshot, is_transient_deploy_app_for,
};

#[cfg(test)]
use self::admin_commands::{
    cli_update_manifest, cli_update_manifest_json, install_binary_atomically,
    join_tailscale_for_repair_with_fallback, setup_script, supervise_service_unit_content,
    update_script, web_service_unit_content, write_oauth_config,
};
#[cfg(test)]
use self::deploy_commands::{
    deploy_interrupted_error, is_deploy_interrupted_error, pending_force_request_is_ours,
    should_process_pending_request,
};
#[cfg(test)]
use self::observability_commands::{
    PsContainerState, canonical_app_name_from_container, check_app_health, has_deploy_suffix,
    is_transient_deploy_app_name, normalize_candidate_app_ref, ps_container_state,
    ps_status_from_parts, should_delegate_health_to_psht,
};

fn home_dir() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/home/psht".to_string()))
}

fn psht_user_home_dir() -> PathBuf {
    run_cmd_capture("getent", &["passwd", "psht"])
        .ok()
        .and_then(|line| {
            line.split(':')
                .nth(5)
                .map(str::trim)
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from("/home/psht"))
}

fn builds_dir() -> PathBuf {
    home_dir().join("builds")
}

fn env_dir() -> PathBuf {
    home_dir().join(".psht").join("env")
}

fn env_path_in(dir: &Path, app: &str) -> PathBuf {
    dir.join(format!("{app}.toml"))
}

fn env_path(app: &str) -> PathBuf {
    env_path_in(&env_dir(), app)
}

fn repos_dir() -> PathBuf {
    home_dir().join("repos")
}

fn stacks_dir() -> PathBuf {
    home_dir().join("stacks")
}

fn upgrade_check_state_path() -> PathBuf {
    home_dir().join(".psht").join("upgrade-check.toml")
}

fn deploy_lock_dir() -> PathBuf {
    home_dir().join("deploy-locks")
}

fn cleanup_lock_dir() -> PathBuf {
    home_dir().join(".psht").join("cleanup-locks")
}

fn build_numbers_dir() -> PathBuf {
    home_dir().join("build-numbers")
}

fn build_number_path_in(dir: &Path, app: &str) -> PathBuf {
    dir.join(format!("{app}.build"))
}

fn read_build_number_from(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn increment_build_number_in(dir: &Path, app: &str) -> Result<u64, String> {
    let path = build_number_path_in(dir, app);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let next = read_build_number_from(&path).saturating_add(1);
    fs::write(&path, format!("{next}\n"))
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(next)
}

fn increment_build_number(app: &str) -> Result<u64, String> {
    increment_build_number_in(&build_numbers_dir(), app)
}

fn binary_hashes_dir() -> PathBuf {
    home_dir().join("binary-hashes")
}

fn binary_hash_path_in(dir: &Path, app: &str) -> PathBuf {
    dir.join(format!("{app}.hash"))
}

fn binary_hash_path(app: &str) -> PathBuf {
    binary_hash_path_in(&binary_hashes_dir(), app)
}

fn read_binary_hash_from(path: &Path) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    Some(value.to_string())
}

fn read_binary_hash(app: &str) -> Option<String> {
    read_binary_hash_from(&binary_hash_path(app))
}

fn write_binary_hash_to(path: &Path, hash: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    fs::write(path, format!("{hash}\n"))
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(())
}

fn write_binary_hash(app: &str, hash: &str) -> Result<(), String> {
    write_binary_hash_to(&binary_hash_path(app), hash)
}

fn clear_binary_hash(app: &str) -> Result<(), String> {
    let path = binary_hash_path(app);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to remove {}: {e}", path.display())),
    }
}

fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn parse_env_assignment(raw: &str) -> Result<(String, String), String> {
    let (name, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("invalid env assignment '{raw}'; expected NAME=value"))?;
    if !is_valid_env_name(name) {
        return Err(format!("invalid env name '{name}'"));
    }
    Ok((name.to_string(), value.to_string()))
}

fn parse_env_name(raw: &str) -> Result<String, String> {
    if !is_valid_env_name(raw) {
        return Err(format!("invalid env name '{raw}'"));
    }
    Ok(raw.to_string())
}

fn read_env_vars_from(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
    };

    let raw: BTreeMap<String, String> =
        toml::from_str(&content).map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
    let mut vars = BTreeMap::new();
    for (name, value) in raw {
        if !is_valid_env_name(&name) {
            return Err(format!("invalid env name '{}' in {}", name, path.display()));
        }
        vars.insert(name, value);
    }
    Ok(vars)
}

fn read_env_vars(app: &str) -> Result<BTreeMap<String, String>, String> {
    read_env_vars_from(&env_path(app))
}

fn write_env_vars_to(path: &Path, vars: &BTreeMap<String, String>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let content = toml::to_string_pretty(vars)
        .map_err(|e| format!("failed to serialize {}: {e}", path.display()))?;
    fs::write(path, content).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

fn write_env_vars(app: &str, vars: &BTreeMap<String, String>) -> Result<(), String> {
    write_env_vars_to(&env_path(app), vars)
}

fn remove_env_vars(app: &str) -> Result<(), String> {
    let path = env_path(app);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to remove {}: {e}", path.display())),
    }
}

fn parse_required_env_metadata(contents: &str) -> Result<Vec<String>, String> {
    let mut required = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !is_valid_env_name(trimmed) {
            return Err(format!(
                "invalid required env name '{trimmed}' in {REQUIRED_ENV_PATH}"
            ));
        }
        if required.iter().any(|name| name == trimmed) {
            continue;
        }
        required.push(trimmed.to_string());
    }
    Ok(required)
}

fn read_required_env(app: &str) -> Result<Vec<String>, String> {
    let raw = container::exec_output(app, &format!("cat {REQUIRED_ENV_PATH} 2>/dev/null || true"))?;
    parse_required_env_metadata(&raw)
}

fn write_required_env_cmd(required_env: &[String]) -> Result<String, String> {
    let mut names = Vec::new();
    for name in required_env {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !is_valid_env_name(trimmed) {
            return Err(format!("invalid required env name '{trimmed}'"));
        }
        if names.iter().any(|v| v == trimmed) {
            continue;
        }
        names.push(trimmed.to_string());
    }
    let content = if names.is_empty() {
        String::new()
    } else {
        format!("{}\n", names.join("\n"))
    };
    let escaped = shell_quote(&content);
    Ok(format!(
        "mkdir -p /etc && printf '%s' {escaped} > {REQUIRED_ENV_PATH}"
    ))
}

fn persist_required_env(app: &str, required_env: &[String]) -> Result<(), String> {
    let cmd = write_required_env_cmd(required_env)?;
    container::exec_cmd(app, &cmd)
}

fn ssh_login_env_content(vars: &BTreeMap<String, String>) -> Result<String, String> {
    let mut lines = vec![
        "# Generated by psht for interactive SSH login shells.".to_string(),
        "# Do not edit manually.".to_string(),
    ];
    for (name, value) in vars {
        if !is_valid_env_name(name) {
            return Err(format!("invalid env name '{name}'"));
        }
        lines.push(format!("export {name}={}", shell_quote(value)));
    }
    Ok(format!("{}\n", lines.join("\n")))
}

fn write_ssh_login_env_cmd(vars: &BTreeMap<String, String>) -> Result<String, String> {
    let content = ssh_login_env_content(vars)?;
    let escaped = shell_quote(&content);
    Ok(format!(
        "mkdir -p /etc/profile.d && printf '%s' {escaped} > {SSH_LOGIN_ENV_PATH} && chmod 644 {SSH_LOGIN_ENV_PATH}"
    ))
}

fn persist_ssh_login_env(app: &str, vars: &BTreeMap<String, String>) -> Result<(), String> {
    let cmd = write_ssh_login_env_cmd(vars)?;
    container::exec_cmd(app, &cmd)
}

fn ensure_required_env_present(
    required_env: &[String],
    vars: &BTreeMap<String, String>,
) -> Result<(), String> {
    let mut missing = Vec::new();
    for name in required_env {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !is_valid_env_name(trimmed) {
            return Err(format!("invalid required env name '{trimmed}'"));
        }
        if !vars.contains_key(trimmed) {
            missing.push(trimmed.to_string());
        }
    }
    if missing.is_empty() {
        return Ok(());
    }

    missing.sort();
    Err(format!(
        "missing required env vars: {}. Set them with `psht env NAME=value`.",
        missing.join(", ")
    ))
}

fn binary_payload_hash(code_dir: &Path) -> Result<Option<String>, String> {
    let marker_path = code_dir.join(".psht-start-command");
    if !marker_path.is_file() {
        return Ok(None);
    }

    let marker = fs::read_to_string(&marker_path)
        .map_err(|e| format!("failed to read {}: {e}", marker_path.display()))?;
    let marker = marker.trim();
    if marker.is_empty() {
        return Err(".psht-start-command is empty".to_string());
    }

    let binary_token = marker
        .split_whitespace()
        .next()
        .ok_or_else(|| ".psht-start-command is empty".to_string())?;

    // Only hash local relative binaries we can inspect.
    if binary_token.starts_with('/') || binary_token.contains("..") {
        return Ok(None);
    }
    let rel = binary_token.strip_prefix("./").unwrap_or(binary_token);
    if rel.is_empty() {
        return Ok(None);
    }

    let binary_path = code_dir.join(rel);
    if !binary_path.is_file() {
        return Ok(None);
    }

    let mut hasher = DefaultHasher::new();
    marker.hash(&mut hasher);
    let bytes = fs::read(&binary_path)
        .map_err(|e| format!("failed to read {}: {e}", binary_path.display()))?;
    bytes.hash(&mut hasher);
    Ok(Some(format!("{:016x}", hasher.finish())))
}

fn command_exists(name: &str) -> bool {
    env::var_os("PATH")
        .map(|path| {
            env::split_paths(&path).any(|dir| {
                let candidate = dir.join(name);
                candidate
                    .metadata()
                    .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn command_succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn pretty_cmd(program: &str, args: &[&str]) -> String {
    if args.is_empty() {
        program.to_string()
    } else {
        format!("{program} {}", args.join(" "))
    }
}

fn output_with_timeout(
    program: &str,
    args: &[&str],
    timeout_secs: u64,
) -> Result<(std::process::Output, bool), String> {
    let use_timeout = command_exists("timeout");
    if use_timeout {
        let timeout_arg = format!("{timeout_secs}s");
        let output = Command::new("timeout")
            .arg("--kill-after=2s")
            .arg(&timeout_arg)
            .arg(program)
            .args(args)
            .output()
            .map_err(|e| format!("failed to run timeout-wrapped {program}: {e}"))?;
        let code = output.status.code().unwrap_or_default();
        let timed_out = code == 124 || code == 137;
        return Ok((output, timed_out));
    }

    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    Ok((output, false))
}

fn run_cmd_capture_with_timeout(
    program: &str,
    args: &[&str],
    timeout_secs: u64,
) -> Result<String, String> {
    let pretty = pretty_cmd(program, args);
    let (output, timed_out) = output_with_timeout(program, args, timeout_secs)?;
    if timed_out {
        return Err(format!("command timed out after {timeout_secs}s: {pretty}"));
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            return Err(format!("command failed: {pretty}"));
        }
        return Err(format!("command failed: {pretty}: {stderr}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn command_succeeds_with_timeout(
    program: &str,
    args: &[&str],
    timeout_secs: u64,
) -> Result<bool, String> {
    let pretty = pretty_cmd(program, args);
    let (output, timed_out) = output_with_timeout(program, args, timeout_secs)?;
    if timed_out {
        return Err(format!("command timed out after {timeout_secs}s: {pretty}"));
    }
    Ok(output.status.success())
}

fn run_incus_metadata_capture(args: &[&str]) -> Result<String, String> {
    run_cmd_capture_with_timeout("incus", args, INCUS_METADATA_TIMEOUT_SECS)
}

fn incus_metadata_command_succeeds(args: &[&str]) -> Result<bool, String> {
    command_succeeds_with_timeout("incus", args, INCUS_METADATA_TIMEOUT_SECS)
}

fn run_cmd(program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    if !status.success() {
        let pretty = if args.is_empty() {
            program.to_string()
        } else {
            format!("{program} {}", args.join(" "))
        };
        return Err(format!("command failed: {pretty}"));
    }
    Ok(())
}

fn scrub_git_local_env(cmd: &mut Command) {
    for name in GIT_LOCAL_ENV_VARS {
        cmd.env_remove(name);
    }
}

fn git_command() -> Command {
    let mut cmd = Command::new("git");
    scrub_git_local_env(&mut cmd);
    cmd
}

fn run_git_in(args: &[&str], cwd: &Path) -> Result<(), String> {
    let status = git_command()
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !status.success() {
        let pretty = if args.is_empty() {
            "git".to_string()
        } else {
            format!("git {}", args.join(" "))
        };
        return Err(format!("command failed: {pretty}"));
    }
    Ok(())
}

fn run_cmd_capture(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let pretty = if args.is_empty() {
            program.to_string()
        } else {
            format!("{program} {}", args.join(" "))
        };
        if stderr.is_empty() {
            return Err(format!("command failed: {pretty}"));
        }
        return Err(format!("command failed: {pretty}: {stderr}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_git_capture_in(args: &[&str], cwd: &Path) -> Result<String, String> {
    let output = git_command()
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let pretty = if args.is_empty() {
            "git".to_string()
        } else {
            format!("git {}", args.join(" "))
        };
        if stderr.is_empty() {
            return Err(format!("command failed: {pretty}"));
        }
        return Err(format!("command failed: {pretty}: {stderr}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_cmd_quiet(program: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }

    let pretty = if args.is_empty() {
        program.to_string()
    } else {
        format!("{program} {}", args.join(" "))
    };
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        return Err(format!("command failed: {pretty}"));
    }
    Err(format!("command failed: {pretty}: {stderr}"))
}

fn parse_version_codename(os_release: &str) -> Option<String> {
    os_release.lines().find_map(|line| {
        line.strip_prefix("VERSION_CODENAME=")
            .map(|value| value.trim_matches('"').to_string())
            .filter(|value| !value.is_empty())
    })
}

fn os_release_codename() -> Result<String, String> {
    let contents = fs::read_to_string("/etc/os-release")
        .map_err(|e| format!("failed to read /etc/os-release: {e}"))?;
    parse_version_codename(&contents)
        .ok_or_else(|| "VERSION_CODENAME missing in /etc/os-release".to_string())
}

fn ensure_line_in_file(path: &Path, line: &str) -> Result<(), String> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == line) {
        return Ok(());
    }

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("failed to open {}: {e}", path.display()))?;

    if !existing.is_empty() && !existing.ends_with('\n') {
        file.write_all(b"\n")
            .map_err(|e| format!("failed to update {}: {e}", path.display()))?;
    }
    writeln!(file, "{line}").map_err(|e| format!("failed to update {}: {e}", path.display()))?;
    Ok(())
}

fn prompt_tty(prompt: &str) -> Result<String, String> {
    let mut tty = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|e| format!("failed to open /dev/tty: {e}"))?;
    tty.write_all(prompt.as_bytes())
        .map_err(|e| format!("failed to write prompt: {e}"))?;
    tty.flush()
        .map_err(|e| format!("failed to flush prompt: {e}"))?;

    let mut input = String::new();
    let mut reader = BufReader::new(tty);
    reader
        .read_line(&mut input)
        .map_err(|e| format!("failed to read from /dev/tty: {e}"))?;
    Ok(input.trim().to_string())
}

fn parse_tailscale_dns_name(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let name = value.pointer("/Self/DNSName")?.as_str()?;
    let trimmed = name.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TailscaleSelfSnapshot {
    device_id: Option<String>,
    hostname_label: Option<String>,
    dns_name: Option<String>,
    backend_state: String,
    online: bool,
    health: Vec<String>,
    ips: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OwnedTailscaleCleanupResult {
    removed_device_ids: Vec<String>,
    retired_device_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TailscaleJoinAttempt {
    dns_name: Option<String>,
    cleanup_lookup_error: Option<String>,
}

impl TailscaleJoinAttempt {
    fn from_dns_name(dns_name: Option<String>) -> Self {
        Self {
            dns_name,
            cleanup_lookup_error: None,
        }
    }
}

fn value_as_nonempty_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn tailscale_dns_label(dns_name: &str) -> Option<&str> {
    let label = dns_name.trim_end_matches('.').split('.').next()?;
    if label.is_empty() { None } else { Some(label) }
}

fn tailscale_hostname_is_exact(dns_name: &str, app: &str) -> bool {
    tailscale_dns_label(dns_name)
        .map(|label| label.eq_ignore_ascii_case(app))
        .unwrap_or(false)
}

fn parse_tailscale_self_snapshot(json: &str) -> Result<TailscaleSelfSnapshot, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("failed to parse tailscale status: {e}"))?;
    let self_value = value
        .get("Self")
        .ok_or_else(|| "missing Self in tailscale status".to_string())?;
    let serde_json::Value::Object(self_obj) = self_value else {
        return Err("invalid Self in tailscale status".to_string());
    };

    let dns_name = self_obj
        .get("DNSName")
        .and_then(value_as_nonempty_string)
        .map(|name| name.trim_end_matches('.').to_string());
    let hostname_label = dns_name
        .as_deref()
        .and_then(tailscale_dns_label)
        .map(|label| label.to_string())
        .or_else(|| self_obj.get("HostName").and_then(value_as_nonempty_string));
    let device_id = self_obj
        .get("ID")
        .and_then(value_as_nonempty_string)
        .or_else(|| self_obj.get("NodeID").and_then(value_as_nonempty_string))
        .or_else(|| self_obj.get("StableID").and_then(value_as_nonempty_string));
    let backend_state = value
        .get("BackendState")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let online = self_obj
        .get("Online")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let health = value
        .get("Health")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .filter(|entry| !entry.trim().is_empty())
                .map(|entry| entry.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let ips = self_obj
        .get("TailscaleIPs")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|ip| !ip.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(TailscaleSelfSnapshot {
        device_id,
        hostname_label,
        dns_name,
        backend_state,
        online,
        health,
        ips,
    })
}

fn read_tailscale_self_snapshot(container_app: &str) -> Result<TailscaleSelfSnapshot, String> {
    let status_json = container::exec_output(container_app, "tailscale status --json")
        .map_err(|e| format!("failed to read tailscale status from {container_app}: {e}"))?;
    parse_tailscale_self_snapshot(&status_json)
}

fn tailscale_conflict_label_for_app(label: &str, app: &str) -> bool {
    if label.eq_ignore_ascii_case(app) {
        return true;
    }

    let app_lower = app.to_ascii_lowercase();
    let label_lower = label.to_ascii_lowercase();
    let Some(suffix) = label_lower.strip_prefix(&format!("{app_lower}-")) else {
        return false;
    };
    !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit())
}

fn tailnet_device_label(device: &tailscale::TailnetDevice) -> Option<String> {
    if let Some(hostname) = device
        .hostname_label
        .as_deref()
        .map(str::trim)
        .filter(|hostname| !hostname.is_empty())
    {
        return Some(hostname.to_string());
    }
    device
        .dns_name
        .as_deref()
        .and_then(tailscale_dns_label)
        .map(str::to_string)
}

fn resolve_tailscale_device_id_from_tailnet(
    snapshot: &TailscaleSelfSnapshot,
) -> Result<Option<String>, String> {
    let has_matching_fields = snapshot
        .dns_name
        .as_deref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
        || snapshot
            .hostname_label
            .as_deref()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
    if !has_matching_fields {
        return Ok(None);
    }

    let token = tailscale::tailnet_access_token()?;
    let devices = tailscale::list_tailnet_devices(&token)?;
    let mut matches = Vec::new();
    for device in devices {
        let dns_matches = snapshot
            .dns_name
            .as_deref()
            .zip(device.dns_name.as_deref())
            .map(|(lhs, rhs)| lhs.eq_ignore_ascii_case(rhs))
            .unwrap_or(false);
        let label_matches = snapshot
            .hostname_label
            .as_deref()
            .zip(tailnet_device_label(&device).as_deref())
            .map(|(lhs, rhs)| lhs.eq_ignore_ascii_case(rhs))
            .unwrap_or(false);
        if dns_matches || label_matches {
            matches.push(device.id);
        }
    }
    matches.sort();
    matches.dedup();

    if matches.len() > 1 {
        return Err(format!(
            "unable to resolve tailscale self device id uniquely; matched {}",
            matches.join(", ")
        ));
    }
    Ok(matches.into_iter().next())
}

fn track_owned_tailscale_device(
    app: &str,
    container_app: &str,
    created_via: &str,
) -> Result<Option<String>, String> {
    let snapshot = read_tailscale_self_snapshot(container_app)?;
    if snapshot
        .dns_name
        .as_deref()
        .map(str::trim)
        .map(|dns| dns.is_empty())
        .unwrap_or(true)
    {
        return Ok(None);
    }

    let mut device_id = snapshot.device_id.clone();
    if device_id.is_none() {
        device_id = resolve_tailscale_device_id_from_tailnet(&snapshot)?;
    }
    let device_id = device_id.ok_or_else(|| {
        format!(
            "tailscale self device id unavailable for {container_app} (dns: {}, state: {}, online: {})",
            snapshot.dns_name.as_deref().unwrap_or("unknown"),
            snapshot.backend_state,
            snapshot.online
        )
    })?;

    sqlite_store::upsert_owned_tailscale_device(
        app,
        &device_id,
        snapshot.hostname_label.as_deref(),
        snapshot.dns_name.as_deref(),
        created_via,
        Some(&instance_name_from_app_ref(container_app)),
    )?;
    Ok(snapshot.dns_name)
}

fn cleanup_owned_tailscale_hostname_conflicts(
    app: &str,
    current_device_id: Option<&str>,
) -> Result<OwnedTailscaleCleanupResult, String> {
    let tracked = sqlite_store::list_active_owned_tailscale_devices(app)?;

    let token = tailscale::tailnet_access_token()?;
    let devices = tailscale::list_tailnet_devices(&token)?;
    let mut devices_by_id = BTreeMap::new();
    for device in devices {
        devices_by_id.insert(device.id.clone(), device);
    }

    let tracked_ids = tracked
        .iter()
        .map(|row| row.device_id.clone())
        .collect::<BTreeSet<_>>();
    let mut untracked_exact = Vec::new();
    for device in devices_by_id.values() {
        let is_tagged_psht = device.tags.iter().any(|tag| tag == "tag:psht");
        if !is_tagged_psht {
            continue;
        }
        let Some(label) = tailnet_device_label(device) else {
            continue;
        };
        if !label.eq_ignore_ascii_case(app) {
            continue;
        }
        if tracked_ids.contains(&device.id) {
            continue;
        }
        if current_device_id == Some(device.id.as_str()) {
            continue;
        }
        let display_name = device
            .dns_name
            .as_deref()
            .unwrap_or(label.as_str())
            .to_string();
        untracked_exact.push(format!("{} ({display_name})", device.id));
    }
    if !untracked_exact.is_empty() {
        untracked_exact.sort();
        untracked_exact.dedup();
        return Err(format!(
            "exact hostname '{app}' is held by untracked tailscale device(s): {}",
            untracked_exact.join(", ")
        ));
    }

    let mut result = OwnedTailscaleCleanupResult::default();
    let allow_exact_delete = current_device_id.is_some();

    for row in tracked {
        if current_device_id == Some(row.device_id.as_str()) {
            continue;
        }

        let Some(device) = devices_by_id.get(&row.device_id) else {
            sqlite_store::retire_owned_tailscale_device(app, &row.device_id)?;
            result.retired_device_ids.push(row.device_id.clone());
            continue;
        };

        if !device.tags.iter().any(|tag| tag == "tag:psht") {
            continue;
        }

        let label = tailnet_device_label(device)
            .or_else(|| row.hostname_label.clone())
            .unwrap_or_default();
        if label.is_empty() || !tailscale_conflict_label_for_app(&label, app) {
            continue;
        }
        if !allow_exact_delete && label.eq_ignore_ascii_case(app) {
            continue;
        }

        tailscale::delete_tailnet_device(&token, &row.device_id)?;
        sqlite_store::retire_owned_tailscale_device(app, &row.device_id)?;
        result.removed_device_ids.push(row.device_id.clone());
    }

    Ok(result)
}

fn tailscale_self_health_from_json(json: &str) -> Result<(String, bool, Vec<String>), String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("failed to parse tailscale status: {e}"))?;
    let backend_state = value
        .get("BackendState")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let online = value
        .pointer("/Self/Online")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let health = value
        .get("Health")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    Ok((backend_state, online, health))
}

fn tailscale_api_permission_denied(err: &str) -> bool {
    let lowered = err.to_ascii_lowercase();
    lowered.contains("http 403")
        || lowered.contains("not have enough permissions")
        || lowered.contains("not enough permissions")
        || lowered.contains("permission denied")
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct TailscaleOauthPermissionCheck {
    token_error: Option<String>,
    devices_read_error: Option<String>,
    devices_write_error: Option<String>,
}

impl TailscaleOauthPermissionCheck {
    fn all_ok(&self) -> bool {
        self.token_error.is_none()
            && self.devices_read_error.is_none()
            && self.devices_write_error.is_none()
    }
}

fn check_tailscale_oauth_permissions(oauth_config_path: &Path) -> TailscaleOauthPermissionCheck {
    let mut check = TailscaleOauthPermissionCheck::default();
    let token = match tailscale::tailnet_access_token_from_path(oauth_config_path) {
        Ok(token) => token,
        Err(err) => {
            check.token_error = Some(format!("failed to acquire tailnet OAuth token: {err}"));
            return check;
        }
    };

    if let Err(err) = tailscale::list_tailnet_devices(&token) {
        check.devices_read_error = Some(err);
    }
    match tailscale::can_delete_tailnet_devices(&token) {
        Ok(true) => {}
        Ok(false) => {
            check.devices_write_error =
                Some("tailnet OAuth actor does not have permission to delete devices".to_string());
        }
        Err(err) => check.devices_write_error = Some(err),
    }

    check
}

fn format_tailscale_oauth_permission_failure(check: &TailscaleOauthPermissionCheck) -> String {
    let mut lines = Vec::new();
    if let Some(err) = check.token_error.as_deref() {
        lines.push(format!("OAuth token: {err}"));
    }
    if let Some(err) = check.devices_read_error.as_deref() {
        lines.push(format!("Devices read: {err}"));
    }
    if let Some(err) = check.devices_write_error.as_deref() {
        lines.push(format!("Devices write: {err}"));
    }
    if lines.is_empty() {
        lines.push("unknown OAuth permission validation failure".to_string());
    }

    format!(
        "tailscale OAuth credential is missing required permissions or failed validation:\n       {}\n       Required OAuth scopes: {TAILSCALE_OAUTH_SCOPE_HINT}\n       Configure at: {TAILSCALE_OAUTH_SETTINGS_URL}",
        lines.join("\n       ")
    )
}

fn wait_for_tailscale_online(
    app: &str,
    timeout: Duration,
) -> Result<(String, bool, Vec<String>), String> {
    let start = Instant::now();
    let mut last_snapshot: Option<(String, bool, Vec<String>)> = None;

    loop {
        if start.elapsed() >= timeout {
            if let Some((last_state, last_online, last_health)) = last_snapshot {
                let mut detail = format!("State: {last_state}\nOnline: {last_online}");
                if !last_health.is_empty() {
                    detail.push_str(&format!("\nHealth: {}", last_health.join(" | ")));
                }
                return Err(format!(
                    "timed out waiting for tailscale to become healthy:\n{detail}"
                ));
            }
            return Err("timed out waiting for tailscale status".to_string());
        }

        match container::exec_output(app, "tailscale status --json") {
            Ok(status_json) => match tailscale_self_health_from_json(&status_json) {
                Ok((backend_state, online, health)) => {
                    let healthy = backend_state == "Running" && online;
                    last_snapshot = Some((backend_state.clone(), online, health.clone()));
                    if healthy {
                        return Ok((backend_state, online, health));
                    }
                }
                Err(e) => {
                    last_snapshot = Some((format!("error: {e}"), false, Vec::new()));
                }
            },
            Err(e) => {
                last_snapshot = Some((format!("error: {e}"), false, Vec::new()));
            }
        }

        thread::sleep(Duration::from_millis(TAILSCALE_ONLINE_WAIT_POLL_MS));
    }
}

fn reset_tailscale_for_retry(container_app: &str) -> Result<(), String> {
    container::exec_cmd(container_app, "tailscale down >/dev/null 2>&1 || true")?;
    container::exec_cmd(
        container_app,
        "systemctl stop tailscaled >/dev/null 2>&1 || true",
    )
}

fn retry_attempt_budget(timeout_secs: u64, retry_sleep: Duration) -> u64 {
    let sleep_ms = retry_sleep.as_millis().max(1);
    let timeout_ms = u128::from(timeout_secs).saturating_mul(1000);
    let attempts = timeout_ms / sleep_ms;
    let attempts = attempts.max(1).saturating_add(1);
    attempts.min(u128::from(u64::MAX)) as u64
}

fn acquire_exact_tailscale_hostname_with_retry<
    FStateJoin,
    FAuthJoin,
    FWaitHealthy,
    FReset,
    FSleep,
>(
    container_app: &str,
    app: &str,
    mut join_with_state: FStateJoin,
    mut join_with_auth_key: FAuthJoin,
    mut wait_healthy: FWaitHealthy,
    mut reset_for_retry: FReset,
    mut sleep: FSleep,
) -> Result<Option<String>, String>
where
    FStateJoin: FnMut(&str, &str) -> Result<TailscaleJoinAttempt, String>,
    FAuthJoin: FnMut(&str, &str) -> Result<TailscaleJoinAttempt, String>,
    FWaitHealthy: FnMut(&str) -> Result<(), String>,
    FReset: FnMut(&str) -> Result<(), String>,
    FSleep: FnMut(Duration),
{
    let started = Instant::now();
    let retry_sleep = Duration::from_millis(TAILSCALE_HOSTNAME_ACQUIRE_RETRY_SLEEP_MS);
    let total_timeout = Duration::from_secs(TAILSCALE_EXACT_HOSTNAME_TOTAL_TIMEOUT_SECS);
    let total_attempt_budget =
        retry_attempt_budget(TAILSCALE_EXACT_HOSTNAME_TOTAL_TIMEOUT_SECS, retry_sleep);
    let mut attempt: u64 = 0;
    let mut in_auth_phase = false;
    let mut last_observation = "no hostname observation yet".to_string();
    let mut last_logged_cleanup_lookup_error: Option<String> = None;

    loop {
        let elapsed = started.elapsed();
        if elapsed >= total_timeout || attempt >= total_attempt_budget {
            break;
        }
        attempt = attempt.saturating_add(1);

        eprintln!(
            "       Acquiring exact tailscale hostname (attempt {attempt}, {}s elapsed)",
            elapsed.as_secs()
        );

        let used_auth_phase = in_auth_phase;
        let join_attempt = if !used_auth_phase {
            match join_with_state(container_app, app) {
                Ok(attempt) => attempt,
                Err(state_err) => {
                    last_observation = format!("state join failed: {state_err}");
                    eprintln!("       State-based tailscale join failed: {state_err}");
                    in_auth_phase = true;
                    eprintln!(
                        "       State-based tailscale recovery failed; switching to auth-key recovery"
                    );
                    if let Err(reset_err) = reset_for_retry(container_app) {
                        eprintln!(
                            "       Warning: failed to reset tailscale before retry: {reset_err}"
                        );
                    }
                    sleep(retry_sleep);
                    continue;
                }
            }
        } else {
            match join_with_auth_key(container_app, app) {
                Ok(attempt) => attempt,
                Err(auth_err) => {
                    last_observation = format!("auth-key join failed: {auth_err}");
                    eprintln!("       Auth-key tailscale join failed: {auth_err}");
                    if let Err(reset_err) = reset_for_retry(container_app) {
                        eprintln!(
                            "       Warning: failed to reset tailscale before retry: {reset_err}"
                        );
                    }
                    sleep(retry_sleep);
                    continue;
                }
            }
        };
        let name = join_attempt.dns_name;
        let active_cleanup_lookup_error = if used_auth_phase {
            let active_cleanup_lookup_error = join_attempt.cleanup_lookup_error;
            if let Some(err) = active_cleanup_lookup_error.as_deref() {
                if tailscale_api_permission_denied(err) {
                    if last_logged_cleanup_lookup_error.as_deref()
                        != Some(TAILSCALE_CLEANUP_PERMISSION_DENIED_MARKER)
                    {
                        eprintln!(
                            "       Warning: tailnet device cleanup skipped (OAuth scopes missing: {TAILSCALE_OAUTH_SCOPE_HINT}). Run `sudo psht-server doctor`."
                        );
                        last_logged_cleanup_lookup_error =
                            Some(TAILSCALE_CLEANUP_PERMISSION_DENIED_MARKER.to_string());
                    }
                } else if last_logged_cleanup_lookup_error.as_deref() != Some(err) {
                    eprintln!(
                        "       Warning: unable to inspect tailnet devices during auth-key recovery: {err}"
                    );
                    last_logged_cleanup_lookup_error = Some(err.to_string());
                } else {
                    eprintln!(
                        "       Warning: tailnet device lookup still failing during auth-key recovery"
                    );
                }
            }
            active_cleanup_lookup_error
        } else {
            None
        };

        let Some(dns_name) = name.as_deref() else {
            last_observation = "tailscale DNS name unavailable".to_string();
            eprintln!("       Tailscale DNS name unavailable yet; retrying");
            if !used_auth_phase {
                in_auth_phase = true;
                eprintln!(
                    "       State-based tailscale recovery produced no DNS name; switching to auth-key recovery"
                );
            }
            if used_auth_phase
                && let Some(err) = active_cleanup_lookup_error.as_deref()
                && !tailscale_api_permission_denied(err)
            {
                return Err(format!(
                    "failed to acquire exact tailscale hostname '{app}' after auth-key join because ownership cleanup was unavailable: {err}; observed dns: unavailable"
                ));
            }
            if let Err(reset_err) = reset_for_retry(container_app) {
                eprintln!("       Warning: failed to reset tailscale before retry: {reset_err}");
            }
            sleep(retry_sleep);
            continue;
        };

        if !tailscale_hostname_is_exact(dns_name, app) {
            last_observation = format!("hostname mismatch: got '{dns_name}', expected '{app}'");
            eprintln!(
                "       Hostname mismatch: got '{dns_name}', expected label '{app}'. Retrying..."
            );
            if !used_auth_phase {
                in_auth_phase = true;
                eprintln!(
                    "       State-based tailscale hostname was not exact; switching to auth-key recovery"
                );
            }
            if used_auth_phase
                && let Some(err) = active_cleanup_lookup_error.as_deref()
                && !tailscale_api_permission_denied(err)
            {
                return Err(format!(
                    "failed to acquire exact tailscale hostname '{app}' after auth-key join because ownership cleanup was unavailable: {err}; observed dns: {dns_name}"
                ));
            }
            if let Err(reset_err) = reset_for_retry(container_app) {
                eprintln!("       Warning: failed to reset tailscale before retry: {reset_err}");
            }
            sleep(retry_sleep);
            continue;
        }

        match wait_healthy(container_app) {
            Ok(()) => {
                eprintln!("       Exact tailscale hostname acquired: {dns_name}");
                return Ok(name);
            }
            Err(health_err) => {
                last_observation = format!("tailscale not healthy yet: {health_err}");
                eprintln!("       Tailscale not healthy yet: {health_err}");
                if !used_auth_phase {
                    in_auth_phase = true;
                    eprintln!(
                        "       State-based tailscale health check failed; switching to auth-key recovery"
                    );
                }
                if let Err(reset_err) = reset_for_retry(container_app) {
                    eprintln!(
                        "       Warning: failed to reset tailscale before retry: {reset_err}"
                    );
                }
                sleep(retry_sleep);
            }
        }
    }

    Err(format!(
        "failed to acquire exact tailscale hostname '{app}' within {}s (attempts: {}); last observation: {last_observation}",
        TAILSCALE_EXACT_HOSTNAME_TOTAL_TIMEOUT_SECS, attempt
    ))
}

fn acquire_exact_tailscale_hostname_for_deploy(
    container_app: &str,
    app: &str,
) -> Result<Option<String>, String> {
    acquire_exact_tailscale_hostname_with_retry(
        container_app,
        app,
        |container, machine_name| {
            check_deploy_interrupt(app, "tailscale exact-hostname acquisition")?;
            let name = tailscale::join_with_state_in_container(container, machine_name)?;
            if name
                .as_deref()
                .map(str::trim)
                .is_some_and(|dns| !dns.is_empty())
                && let Err(track_err) = track_owned_tailscale_device(app, container, "state")
            {
                eprintln!("       Warning: failed to track tailscale state device: {track_err}");
            }
            Ok(TailscaleJoinAttempt::from_dns_name(name))
        },
        |container, machine_name| {
            check_deploy_interrupt(app, "tailscale exact-hostname auth-key fallback")?;
            let current_device_id = read_tailscale_self_snapshot(container)
                .ok()
                .and_then(|snapshot| snapshot.device_id);
            let mut cleanup_lookup_error = None;
            match cleanup_owned_tailscale_hostname_conflicts(app, current_device_id.as_deref()) {
                Ok(cleanup) => {
                    if !cleanup.removed_device_ids.is_empty()
                        || !cleanup.retired_device_ids.is_empty()
                    {
                        eprintln!(
                            "       Reclaimed tracked tailscale devices (removed: {}; retired: {})",
                            join_or_none(&cleanup.removed_device_ids),
                            join_or_none(&cleanup.retired_device_ids),
                        );
                    }
                }
                Err(err) => cleanup_lookup_error = Some(err),
            }

            let name = tailscale::join_with_auth_key_in_container(container, machine_name)?;
            if name
                .as_deref()
                .map(str::trim)
                .is_some_and(|dns| !dns.is_empty())
                && let Err(track_err) = track_owned_tailscale_device(app, container, "auth_key")
            {
                eprintln!("       Warning: failed to track tailscale auth-key device: {track_err}");
            }
            Ok(TailscaleJoinAttempt {
                dns_name: name,
                cleanup_lookup_error,
            })
        },
        |container| {
            check_deploy_interrupt(app, "tailscale health wait")?;
            wait_for_tailscale_online(container, Duration::from_secs(TAILSCALE_ONLINE_WAIT_SECS))
                .map(|_| ())
        },
        |container| {
            check_deploy_interrupt(app, "tailscale retry reset")?;
            reset_tailscale_for_retry(container)
        },
        thread::sleep,
    )
}

fn seed_tailscale_state_volume_from_container(
    app: &str,
    pool: &str,
    volume: &str,
) -> Result<(), String> {
    if container::has_tailscale_state_mount(app, pool, volume)? {
        return Ok(());
    }

    eprintln!("       Seeding tailscale state volume from current container");
    container::ensure_tailscale_state_seed_mount(app, pool, volume)?;
    let seed_cmd = format!(
        "mkdir -p {0} && if [ -z \"$(ls -A {0} 2>/dev/null)\" ]; then if [ -f /var/lib/tailscale/tailscaled.state ]; then cp /var/lib/tailscale/tailscaled.state {0}/tailscaled.state; fi; fi",
        TAILSCALE_STATE_SEED_PATH
    );
    let seed_result = container::exec_cmd(app, &seed_cmd);
    let unmount_result = container::remove_tailscale_state_seed_mount(app);

    if let Err(err) = seed_result {
        let _ = unmount_result;
        return Err(format!(
            "failed to seed tailscale state volume from container '{app}': {err}"
        ));
    }
    if let Err(err) = unmount_result {
        return Err(format!(
            "failed to detach staged tailscale state volume from container '{app}': {err}"
        ));
    }
    Ok(())
}

fn tailscale_self_status_summary_from_json(app: &str, json: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("failed to parse tailscale status: {e}"))?;

    let self_value = value
        .get("Self")
        .ok_or_else(|| "missing Self in tailscale status".to_string())?;
    let host = self_value
        .get("HostName")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let dns = self_value
        .get("DNSName")
        .and_then(serde_json::Value::as_str)
        .map(|name| name.trim_end_matches('.').to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let online = self_value
        .get("Online")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let ips = self_value
        .get("TailscaleIPs")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|joined| !joined.is_empty())
        .unwrap_or_else(|| "-".to_string());
    let backend_state = value
        .get("BackendState")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let health: Vec<String> = value
        .get("Health")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();

    let mut lines = vec![
        format!("App: {app}"),
        format!("Host: {host}"),
        format!("DNS: {dns}"),
        format!("State: {backend_state}"),
        format!("Online: {}", if online { "yes" } else { "no" }),
        format!("IPs: {ips}"),
    ];
    if !health.is_empty() {
        lines.push(format!("Health: {}", health.join(" | ")));
    }
    if backend_state != "Running" || !online || !health.is_empty() {
        lines.push(format!("Repair: psht tailscale up {app}"));
    }

    Ok(lines.join("\n"))
}

fn tailscale_ssh_enabled_from_status_json(json: &str) -> Result<bool, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("failed to parse tailscale status: {e}"))?;

    if value
        .pointer("/Self/SSH")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(true);
    }

    let capmap_has_ssh = value
        .get("Self")
        .and_then(serde_json::Value::as_object)
        .and_then(|self_obj| self_obj.get("CapMap"))
        .and_then(serde_json::Value::as_object)
        .map(|caps| caps.contains_key("https://tailscale.com/cap/ssh"))
        .unwrap_or(false);
    if capmap_has_ssh {
        return Ok(true);
    }

    let capabilities_have_ssh = value
        .get("Self")
        .and_then(serde_json::Value::as_object)
        .and_then(|self_obj| self_obj.get("Capabilities"))
        .and_then(serde_json::Value::as_array)
        .map(|caps| {
            caps.iter()
                .filter_map(serde_json::Value::as_str)
                .any(|cap| {
                    cap.eq_ignore_ascii_case("https://tailscale.com/cap/ssh")
                        || cap.eq_ignore_ascii_case("ssh")
                })
        })
        .unwrap_or(false);
    if capabilities_have_ssh {
        return Ok(true);
    }

    Ok(json.contains("\"SSH\":true"))
}

fn tailscale_ssh_enabled() -> Result<bool, String> {
    let json = run_cmd_capture("tailscale", &["status", "--json"])?;
    tailscale_ssh_enabled_from_status_json(&json)
}

fn current_psht_binary() -> Result<PathBuf, String> {
    let exe = env::current_exe().map_err(|e| format!("failed to locate current binary: {e}"))?;
    match fs::canonicalize(&exe) {
        Ok(path) => Ok(path),
        Err(_) => Ok(exe),
    }
}

fn binary_version(path: &Path) -> Option<String> {
    let output = Command::new(path).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();
    let version = line.split_whitespace().nth(1)?;
    if version.is_empty() {
        return None;
    }
    Some(version.to_string())
}

fn binary_matches_version(path: &Path, expected: &str) -> bool {
    binary_version(path).as_deref() == Some(expected)
}

fn configured_forge_url() -> String {
    env::var("PSHT_FORGE_URL")
        .ok()
        .map(|raw| raw.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_FORGE_URL.to_string())
}

fn parse_latest_release_version_url(url: &str) -> Option<String> {
    let no_fragment = url.trim().split('#').next()?;
    let no_query = no_fragment.split('?').next()?;
    let trimmed = no_query.trim_end_matches('/');
    let tag = trimmed.rsplit('/').next()?;
    if tag.is_empty() || tag.eq_ignore_ascii_case("latest") {
        return None;
    }
    let version = tag.trim_start_matches('v').trim();
    if version.is_empty() {
        return None;
    }
    Some(version.to_string())
}

fn parse_version_components(version: &str) -> Option<Vec<u64>> {
    let stripped = version
        .trim()
        .trim_start_matches('v')
        .trim_start_matches('V');
    let core = stripped.split('+').next().unwrap_or(stripped);
    let core = core.split('-').next().unwrap_or(core);
    if core.is_empty() {
        return None;
    }

    let mut components = Vec::new();
    for piece in core.split('.') {
        if piece.is_empty() {
            return None;
        }
        let value = piece.parse::<u64>().ok()?;
        components.push(value);
    }
    if components.is_empty() {
        return None;
    }
    Some(components)
}

fn version_is_newer(latest: &str, current: &str) -> bool {
    let Some(latest_parts) = parse_version_components(latest) else {
        return false;
    };
    let Some(current_parts) = parse_version_components(current) else {
        return false;
    };
    let len = latest_parts.len().max(current_parts.len());
    for idx in 0..len {
        let lhs = latest_parts.get(idx).copied().unwrap_or(0);
        let rhs = current_parts.get(idx).copied().unwrap_or(0);
        if lhs > rhs {
            return true;
        }
        if lhs < rhs {
            return false;
        }
    }
    false
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct UpgradeCheckState {
    checked_at: u64,
    latest: String,
}

fn read_upgrade_check_state_from(path: &Path) -> Result<Option<UpgradeCheckState>, String> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
    };
    let state: UpgradeCheckState =
        toml::from_str(&content).map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
    Ok(Some(state))
}

fn write_upgrade_check_state_to(path: &Path, state: &UpgradeCheckState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let content = toml::to_string_pretty(state)
        .map_err(|e| format!("failed to serialize {}: {e}", path.display()))?;
    fs::write(path, content).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

fn latest_release_version_with_timeouts(
    connect_timeout_secs: &str,
    max_time_secs: &str,
) -> Result<String, String> {
    let forge_url = configured_forge_url();
    let latest_url = run_cmd_capture(
        "curl",
        &[
            "-fsSL",
            "--connect-timeout",
            connect_timeout_secs,
            "--max-time",
            max_time_secs,
            "-o",
            "/dev/null",
            "-w",
            "%{url_effective}",
            &format!("{forge_url}/releases/latest"),
        ],
    )?;
    parse_latest_release_version_url(&latest_url)
        .ok_or_else(|| format!("failed to parse latest release version from URL: {latest_url}"))
}

fn latest_release_version_for_warning() -> Option<String> {
    let path = upgrade_check_state_path();
    let cached = read_upgrade_check_state_from(&path).ok().flatten();
    let now = now_unix_secs();

    if let Some(state) = cached.as_ref()
        && now.saturating_sub(state.checked_at) <= UPGRADE_CHECK_TTL_SECS
        && !state.latest.trim().is_empty()
    {
        return Some(state.latest.trim().to_string());
    }

    match latest_release_version_with_timeouts("2", "3") {
        Ok(latest) => {
            let state = UpgradeCheckState {
                checked_at: now,
                latest: latest.clone(),
            };
            let _ = write_upgrade_check_state_to(&path, &state);
            Some(latest)
        }
        Err(_) => cached
            .map(|state| state.latest.trim().to_string())
            .filter(|latest| !latest.is_empty()),
    }
}

fn warn_if_upgrade_available() {
    let current = env!("CARGO_PKG_VERSION");
    let Some(latest) = latest_release_version_for_warning() else {
        return;
    };
    if version_is_newer(&latest, current) {
        eprintln!(
            "       Warning: psht-server {current} is behind latest {latest}; run `sudo psht-server upgrade`"
        );
    }
}

fn detect_release_target() -> Result<&'static str, String> {
    let arch = run_cmd_capture("uname", &["-m"])?;
    match arch.trim() {
        "x86_64" => Ok("x86_64-unknown-linux-gnu"),
        "aarch64" => Ok("aarch64-unknown-linux-gnu"),
        other => Err(format!("unsupported architecture: {other}")),
    }
}

fn first_storage_pool_name() -> Result<Option<String>, String> {
    let json = run_incus_metadata_capture(&["storage", "list", "--format=json"])?;
    let value: serde_json::Value = serde_json::from_str(&json)
        .map_err(|e| format!("failed to parse incus storage list: {e}"))?;
    let pools = value
        .as_array()
        .ok_or_else(|| "unexpected incus storage list response".to_string())?;
    for pool in pools {
        if let Some(name) = pool.get("name").and_then(serde_json::Value::as_str) {
            if !name.is_empty() {
                return Ok(Some(name.to_string()));
            }
        }
    }
    Ok(None)
}

fn default_storage_pool() -> Result<String, String> {
    if let Ok(pool) =
        run_incus_metadata_capture(&["profile", "device", "get", "default", "root", "pool"])
    {
        let pool = pool.trim();
        if !pool.is_empty() {
            return Ok(pool.to_string());
        }
    }

    if let Some(pool) = first_storage_pool_name()? {
        return Ok(pool);
    }

    // Fresh Incus installs may have no storage configured yet.
    run_cmd("incus", &["storage", "create", "default", "dir"])?;
    Ok("default".to_string())
}

fn app_storage_volume_name(app: &str) -> String {
    format!("psht-storage-{app}")
}

fn app_tailscale_volume_name(app: &str) -> String {
    format!("psht-tailscale-{app}")
}

fn ensure_app_storage_volume(app: &str) -> Result<(String, String), String> {
    let pool = default_storage_pool()?;
    let volume = app_storage_volume_name(app);
    let show_args = vec!["storage", "volume", "show", pool.as_str(), volume.as_str()];
    if !incus_metadata_command_succeeds(&show_args)? {
        let create_args = vec![
            "storage",
            "volume",
            "create",
            pool.as_str(),
            volume.as_str(),
        ];
        run_cmd("incus", &create_args)?;
    }
    Ok((pool, volume))
}

fn delete_app_storage_volume(app: &str) -> Result<(), String> {
    let pool = default_storage_pool()?;
    let volume = app_storage_volume_name(app);
    let show_args = vec!["storage", "volume", "show", pool.as_str(), volume.as_str()];
    if !incus_metadata_command_succeeds(&show_args)? {
        return Ok(());
    }
    let delete_args = vec![
        "storage",
        "volume",
        "delete",
        pool.as_str(),
        volume.as_str(),
    ];
    run_cmd("incus", &delete_args)
}

fn ensure_app_tailscale_volume(app: &str) -> Result<(String, String), String> {
    let pool = default_storage_pool()?;
    let volume = app_tailscale_volume_name(app);
    let show_args = vec!["storage", "volume", "show", pool.as_str(), volume.as_str()];
    if !incus_metadata_command_succeeds(&show_args)? {
        let create_args = vec![
            "storage",
            "volume",
            "create",
            pool.as_str(),
            volume.as_str(),
        ];
        run_cmd("incus", &create_args)?;
    }
    Ok((pool, volume))
}

fn delete_app_tailscale_volume(app: &str) -> Result<(), String> {
    let pool = default_storage_pool()?;
    let volume = app_tailscale_volume_name(app);
    let show_args = vec!["storage", "volume", "show", pool.as_str(), volume.as_str()];
    if !incus_metadata_command_succeeds(&show_args)? {
        return Ok(());
    }
    let delete_args = vec![
        "storage",
        "volume",
        "delete",
        pool.as_str(),
        volume.as_str(),
    ];
    run_cmd("incus", &delete_args)
}

fn first_managed_network_name() -> Result<Option<String>, String> {
    let json = run_cmd_capture("incus", &["network", "list", "--format=json"])?;
    let value: serde_json::Value = serde_json::from_str(&json)
        .map_err(|e| format!("failed to parse incus network list: {e}"))?;
    let networks = value
        .as_array()
        .ok_or_else(|| "unexpected incus network list response".to_string())?;

    let mut fallback = None;
    for network in networks {
        let Some(name) = network.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if name.is_empty() || name == "lo" {
            continue;
        }
        let managed = network
            .get("managed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if !managed {
            continue;
        }
        let ty = network
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if ty == "bridge" {
            return Ok(Some(name.to_string()));
        }
        if fallback.is_none() {
            fallback = Some(name.to_string());
        }
    }
    Ok(fallback)
}

fn default_network_name() -> Result<String, String> {
    for candidate in ["incusbr0", "default"] {
        if command_succeeds("incus", &["network", "show", candidate]) {
            return Ok(candidate.to_string());
        }
    }

    if let Some(existing) = first_managed_network_name()? {
        return Ok(existing);
    }

    let candidate = "incusbr0";
    if let Err(create_err) = run_cmd(
        "incus",
        &[
            "network",
            "create",
            candidate,
            "ipv4.address=auto",
            "ipv6.address=none",
        ],
    ) {
        if !command_succeeds("incus", &["network", "show", candidate]) {
            return Err(format!(
                "failed to create fallback incus network {candidate}: {create_err}"
            ));
        }
    }
    Ok(candidate.to_string())
}

fn project_uses_profiles(project: &str) -> Result<bool, String> {
    let value = run_cmd_capture("incus", &["project", "get", project, "features.profiles"])?;
    Ok(value.trim().eq_ignore_ascii_case("true"))
}

fn profile_has_root_disk(profile: &str) -> bool {
    let mut in_devices = false;
    let mut current_device_indent = None;
    let mut current_is_disk = false;
    let mut current_is_root_path = false;

    for raw_line in profile.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() {
            continue;
        }

        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }

        let indent = line
            .chars()
            .take_while(|ch| ch.is_ascii_whitespace())
            .count();

        if !in_devices {
            if trimmed == "devices:" {
                in_devices = true;
            }
            continue;
        }

        if indent == 0 {
            if current_is_disk && current_is_root_path {
                return true;
            }
            break;
        }

        if indent == 2 && trimmed.ends_with(':') {
            if current_is_disk && current_is_root_path {
                return true;
            }
            current_device_indent = Some(indent);
            current_is_disk = false;
            current_is_root_path = false;
            continue;
        }

        let Some(device_indent) = current_device_indent else {
            continue;
        };

        if indent <= device_indent {
            if current_is_disk && current_is_root_path {
                return true;
            }
            current_device_indent = None;
            continue;
        }

        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if key == "type" && value == "disk" {
                current_is_disk = true;
            }
            if key == "path" && value == "/" {
                current_is_root_path = true;
            }
        }
    }

    current_is_disk && current_is_root_path
}

fn profile_has_nic(profile: &str) -> bool {
    let mut in_devices = false;
    let mut current_device_indent = None;
    let mut current_is_nic = false;

    for raw_line in profile.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() {
            continue;
        }

        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }

        let indent = line
            .chars()
            .take_while(|ch| ch.is_ascii_whitespace())
            .count();

        if !in_devices {
            if trimmed == "devices:" {
                in_devices = true;
            }
            continue;
        }

        if indent == 0 {
            if current_is_nic {
                return true;
            }
            break;
        }

        if indent == 2 && trimmed.ends_with(':') {
            if current_is_nic {
                return true;
            }
            current_device_indent = Some(indent);
            current_is_nic = false;
            continue;
        }

        let Some(device_indent) = current_device_indent else {
            continue;
        };

        if indent <= device_indent {
            if current_is_nic {
                return true;
            }
            current_device_indent = None;
            continue;
        }

        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if key == "type" && value == "nic" {
                current_is_nic = true;
            }
        }
    }

    current_is_nic
}

fn project_profile_has_root_disk(project: &str) -> Result<bool, String> {
    let profile = run_cmd_capture(
        "incus",
        &["--project", project, "profile", "show", "default"],
    )?;
    Ok(profile_has_root_disk(&profile))
}

fn project_profile_has_nic(project: &str) -> Result<bool, String> {
    let profile = run_cmd_capture(
        "incus",
        &["--project", project, "profile", "show", "default"],
    )?;
    Ok(profile_has_nic(&profile))
}

fn ensure_project_default_profile(project: &str) -> Result<(), String> {
    if !project_uses_profiles(project)? {
        return Ok(());
    }

    if !command_succeeds(
        "incus",
        &["--project", project, "profile", "show", "default"],
    ) {
        run_cmd(
            "incus",
            &["--project", project, "profile", "create", "default"],
        )?;
    }

    let needs_root_disk = !project_profile_has_root_disk(project)?;
    let needs_nic = !project_profile_has_nic(project)?;
    if !needs_root_disk && !needs_nic {
        return Ok(());
    }

    if needs_root_disk {
        let pool = default_storage_pool()?;
        let path_arg = "path=/".to_string();
        let pool_arg = format!("pool={pool}");
        run_cmd(
            "incus",
            &[
                "--project",
                project,
                "profile",
                "device",
                "add",
                "default",
                "root",
                "disk",
                &path_arg,
                &pool_arg,
            ],
        )?;
    }

    if needs_nic {
        let network = default_network_name()?;
        let network_arg = format!("network={network}");
        run_cmd(
            "incus",
            &[
                "--project",
                project,
                "profile",
                "device",
                "add",
                "default",
                "psht-net0",
                "nic",
                &network_arg,
            ],
        )?;
    }
    Ok(())
}

fn latest_release_version() -> Result<String, String> {
    latest_release_version_with_timeouts("5", "10")
}

fn release_version_candidates(current: &str, latest: Option<&str>) -> Vec<String> {
    let mut versions = vec![current.to_string()];
    if let Some(latest) = latest {
        let latest = latest.trim();
        if !latest.is_empty() && latest != current {
            versions.push(latest.to_string());
        }
    }
    versions
}

fn cli_release_url(forge_url: &str, version: &str, target: &str) -> String {
    format!("{forge_url}/releases/download/v{version}/psht-{version}-{target}.tar.gz")
}

fn install_cli_from_release(dst: &Path) -> Result<(), String> {
    let current_version = env!("CARGO_PKG_VERSION");
    let latest_version = latest_release_version().ok();
    let versions = release_version_candidates(current_version, latest_version.as_deref());
    let forge_url = configured_forge_url();
    let target = detect_release_target()?;
    let tmpdir = run_cmd_capture("mktemp", &["-d"])?;
    let tmpdir_path = PathBuf::from(tmpdir);
    let tmpdir_s = tmpdir_path.to_string_lossy().to_string();
    let tarball = tmpdir_path.join("psht.tar.gz");
    let tarball_s = tarball.to_string_lossy().to_string();

    let result = (|| {
        let mut errors = Vec::new();
        for (idx, version) in versions.iter().enumerate() {
            let url = cli_release_url(&forge_url, version, target);
            if let Err(e) = run_cmd_quiet("curl", &["-fsSL", &url, "-o", &tarball_s]) {
                errors.push(format!("{version}: {url}: {e}"));
                continue;
            }
            let _ = fs::remove_file(tmpdir_path.join("psht"));
            if let Err(e) = run_cmd_quiet("tar", &["xzf", &tarball_s, "-C", &tmpdir_s]) {
                errors.push(format!("{version}: {e}"));
                continue;
            }

            let extracted = tmpdir_path.join("psht");
            if !extracted.is_file() {
                errors.push(format!("{version}: release tarball did not contain psht"));
                continue;
            }
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
            }
            fs::copy(&extracted, dst).map_err(|e| {
                format!(
                    "failed to copy {} to {}: {e}",
                    extracted.display(),
                    dst.display()
                )
            })?;
            fs::set_permissions(dst, fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("failed to chmod {}: {e}", dst.display()))?;
            if idx > 0 {
                eprintln!(
                    "-----> psht {current_version} not published; using released psht {version}"
                );
            }
            return Ok(());
        }
        Err(format!(
            "release download attempts failed: {}",
            errors.join("; ")
        ))
    })();

    let _ = fs::remove_dir_all(&tmpdir_path);
    result
}

fn build_cli_from_source(dst: &Path) -> Result<bool, String> {
    let current_bin = current_psht_binary()?;
    let mut cursor = current_bin.parent().map(|p| p.to_path_buf());
    while let Some(dir) = cursor {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file() {
            let output = Command::new("cargo")
                .args(["build", "--release", "--bin", "psht"])
                .current_dir(&dir)
                .output()
                .map_err(|e| format!("failed to run cargo in {}: {e}", dir.display()))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                if stderr.is_empty() {
                    return Err(format!("cargo build failed in {}", dir.display()));
                }
                return Err(format!("cargo build failed in {}: {stderr}", dir.display()));
            }

            let built = dir.join("target/release/psht");
            if !built.is_file() {
                return Err(format!(
                    "cargo build succeeded but {} was not created",
                    built.display()
                ));
            }
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
            }
            fs::copy(&built, dst).map_err(|e| {
                format!(
                    "failed to copy {} to {}: {e}",
                    built.display(),
                    dst.display()
                )
            })?;
            fs::set_permissions(dst, fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("failed to chmod {}: {e}", dst.display()))?;
            return Ok(true);
        }
        cursor = dir.parent().map(|p| p.to_path_buf());
    }
    Ok(false)
}

fn ensure_cli_binary() -> Result<PathBuf, String> {
    let expected_version = env!("CARGO_PKG_VERSION");
    let home_cli = home_dir().join("bin/psht");
    if home_cli.is_file() && binary_matches_version(&home_cli, expected_version) {
        return Ok(home_cli);
    }

    let current_bin = current_psht_binary()?;
    if let Some(parent) = current_bin.parent() {
        let sibling = parent.join("psht");
        if sibling.is_file() && binary_matches_version(&sibling, expected_version) {
            return Ok(sibling);
        }
    }

    let build_err = match build_cli_from_source(&home_cli) {
        Ok(true) => {
            if binary_matches_version(&home_cli, expected_version) {
                return Ok(home_cli);
            }
            let installed = binary_version(&home_cli).unwrap_or_else(|| "unknown".to_string());
            return Err(format!(
                "failed to provide psht {expected_version}: built version was {installed}"
            ));
        }
        Ok(false) => None,
        Err(e) => Some(e),
    };

    if let Err(download_err) = install_cli_from_release(&home_cli) {
        if let Some(build_err) = build_err {
            return Err(format!(
                "failed to provide psht (build failed: {build_err}; release download failed: {download_err})"
            ));
        }
        return Err(format!("failed to provide psht: {download_err}"));
    }

    if binary_matches_version(&home_cli, expected_version) {
        return Ok(home_cli);
    }
    let installed = binary_version(&home_cli).unwrap_or_else(|| "unknown".to_string());
    Err(format!(
        "failed to provide psht {expected_version}: installed version was {installed}"
    ))
}

fn path_is_world_executable(path: &Path) -> Result<bool, String> {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    let mut dirs = Vec::new();
    let mut cursor = resolved.parent();
    while let Some(dir) = cursor {
        if dir == Path::new("/") {
            break;
        }
        dirs.push(dir.to_path_buf());
        cursor = dir.parent();
    }
    dirs.reverse();

    for dir in dirs {
        let meta =
            fs::metadata(&dir).map_err(|e| format!("failed to stat {}: {e}", dir.display()))?;
        if !meta.is_dir() {
            return Ok(false);
        }
        if meta.permissions().mode() & 0o001 == 0 {
            return Ok(false);
        }
    }

    let file_meta = fs::metadata(&resolved)
        .map_err(|e| format!("failed to stat {}: {e}", resolved.display()))?;
    Ok(file_meta.permissions().mode() & 0o001 != 0)
}

fn prepare_server_binary(current_bin: &Path) -> Result<PathBuf, String> {
    let resolved = fs::canonicalize(current_bin).unwrap_or_else(|_| current_bin.to_path_buf());
    if path_is_world_executable(&resolved)? {
        return Ok(resolved);
    }

    let fallback = PathBuf::from("/usr/local/bin/psht-server");
    eprintln!(
        "-----> Binary path {} is not accessible to other users; installing to {}",
        resolved.display(),
        fallback.display()
    );

    if resolved != fallback {
        if let Some(parent) = fallback.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        fs::copy(&resolved, &fallback).map_err(|e| {
            format!(
                "failed to copy {} to {}: {e}",
                resolved.display(),
                fallback.display()
            )
        })?;
    }
    let fallback_str = fallback.to_string_lossy().to_string();
    run_cmd("chmod", &["755", &fallback_str])?;
    Ok(fallback)
}

fn stack_hash(path: &Path) -> Result<String, String> {
    let contents = fs::read(path)
        .map_err(|e| format!("failed to read stack script {}: {e}", path.display()))?;
    let mut hasher = DefaultHasher::new();
    contents.hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
}

fn apt_packages_fingerprint(packages: &[String]) -> Option<String> {
    let mut normalized: Vec<String> = packages
        .iter()
        .map(|pkg| pkg.trim())
        .filter(|pkg| !pkg.is_empty())
        .map(ToString::to_string)
        .collect();
    if normalized.is_empty() {
        return None;
    }

    normalized.sort();
    normalized.dedup();

    let canonical = normalized.join("\n");
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    Some(format!("{:016x}", hasher.finish()))
}

fn setup_hash(stack_hash: &str, apt_fingerprint: Option<&str>) -> String {
    match apt_fingerprint {
        Some(fingerprint) => format!("{stack_hash}:{fingerprint}"),
        None => stack_hash.to_string(),
    }
}

fn resolve_stack_in(
    app: &str,
    code_dir: &Path,
    detected_stack: &str,
    stacks: &Path,
) -> Result<(String, PathBuf), String> {
    let custom = code_dir.join("psht-stack.sh");
    if custom.exists() {
        let saved = stacks.join(format!("{app}.sh"));
        fs::copy(&custom, &saved).map_err(|e| format!("failed to save custom stack: {e}"))?;
        Ok((app.to_string(), saved))
    } else {
        Ok((
            detected_stack.to_string(),
            stacks.join(format!("{detected_stack}.sh")),
        ))
    }
}

fn resolve_stack(
    app: &str,
    code_dir: &Path,
    detected_stack: &str,
) -> Result<(String, PathBuf), String> {
    resolve_stack_in(app, code_dir, detected_stack, &stacks_dir())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusyOpPolicy {
    Auto,
    Diagnose,
    Force,
}

static DEPLOY_INTERRUPT_SIGNAL_PENDING: AtomicBool = AtomicBool::new(false);
static DEPLOY_INTERRUPT_SIGNAL_INSTALLED: OnceLock<()> = OnceLock::new();

#[cfg(unix)]
const SIGHUP: i32 = 1;
#[cfg(unix)]
const SIGINT: i32 = 2;
#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGNAL_ERR: usize = usize::MAX;

#[cfg(unix)]
unsafe extern "C" {
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
}

extern "C" fn mark_deploy_interrupt_signal(_signal: i32) {
    DEPLOY_INTERRUPT_SIGNAL_PENDING.store(true, Ordering::SeqCst);
}

fn install_deploy_interrupt_signal_handlers() {
    DEPLOY_INTERRUPT_SIGNAL_INSTALLED.get_or_init(|| {
        #[cfg(unix)]
        {
            // SAFETY: Installing process signal handlers is required to map Ctrl-C/session
            // hangup to deploy interruption. Handler only stores to an atomic flag.
            unsafe {
                for (signum, name) in [(SIGINT, "SIGINT"), (SIGTERM, "SIGTERM"), (SIGHUP, "SIGHUP")]
                {
                    if signal(signum, mark_deploy_interrupt_signal) == SIGNAL_ERR {
                        std::eprintln!(
                            "warning: failed to install {name} handler; Ctrl-C cancel may be degraded"
                        );
                    }
                }
            }
        }
    });
}

fn app_ref_from_instance_name(instance: &str) -> Option<String> {
    app_state::app_ref_from_instance_name(instance)
}

fn instance_name_from_app_ref(app_ref: &str) -> String {
    app_state::instance_name_from_app_ref(app_ref)
}

fn sqlite_i64_to_u64(value: i64, app: &str, field: &str) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| {
        format!("invalid sqlite value for {field} in app '{app}': expected non-negative integer")
    })
}

fn sqlite_i64_to_u32(value: i64, app: &str, field: &str) -> Result<u32, String> {
    u32::try_from(value).map_err(|_| {
        format!("invalid sqlite value for {field} in app '{app}': expected u32-compatible integer")
    })
}

fn sqlite_u64_to_i64(value: u64, app: &str, field: &str) -> Result<i64, String> {
    i64::try_from(value)
        .map_err(|_| format!("value for {field} in app '{app}' exceeds sqlite integer range"))
}

fn read_app_runtime_state(app: &str) -> Result<Option<AppRuntimeState>, String> {
    app_state::read_app_runtime_state(app)
}

fn write_app_runtime_state(
    app: &str,
    active_app_ref: &str,
    previous_app_ref: Option<&str>,
) -> Result<(), String> {
    app_state::write_app_runtime_state(app, active_app_ref, previous_app_ref)
}

fn clear_app_runtime_state(app: &str) -> Result<(), String> {
    app_state::clear_app_runtime_state(app)
}

fn read_all_app_runtime_states() -> Result<Vec<(String, AppRuntimeState)>, String> {
    app_state::read_all_app_runtime_states()
}

fn resolve_active_app_ref(app: &str) -> Result<Option<String>, String> {
    app_state::resolve_active_app_ref(app)
}

fn resolve_existing_active_app_ref(app: &str) -> Result<String, String> {
    app_state::resolve_existing_active_app_ref(app)
}

fn deploy_lock_path_in(dir: &Path, app: &str) -> PathBuf {
    dir.join(format!("{app}.lock"))
}

fn deploy_lock_path(app: &str) -> PathBuf {
    deploy_lock_path_in(&deploy_lock_dir(), app)
}

fn cleanup_lock_path_in(dir: &Path, app: &str) -> PathBuf {
    dir.join(format!("{app}.lock"))
}

fn cleanup_lock_path(app: &str) -> PathBuf {
    cleanup_lock_path_in(&cleanup_lock_dir(), app)
}

struct DeployLockGuard {
    path: PathBuf,
}

impl Drop for DeployLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct CleanupLockGuard {
    path: PathBuf,
}

impl Drop for CleanupLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn env_u64(name: &str, default_value: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(default_value)
}

fn reconcile_lease_ttl_secs() -> u64 {
    env_u64("PSHT_LEASE_TTL_SECS", RECONCILE_LEASE_TTL_SECS).max(5)
}

fn reconcile_lease_heartbeat_secs() -> u64 {
    env_u64("PSHT_LEASE_HEARTBEAT_SECS", RECONCILE_LEASE_HEARTBEAT_SECS).max(1)
}

fn takeover_retry_ms() -> u64 {
    env_u64("PSHT_TAKEOVER_RETRY_MS", TAKEOVER_RETRY_MS).max(100)
}

fn takeover_max_cancel_per_cycle() -> usize {
    env_u64(
        "PSHT_TAKEOVER_MAX_CANCEL_PER_CYCLE",
        TAKEOVER_MAX_CANCEL_PER_CYCLE as u64,
    )
    .max(1) as usize
}

fn busy_op_policy_from_raw(raw: Option<&str>) -> BusyOpPolicy {
    match raw.map(|value| value.to_ascii_lowercase()).as_deref() {
        Some("diagnose") => BusyOpPolicy::Diagnose,
        Some("force") => BusyOpPolicy::Force,
        _ => BusyOpPolicy::Auto,
    }
}

fn busy_op_policy() -> BusyOpPolicy {
    let raw = env::var(BUSY_OP_POLICY_ENV).ok();
    busy_op_policy_from_raw(raw.as_deref())
}

fn blocked_op_budget_secs() -> u64 {
    env_u64("PSHT_BLOCKED_OP_BUDGET_SECS", BLOCKED_OP_BUDGET_SECS).max(1)
}

fn append_reconcile_attempt_record(
    app: &str,
    step_name: &str,
    result: &str,
    detail_json: serde_json::Value,
) {
    reconcile_runtime::append_attempt(app, step_name, result, detail_json);
}

fn update_reconcile_phase_from_lease(app: &str, phase: &str, last_error: Option<&str>) {
    let phase = if phase.eq_ignore_ascii_case("blocked") {
        AppPhase::Blocked
    } else {
        AppPhase::Reconciling
    };
    reconcile_runtime::update_phase(app, phase, control_plane_snapshot(app), last_error);
}

fn refresh_active_reconcile_lease(app: &str) -> Result<(), String> {
    reconcile_runtime::refresh(
        app,
        (reconcile_lease_ttl_secs() * 1000) as i64,
        reconcile_lease_heartbeat_secs(),
    )
}

fn acquire_reconcile_lease(
    app: &str,
    ctx: &ReconcileIntentContext,
) -> Result<reconcile_runtime::ReconcileLeaseGuard, String> {
    let owner = format!("{}:{}:{}", hostname(), std::process::id(), ctx.intent_id);
    reconcile_runtime::acquire(
        app,
        &owner,
        &ctx.intent_id,
        ctx.generation,
        (reconcile_lease_ttl_secs() * 1000) as i64,
    )
}

fn parse_deploy_lock_metadata(content: &str) -> DeployLockMetadata {
    let mut metadata = DeployLockMetadata::default();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "pid" => {
                metadata.pid = value.trim().parse::<u32>().ok();
            }
            "created" => {
                metadata.created = value.trim().parse::<u64>().ok();
            }
            "updated" => {
                metadata.updated = value.trim().parse::<u64>().ok();
            }
            _ => {}
        }
    }
    metadata
}

fn read_deploy_lock_metadata_from(path: &Path) -> Result<Option<DeployLockMetadata>, String> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
    };
    Ok(Some(parse_deploy_lock_metadata(&content)))
}

fn read_deploy_lock_metadata(app: &str) -> Result<Option<DeployLockMetadata>, String> {
    read_deploy_lock_metadata_from(&deploy_lock_path(app))
}

fn write_deploy_lock_metadata(path: &Path, metadata: &DeployLockMetadata) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let pid = metadata.pid.unwrap_or_else(std::process::id);
    let created = metadata.created.unwrap_or_else(now_unix_secs);
    let updated = metadata.updated.unwrap_or_else(now_unix_secs);
    let body = format!("pid={pid}\ncreated={created}\nupdated={updated}\n");
    fs::write(path, body).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

fn refresh_deploy_lock_heartbeat_at(path: &Path, owner_pid: u32) -> Result<(), String> {
    let Some(mut metadata) = read_deploy_lock_metadata_from(path)? else {
        return Ok(());
    };
    if let Some(holder_pid) = metadata.pid
        && holder_pid != owner_pid
    {
        return Ok(());
    }
    metadata.pid = Some(owner_pid);
    metadata.created = Some(metadata.created.unwrap_or_else(now_unix_secs));
    metadata.updated = Some(now_unix_secs());
    write_deploy_lock_metadata(path, &metadata)
}

fn refresh_deploy_lock_heartbeat(app: &str) -> Result<(), String> {
    refresh_deploy_lock_heartbeat_at(&deploy_lock_path(app), std::process::id())
}

fn clear_deploy_lock_path(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to remove {}: {e}", path.display())),
    }
}

fn clear_deploy_lock(app: &str) -> Result<(), String> {
    clear_deploy_lock_path(&deploy_lock_path(app))
}

fn pid_is_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn send_kill_signal(pid: u32) -> Result<(), String> {
    let output = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .output()
        .map_err(|e| format!("failed to execute kill -KILL {pid}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    if !pid_is_alive(pid) {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if detail.is_empty() {
        Err(format!("failed to send SIGKILL to lock holder pid {pid}"))
    } else {
        Err(format!(
            "failed to send SIGKILL to lock holder pid {pid}: {detail}"
        ))
    }
}

fn write_new_lock_file(path: &Path) -> Result<(), std::io::Error> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let now = now_unix_secs();
    let body = format!("pid={}\ncreated={now}\nupdated={now}\n", std::process::id());
    file.write_all(body.as_bytes())?;
    Ok(())
}

fn lock_file_is_stale(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let Ok(elapsed) = modified.elapsed() else {
        return false;
    };
    elapsed.as_secs() > DEPLOY_LOCK_STALE_SECS
}

fn try_acquire_deploy_lock_at(path: &Path) -> Result<Option<DeployLockGuard>, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }

    for _ in 0..2 {
        match write_new_lock_file(path) {
            Ok(()) => {
                return Ok(Some(DeployLockGuard {
                    path: path.to_path_buf(),
                }));
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if lock_file_is_stale(path) {
                    match fs::remove_file(path) {
                        Ok(()) => continue,
                        Err(err) if err.kind() == ErrorKind::NotFound => continue,
                        Err(err) => {
                            return Err(format!(
                                "failed to clear stale lock {}: {err}",
                                path.display()
                            ));
                        }
                    }
                }
                return Ok(None);
            }
            Err(e) => {
                return Err(format!("failed to create lock {}: {e}", path.display()));
            }
        }
    }

    Ok(None)
}

fn try_acquire_deploy_lock(app: &str) -> Result<Option<DeployLockGuard>, String> {
    try_acquire_deploy_lock_at(&deploy_lock_path(app))
}

fn try_acquire_cleanup_lock_at(path: &Path) -> Result<Option<CleanupLockGuard>, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }

    for _ in 0..2 {
        match write_new_lock_file(path) {
            Ok(()) => {
                return Ok(Some(CleanupLockGuard {
                    path: path.to_path_buf(),
                }));
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if lock_file_is_stale(path) {
                    match fs::remove_file(path) {
                        Ok(()) => continue,
                        Err(err) if err.kind() == ErrorKind::NotFound => continue,
                        Err(err) => {
                            return Err(format!(
                                "failed to clear stale lock {}: {err}",
                                path.display()
                            ));
                        }
                    }
                }
                return Ok(None);
            }
            Err(e) => {
                return Err(format!("failed to create lock {}: {e}", path.display()));
            }
        }
    }

    Ok(None)
}

fn try_acquire_cleanup_lock(app: &str) -> Result<Option<CleanupLockGuard>, String> {
    try_acquire_cleanup_lock_at(&cleanup_lock_path(app))
}

fn git_deploy_status_from_str(status: &str) -> Result<GitDeployStatus, String> {
    match status {
        "pending" => Ok(GitDeployStatus::Pending),
        "success" => Ok(GitDeployStatus::Success),
        "failed" => Ok(GitDeployStatus::Failed),
        "interrupted" => Ok(GitDeployStatus::Interrupted),
        _ => Err(format!(
            "invalid git deploy status '{status}' in sqlite state"
        )),
    }
}

fn git_deploy_status_as_str(status: GitDeployStatus) -> &'static str {
    match status {
        GitDeployStatus::Pending => "pending",
        GitDeployStatus::Success => "success",
        GitDeployStatus::Failed => "failed",
        GitDeployStatus::Interrupted => "interrupted",
    }
}

fn git_deploy_state_from_row(
    row: sqlite_store::GitDeployStateRow,
) -> Result<GitDeployState, String> {
    Ok(GitDeployState {
        ref_name: row.ref_name,
        sha: row.sha,
        status: git_deploy_status_from_str(&row.status)?,
    })
}

fn read_git_deploy_state(app: &str) -> Result<Option<GitDeployState>, String> {
    sqlite_store::get_git_deploy_state(app)?
        .map(git_deploy_state_from_row)
        .transpose()
}

fn write_git_deploy_state(
    app: &str,
    target: &GitCheckoutTarget,
    status: GitDeployStatus,
) -> Result<(), String> {
    sqlite_store::upsert_git_deploy_state(&sqlite_store::GitDeployStateRow {
        app_id: app.to_string(),
        ref_name: target.ref_name.clone(),
        sha: target.sha.clone(),
        status: git_deploy_status_as_str(status).to_string(),
    })
}

fn clear_git_deploy_state(app: &str) -> Result<(), String> {
    sqlite_store::delete_git_deploy_state(app)
}

fn pending_git_request_from_row(
    row: sqlite_store::PendingGitRequestRow,
) -> Result<PendingGitDeployRequest, String> {
    let app = row.app_id.clone();
    Ok(PendingGitDeployRequest {
        ref_name: row.ref_name,
        sha: row.sha,
        force: row.force,
        request_id: row.request_id,
        interrupt_requested_at: row
            .interrupt_requested_at
            .map(|value| sqlite_i64_to_u64(value, &app, "interrupt_requested_at"))
            .transpose()?,
    })
}

fn write_pending_git_request(app: &str, request: &PendingGitDeployRequest) -> Result<(), String> {
    sqlite_store::upsert_pending_git_request(&sqlite_store::PendingGitRequestRow {
        app_id: app.to_string(),
        ref_name: request.ref_name.clone(),
        sha: request.sha.clone(),
        force: request.force,
        request_id: request.request_id.clone(),
        interrupt_requested_at: request
            .interrupt_requested_at
            .map(|value| sqlite_u64_to_i64(value, app, "interrupt_requested_at"))
            .transpose()?,
    })
}

fn read_pending_git_request(app: &str) -> Result<Option<PendingGitDeployRequest>, String> {
    sqlite_store::get_pending_git_request(app)?
        .map(pending_git_request_from_row)
        .transpose()
}

fn take_pending_git_request(app: &str) -> Result<Option<PendingGitDeployRequest>, String> {
    sqlite_store::take_pending_git_request(app)?
        .map(pending_git_request_from_row)
        .transpose()
}

fn deploy_interrupt_from_row(
    row: sqlite_store::DeployInterruptRow,
) -> Result<DeployInterruptState, String> {
    Ok(DeployInterruptState {
        request_id: row.request_id,
        requested_at: sqlite_i64_to_u64(row.requested_at, &row.app_id, "requested_at")?,
        target_sha: row.target_sha,
    })
}

fn read_deploy_interrupt(app: &str) -> Result<Option<DeployInterruptState>, String> {
    sqlite_store::get_deploy_interrupt(app)?
        .map(deploy_interrupt_from_row)
        .transpose()
}

fn request_deploy_interrupt(app: &str, state: &DeployInterruptState) -> Result<(), String> {
    sqlite_store::upsert_deploy_interrupt(&sqlite_store::DeployInterruptRow {
        app_id: app.to_string(),
        request_id: state.request_id.clone(),
        requested_at: sqlite_u64_to_i64(state.requested_at, app, "requested_at")?,
        target_sha: state.target_sha.clone(),
    })
}

fn clear_deploy_interrupt(app: &str) -> Result<(), String> {
    sqlite_store::delete_deploy_interrupt(app)
}

fn cleanup_job_from_row(row: sqlite_store::CleanupJobRow) -> Result<CleanupJobState, String> {
    Ok(CleanupJobState {
        app: row.app_id.clone(),
        active_instance_at_schedule: row.active_instance_at_schedule,
        scheduled_previous_instance: row.scheduled_previous_instance,
        attempts: sqlite_i64_to_u32(row.attempts, &row.app_id, "attempts")?,
        last_error: row.last_error,
        scheduled_at: sqlite_i64_to_u64(row.scheduled_at, &row.app_id, "scheduled_at")?,
        updated_at: sqlite_i64_to_u64(row.updated_at, &row.app_id, "updated_at")?,
    })
}

fn read_cleanup_job(app: &str) -> Result<Option<CleanupJobState>, String> {
    sqlite_store::get_cleanup_job(app)?
        .map(cleanup_job_from_row)
        .transpose()
}

fn write_cleanup_job(app: &str, state: &CleanupJobState) -> Result<(), String> {
    sqlite_store::upsert_cleanup_job(&sqlite_store::CleanupJobRow {
        app_id: app.to_string(),
        active_instance_at_schedule: state.active_instance_at_schedule.clone(),
        scheduled_previous_instance: state.scheduled_previous_instance.clone(),
        attempts: i64::from(state.attempts),
        last_error: state.last_error.clone(),
        scheduled_at: sqlite_u64_to_i64(state.scheduled_at, app, "scheduled_at")?,
        updated_at: sqlite_u64_to_i64(state.updated_at, app, "updated_at")?,
    })
}

fn clear_cleanup_job(app: &str) -> Result<(), String> {
    sqlite_store::delete_cleanup_job(app)
}

fn current_project_name() -> Result<String, String> {
    Ok(runtime_graph::RuntimeProject::current()?.name)
}

fn git_target_already_succeeded_with_state(
    state: Option<&GitDeployState>,
    target: &GitCheckoutTarget,
) -> bool {
    matches!(
        state,
        Some(GitDeployState {
            sha,
            status: GitDeployStatus::Success,
            ..
        }) if sha == &target.sha
    )
}

fn git_target_already_succeeded(app: &str, target: &GitCheckoutTarget) -> Result<bool, String> {
    let state = read_git_deploy_state(app)?;
    Ok(git_target_already_succeeded_with_state(
        state.as_ref(),
        target,
    ))
}

fn parse_git_checkout_target(
    git_ref: Option<&str>,
    git_sha: Option<&str>,
) -> Result<Option<GitCheckoutTarget>, String> {
    match (git_ref, git_sha) {
        (None, None) => Ok(None),
        (Some(ref_name), Some(sha)) => {
            let ref_name = ref_name.trim();
            let sha = sha.trim();
            if ref_name.is_empty() {
                return Err("deploy ref is empty".to_string());
            }
            if sha.is_empty() {
                return Err("deploy sha is empty".to_string());
            }
            Ok(Some(GitCheckoutTarget {
                ref_name: ref_name.to_string(),
                sha: sha.to_string(),
            }))
        }
        _ => Err("deploy requires both --ref and --sha".to_string()),
    }
}

fn checkout_ref_target(ref_name: &str) -> String {
    if let Some(branch) = ref_name.strip_prefix("refs/heads/") {
        return format!("origin/{branch}");
    }
    ref_name.to_string()
}

fn checkout_code_in(
    repo_dir: &Path,
    build_dir: &Path,
    target: Option<&GitCheckoutTarget>,
) -> Result<(), String> {
    if build_dir.exists() {
        fs::remove_dir_all(build_dir).map_err(|e| format!("failed to clean build dir: {e}"))?;
    }
    fs::create_dir_all(build_dir).map_err(|e| format!("failed to create build dir: {e}"))?;

    match target {
        None => {
            let status = git_command()
                .args(["clone", "--depth", "1"])
                .arg(repo_dir)
                .arg(build_dir)
                .status()
                .map_err(|e| format!("failed to checkout code: {e}"))?;
            if !status.success() {
                return Err("git clone failed".to_string());
            }
        }
        Some(target) => {
            let status = git_command()
                .args(["clone", "--no-checkout"])
                .arg(repo_dir)
                .arg(build_dir)
                .status()
                .map_err(|e| format!("failed to checkout code: {e}"))?;
            if !status.success() {
                return Err("git clone failed".to_string());
            }

            let checkout_target = checkout_ref_target(&target.ref_name);
            run_git_in(&["checkout", "--detach", &checkout_target], build_dir)
                .map_err(|e| format!("failed to checkout {}: {e}", target.ref_name))?;

            let object_type = run_git_capture_in(&["cat-file", "-t", &target.sha], build_dir)
                .map_err(|e| format!("failed to resolve pushed object {}: {e}", target.sha))?;
            if object_type == "commit" {
                let head = run_git_capture_in(&["rev-parse", "HEAD"], build_dir)?;
                if head != target.sha {
                    return Err(format!(
                        "checked out commit {head} does not match pushed commit {}",
                        target.sha
                    ));
                }
            } else if object_type == "tag" {
                let resolved = run_git_capture_in(&["rev-parse", &target.ref_name], build_dir)
                    .map_err(|e| format!("failed to resolve {}: {e}", target.ref_name))?;
                if resolved != target.sha {
                    return Err(format!(
                        "checked out ref {} resolved to {resolved}, expected {}",
                        target.ref_name, target.sha
                    ));
                }
            }
        }
    }

    Ok(())
}

fn checkout_code(app: &str, target: Option<&GitCheckoutTarget>) -> Result<PathBuf, String> {
    let build_dir = builds_dir().join(app);
    let repo_dir = repos_dir().join(format!("{app}.git"));
    checkout_code_in(&repo_dir, &build_dir, target)?;
    Ok(build_dir)
}

fn allocate_port(app: &str) -> u16 {
    // Simple deterministic port allocation based on app name hash
    let hash: u32 = app
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    3001 + (hash % 1000) as u16
}

fn start_exports(port: u16, vars: &BTreeMap<String, String>) -> Result<String, String> {
    let mut parts = vec![format!("export PORT={port}")];
    for (name, value) in vars {
        if !is_valid_env_name(name) {
            return Err(format!("invalid env name '{name}'"));
        }
        parts.push(format!("export {name}={}", shell_quote(value)));
    }
    Ok(parts.join(" && "))
}

fn app_runner_script_content(
    port: u16,
    cmd: &str,
    vars: &BTreeMap<String, String>,
) -> Result<String, String> {
    let escaped = shell_quote(cmd);
    let exports = start_exports(port, vars)?;
    Ok(format!(
        "#!/bin/sh\nset -eu\nmkdir -p /var/psht\ncd /app\n{exports}\nexec sh -c {escaped}\n"
    ))
}

fn write_app_runner_cmd(
    port: u16,
    cmd: &str,
    vars: &BTreeMap<String, String>,
) -> Result<String, String> {
    let script = app_runner_script_content(port, cmd, vars)?;
    let escaped = shell_quote(&script);
    Ok(format!(
        "mkdir -p /usr/local/bin && printf '%s' {escaped} > {APP_SERVICE_RUNNER_PATH} && chmod 755 {APP_SERVICE_RUNNER_PATH}"
    ))
}

fn app_service_unit_content() -> String {
    format!(
        "[Unit]\nDescription=psht application process\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nWorkingDirectory=/app\nExecStart={APP_SERVICE_RUNNER_PATH}\nRestart=always\nRestartSec=2\nKillMode=control-group\nStandardOutput=append:{APP_PROCESS_LOG_PATH}\nStandardError=append:{APP_PROCESS_LOG_PATH}\n\n[Install]\nWantedBy=multi-user.target\n"
    )
}

fn write_app_service_unit_cmd() -> String {
    let escaped = shell_quote(&app_service_unit_content());
    format!(
        "mkdir -p /etc/systemd/system /var/psht && printf '%s' {escaped} > {APP_SERVICE_UNIT_PATH} && chmod 644 {APP_SERVICE_UNIT_PATH}"
    )
}

fn ensure_app_service_installed(
    app: &str,
    port: u16,
    cmd: &str,
    vars: &BTreeMap<String, String>,
) -> Result<(), String> {
    persist_ssh_login_env(app, vars)?;
    container::exec_cmd(app, &write_app_runner_cmd(port, cmd, vars)?)?;
    container::exec_cmd(app, &write_app_service_unit_cmd())?;
    container::exec_cmd(app, "systemctl daemon-reload")?;
    container::exec_cmd(app, &format!("systemctl enable {APP_SERVICE_NAME}"))?;
    Ok(())
}

fn app_service_is_active(app: &str) -> Result<bool, String> {
    let output = container::exec_output(
        app,
        &format!("if systemctl is-active --quiet {APP_SERVICE_NAME}; then echo active; fi; true"),
    )?;
    Ok(output.trim() == "active")
}

fn app_process_probe_cmd() -> String {
    format!(
        r#"if [ ! -s {APP_PROCESS_PID_PATH} ]; then exit 0; fi
pid="$(cat {APP_PROCESS_PID_PATH} 2>/dev/null | tr -d '[:space:]')"
case "$pid" in
  ''|*[!0-9]*) exit 0 ;;
esac
if ! kill -0 "$pid" 2>/dev/null; then
  exit 0
fi
mtime="$(stat -c %Y {APP_PROCESS_PID_PATH} 2>/dev/null || true)"
uptime="$(cut -d. -f1 /proc/uptime 2>/dev/null || true)"
now="$(date +%s 2>/dev/null || true)"
if [ -n "$mtime" ] && [ -n "$uptime" ] && [ -n "$now" ]; then
  case "$mtime:$uptime:$now" in
    *[!0-9:]*)
      ;;
    *)
      boot="$((now - uptime))"
      if [ "$mtime" -lt "$boot" ]; then
        exit 0
      fi
      ;;
  esac
fi
echo alive
true"#
    )
}

fn app_process_is_running(app: &str) -> Result<bool, String> {
    if app_service_is_active(app)? {
        return Ok(true);
    }
    let output = container::exec_output(app, &app_process_probe_cmd())?;
    Ok(output.trim() == "alive")
}

fn stop_app_process_cmd_with_limits(
    term_checks: u32,
    kill_checks: u32,
    poll_sleep: &str,
) -> String {
    format!(
        r#"if [ ! -s {APP_PROCESS_PID_PATH} ]; then rm -f {APP_PROCESS_PID_PATH}; exit 0; fi
pid="$(cat {APP_PROCESS_PID_PATH} 2>/dev/null | tr -d '[:space:]')"
case "$pid" in
  ''|*[!0-9]*) rm -f {APP_PROCESS_PID_PATH}; exit 0 ;;
esac
kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
i=0
while kill -0 "$pid" 2>/dev/null; do
  i=$((i + 1))
  if [ "$i" -ge {term_checks} ]; then
    kill -KILL -- "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
    break
  fi
  sleep {poll_sleep}
done
j=0
while kill -0 "$pid" 2>/dev/null; do
  j=$((j + 1))
  if [ "$j" -ge {kill_checks} ]; then
    echo "app process $pid did not exit after SIGTERM+SIGKILL" >&2
    exit 1
  fi
  sleep {poll_sleep}
done
rm -f {APP_PROCESS_PID_PATH}"#
    )
}

fn stop_app_process_cmd() -> String {
    stop_app_process_cmd_with_limits(
        APP_PROCESS_STOP_TERM_CHECKS,
        APP_PROCESS_STOP_KILL_CHECKS,
        APP_PROCESS_POLL_SLEEP,
    )
}

fn stop_port_listeners_cmd_with_limits(
    port: u16,
    term_checks: u32,
    kill_checks: u32,
    poll_sleep: &str,
) -> String {
    format!(
        r#"if ! command -v ss >/dev/null 2>&1; then exit 0; fi
pids="$(ss -ltnp "sport = :{port}" 2>/dev/null | sed -n 's/.*pid=\([0-9][0-9]*\).*/\1/p' | sort -u)"
[ -z "$pids" ] && exit 0
for pid in $pids; do
  kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
done
i=0
while :; do
  remaining=""
  for pid in $pids; do
    if kill -0 "$pid" 2>/dev/null; then
      remaining="$remaining $pid"
    fi
  done
  [ -z "$remaining" ] && break
  i=$((i + 1))
  if [ "$i" -ge {term_checks} ]; then
    for pid in $remaining; do
      kill -KILL -- "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
    done
    break
  fi
  sleep {poll_sleep}
done
j=0
while :; do
  remaining=""
  for pid in $pids; do
    if kill -0 "$pid" 2>/dev/null; then
      remaining="$remaining $pid"
    fi
  done
  [ -z "$remaining" ] && break
  j=$((j + 1))
  if [ "$j" -ge {kill_checks} ]; then
    echo "listener process(es) on port {port} did not exit:$remaining" >&2
    exit 1
  fi
  sleep {poll_sleep}
done"#
    )
}

fn stop_port_listeners_cmd(port: u16) -> String {
    stop_port_listeners_cmd_with_limits(
        port,
        APP_PROCESS_STOP_TERM_CHECKS,
        APP_PROCESS_STOP_KILL_CHECKS,
        APP_PROCESS_POLL_SLEEP,
    )
}

fn stop_app_process_on_port(app: &str, port: u16) -> Result<(), String> {
    container::exec_cmd(
        app,
        &format!("systemctl stop {APP_SERVICE_NAME} >/dev/null 2>&1 || true"),
    )?;
    container::exec_cmd(app, &stop_app_process_cmd())?;
    container::exec_cmd(app, &stop_port_listeners_cmd(port))
}

fn app_log_tail(app: &str, lines: u32) -> Option<String> {
    let output = container::exec_output(
        app,
        &format!("if [ -f {APP_PROCESS_LOG_PATH} ]; then tail -n {lines} {APP_PROCESS_LOG_PATH} 2>/dev/null || true; fi"),
    )
    .ok()?;
    let trimmed = output.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn launch_app_process(
    app: &str,
    port: u16,
    cmd: &str,
    vars: &BTreeMap<String, String>,
) -> Result<(), String> {
    ensure_app_service_installed(app, port, cmd, vars)?;
    stop_app_process_on_port(app, port)?;
    container::exec_cmd(app, &format!("systemctl restart {APP_SERVICE_NAME}"))?;
    for _ in 0..APP_PROCESS_START_WAIT_CHECKS {
        if app_service_is_active(app)? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
    let mut message = format!(
        "app process failed to stay up after launch (start command: {}); check logs with: psht logs {app}",
        cmd.trim()
    );
    if let Some(log_excerpt) = app_log_tail(app, APP_LOG_TAIL_LINES) {
        message.push_str("\nLast app log lines:\n");
        message.push_str(&log_excerpt);
    }
    Err(message)
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

fn write_start_command_cmd(cmd: &str) -> Result<String, String> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return Err("start command is empty".to_string());
    }
    let escaped = shell_quote(cmd);
    Ok(format!(
        "mkdir -p /etc && printf '%s\\n' {escaped} > {START_COMMAND_PATH}"
    ))
}

fn persist_start_command(app: &str, cmd: &str) -> Result<(), String> {
    let command = write_start_command_cmd(cmd)?;
    container::exec_cmd(app, &command)
}

fn read_start_command(app: &str) -> Result<String, String> {
    let cmd = container::exec_output(
        app,
        &format!("cat {START_COMMAND_PATH} 2>/dev/null || true"),
    )?;
    let cmd = cmd.trim().to_string();
    if cmd.is_empty() {
        return Err(format!(
            "missing start command metadata at {START_COMMAND_PATH}; redeploy app '{app}'"
        ));
    }
    Ok(cmd)
}

fn app_workdir_command(command: &str) -> Option<String> {
    let command = command.trim();
    if command.is_empty() {
        None
    } else {
        Some(format!("cd /app && {command}"))
    }
}

fn install_log_tail(app: &str, lines: u32) -> Option<String> {
    let output = container::exec_output(
        app,
        &format!(
            "if [ -f {INSTALL_LOG_PATH} ]; then tail -n {lines} {INSTALL_LOG_PATH} 2>/dev/null || true; fi"
        ),
    )
    .ok()?;
    let trimmed = output.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn logged_command_wrapper(command: &str, log_path: &str) -> String {
    let quoted_log_path = shell_quote(log_path);
    format!(
        r#"log_path={quoted_log_path}
log_dir="$(dirname "$log_path")"
mkdir -p "$log_dir"
status_file="$(mktemp)"
(
trap 'status=$?; printf "%s\n" "$status" > "$status_file"' EXIT
{command}
) 2>&1 | tee "$log_path"
status="$(tr -d '[:space:]' < "$status_file" 2>/dev/null || true)"
rm -f "$status_file"
case "$status" in
  ''|*[!0-9]*) status=1 ;;
esac
exit "$status""#,
        quoted_log_path = quoted_log_path
    )
}

fn run_install_command_with_logging(app: &str, command: &str, label: &str) -> Result<(), String> {
    let wrapped = logged_command_wrapper(command, INSTALL_LOG_PATH);
    container::exec_cmd_rolling(app, &wrapped, 5).map_err(|e| {
        let mut message = format!("{label} failed: {e}");
        if let Some(log_excerpt) = install_log_tail(app, INSTALL_LOG_TAIL_LINES) {
            message.push_str("\nLast install/build log lines:\n");
            message.push_str(&log_excerpt);
        }
        if is_resource_pressure_error(&message)
            && let Ok(project) = current_project_name()
        {
            emit_resource_diagnostics(app, app, &project, label);
        }
        message
    })
}

fn setup_log_tail(app: &str, lines: u32) -> Option<String> {
    let output = container::exec_output(
        app,
        &format!(
            "if [ -f {SETUP_LOG_PATH} ]; then tail -n {lines} {SETUP_LOG_PATH} 2>/dev/null || true; fi"
        ),
    )
    .ok()?;
    let trimmed = output.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupFailureClass {
    Transient,
    NonRetryable,
}

fn classify_setup_failure(err: &str) -> SetupFailureClass {
    let lowered = err.to_ascii_lowercase();
    if is_resource_pressure_error(&lowered)
        || [
            "temporary failure resolving",
            "failed to fetch",
            "could not connect",
            "connection timed out",
            "connection reset by peer",
            "dpkg frontend lock",
            "could not get lock",
            "hash sum mismatch",
        ]
        .iter()
        .any(|needle| lowered.contains(needle))
    {
        SetupFailureClass::Transient
    } else {
        SetupFailureClass::NonRetryable
    }
}

fn run_setup_command_with_logging(app: &str, command: &str, label: &str) -> Result<(), String> {
    let wrapped = logged_command_wrapper(command, SETUP_LOG_PATH);
    container::exec_cmd_rolling(app, &wrapped, 5).map_err(|e| {
        let mut message = format!("{label} failed: {e}");
        if let Some(log_excerpt) = setup_log_tail(app, SETUP_LOG_TAIL_LINES) {
            message.push_str("\nLast setup log lines:\n");
            message.push_str(&log_excerpt);
        }
        if let Ok(project) = current_project_name() {
            emit_resource_diagnostics(app, app, &project, label);
        }
        match classify_setup_failure(&message) {
            SetupFailureClass::Transient => mark_setup_transient_failure(message),
            SetupFailureClass::NonRetryable => mark_setup_nonretryable_failure(message),
        }
    })
}

fn run_hook(app: &str, phase: &str, command: Option<&str>) -> Result<(), String> {
    let Some(command) = command.and_then(app_workdir_command) else {
        return Ok(());
    };
    eprintln!("-----> Running {phase} hook");
    container::exec_cmd_rolling(app, &command, 5).map_err(|e| format!("{phase} hook failed: {e}"))
}

fn normalized_apt_packages(packages: &[String]) -> Vec<String> {
    packages
        .iter()
        .map(|pkg| pkg.trim())
        .filter(|pkg| !pkg.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn apt_install_command(packages: &[String]) -> Option<String> {
    let packages = normalized_apt_packages(packages);
    if packages.is_empty() {
        return None;
    }
    let quoted = packages
        .iter()
        .map(|pkg| shell_quote(pkg))
        .collect::<Vec<_>>()
        .join(" ");
    Some(format!(
        r#"export DEBIAN_FRONTEND=noninteractive
attempt=1
status=1
max_attempts={APT_INSTALL_MAX_ATTEMPTS}
while [ "$attempt" -le "$max_attempts" ]; do
  echo "apt attempt ${{attempt}}/${{max_attempts}}"
  if apt-get update && apt-get install -y {quoted}; then
    status=0
    break
  fi
  status="$?"
  if [ "$attempt" -ge "$max_attempts" ]; then
    break
  fi
  sleep "$((attempt * {APT_INSTALL_RETRY_SLEEP_SECS}))"
  attempt="$((attempt + 1))"
done
exit "$status""#
    ))
}

fn extract_missing_apt_packages(text: &str) -> Vec<String> {
    let mut missing = Vec::new();
    let mut seen = BTreeSet::new();

    for line in text.lines().map(str::trim) {
        let candidate = if let Some(pkg) = line
            .strip_prefix("E: Unable to locate package ")
            .or_else(|| line.strip_prefix("N: Unable to locate package "))
        {
            Some(pkg.trim())
        } else if let Some(rest) = line.strip_prefix("E: Package ") {
            if let Some(pkg) = rest
                .strip_prefix('\'')
                .and_then(|value| value.split_once('\''))
                .map(|(pkg, _)| pkg.trim())
            {
                Some(pkg)
            } else {
                rest.strip_suffix(" has no installation candidate")
                    .map(str::trim)
            }
        } else {
            line.strip_prefix("Package ")
                .and_then(|rest| {
                    rest.strip_suffix(" is not available, but is referred to by another package.")
                })
                .map(str::trim)
        };

        let Some(pkg) = candidate else {
            continue;
        };
        if pkg.is_empty() || !seen.insert(pkg.to_string()) {
            continue;
        }
        missing.push(pkg.to_string());
    }

    missing
}

fn apt_policy_output_indicates_unavailable(policy_output: &str) -> bool {
    let lowered = policy_output.to_ascii_lowercase();
    lowered.contains("unable to locate package") || lowered.contains("candidate: (none)")
}

fn diagnose_unavailable_apt_packages(app: &str, packages: &[String]) -> Vec<String> {
    let mut missing = Vec::new();
    let mut seen = BTreeSet::new();

    for package in normalized_apt_packages(packages) {
        let command = format!("apt-cache policy {} 2>&1 || true", shell_quote(&package));
        let Ok(output) = container::exec_output(app, &command) else {
            continue;
        };
        if !apt_policy_output_indicates_unavailable(&output) {
            continue;
        }
        if seen.insert(package.clone()) {
            missing.push(package);
        }
    }

    missing
}

fn install_apt_packages(app: &str, packages: &[String]) -> Result<(), String> {
    let Some(command) = apt_install_command(packages) else {
        return Ok(());
    };
    eprintln!("-----> Installing apt packages");
    match run_install_command_with_logging(app, &command, "apt package install") {
        Ok(()) => Ok(()),
        Err(err) => {
            let mut unavailable = extract_missing_apt_packages(&err);
            if unavailable.is_empty() {
                unavailable = diagnose_unavailable_apt_packages(app, packages);
            }

            let mut message = err;
            if !unavailable.is_empty() {
                message.push_str("\nUnavailable apt package(s) in the current base image: ");
                message.push_str(&unavailable.join(", "));
            }
            Err(message)
        }
    }
}

fn blocking_op_age_secs(seen_at: &HashMap<String, Instant>, op_id: &str) -> u64 {
    seen_at
        .get(op_id)
        .map(|seen| seen.elapsed().as_secs())
        .unwrap_or(0)
}

fn blocking_ops_json(
    blocking: &[container::BlockingOperation],
    seen_at: &HashMap<String, Instant>,
) -> Vec<serde_json::Value> {
    blocking
        .iter()
        .map(|op| {
            serde_json::json!({
                "id": op.id,
                "status": op.status,
                "status_code": op.status_code,
                "class": op.class,
                "description": op.description,
                "may_cancel": op.may_cancel,
                "age_secs": blocking_op_age_secs(seen_at, &op.id),
                "resources": op.resources,
                "instance_names": op.instance_names,
            })
        })
        .collect()
}

fn blocking_ops_summary(
    blocking: &[container::BlockingOperation],
    seen_at: &HashMap<String, Instant>,
) -> String {
    blocking
        .iter()
        .map(|op| {
            format!(
                "{}(class={},age={}s,cancelable={},targets={})",
                op.id,
                op.class,
                blocking_op_age_secs(seen_at, &op.id),
                op.may_cancel,
                if op.instance_names.is_empty() {
                    "[]".to_string()
                } else {
                    op.instance_names.join("|")
                }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn target_is_currently_serving(target: &str, deploy_app: Option<&str>) -> bool {
    let Some(deploy_app) = deploy_app else {
        return false;
    };
    let Some(active_app_ref) = resolve_active_app_ref(deploy_app).ok().flatten() else {
        return false;
    };
    if active_app_ref == target {
        return true;
    }
    if instance_name_from_app_ref(&active_app_ref) == target {
        return true;
    }
    app_ref_from_instance_name(target)
        .map(|target_ref| target_ref == active_app_ref)
        .unwrap_or(false)
}

fn blocking_op_targets(op: &container::BlockingOperation, fallback_app_ref: &str) -> Vec<String> {
    if !op.instance_names.is_empty() {
        return op.instance_names.clone();
    }
    vec![instance_name_from_app_ref(fallback_app_ref)]
}

fn blocked_operation_error(
    app: &str,
    project: &str,
    budget_secs: u64,
    phase: &str,
    blocking: &[container::BlockingOperation],
    seen_at: &HashMap<String, Instant>,
    attempted_cancel_ids: &BTreeSet<String>,
    forced_actions: &[String],
    skipped_actions: &[String],
) -> String {
    let mut lines = vec![format!(
        "container '{app}' remained blocked after {budget_secs}s (phase={phase}); unresolved operations: {}",
        blocking_ops_summary(blocking, seen_at)
    )];
    if !attempted_cancel_ids.is_empty() {
        lines.push(format!(
            "automatic actions: attempted operation cancellation for {} op(s): {}",
            attempted_cancel_ids.len(),
            attempted_cancel_ids
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else {
        lines.push("automatic actions: no cancelable operations were found".to_string());
    }
    if forced_actions.is_empty() {
        lines
            .push("automatic actions: no forced target-instance resets were performed".to_string());
    } else {
        lines.push(format!(
            "automatic actions: forced target-instance resets: {}",
            forced_actions.join(" | ")
        ));
    }
    if !skipped_actions.is_empty() {
        lines.push(format!(
            "automatic actions: skipped actions: {}",
            skipped_actions.join(" | ")
        ));
    }
    lines.push(format!("inspect: incus --project {project} operation list"));
    for op in blocking {
        lines.push(format!(
            "inspect op {}: incus --project {project} operation show {}",
            op.id, op.id
        ));
    }
    lines.join("\n")
}

fn mark_setup_transient_failure(err: String) -> String {
    format!("{DEPLOY_ERR_SETUP_TRANSIENT_MARKER}{err}")
}

fn mark_setup_nonretryable_failure(err: String) -> String {
    format!("{DEPLOY_ERR_SETUP_NONRETRYABLE_MARKER}{err}")
}

fn strip_internal_deploy_error_markers(err: &str) -> &str {
    err.strip_prefix(DEPLOY_ERR_CACHED_SETUP_FAILURE_MARKER)
        .or_else(|| err.strip_prefix(DEPLOY_ERR_FRESH_SETUP_FAILURE_MARKER))
        .or_else(|| err.strip_prefix(DEPLOY_ERR_SETUP_TRANSIENT_MARKER))
        .or_else(|| err.strip_prefix(DEPLOY_ERR_SETUP_NONRETRYABLE_MARKER))
        .unwrap_or(err)
}

fn is_resource_pressure_error(err: &str) -> bool {
    let lowered = err.to_ascii_lowercase();
    [
        "resource temporarily unavailable",
        "fork failed",
        "cannot allocate memory",
        "out of memory",
        "pthread_create",
        "eagain",
        "enomem",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

fn run_diagnostic_shell(command: &str, timeout_secs: u64) -> String {
    match run_cmd_capture_with_timeout("sh", &["-c", command], timeout_secs) {
        Ok(output) if output.trim().is_empty() => "(no output)".to_string(),
        Ok(output) => output,
        Err(err) => format!("(unavailable: {err})"),
    }
}

fn append_diagnostic_section(lines: &mut Vec<String>, label: &str, command: &str) {
    lines.push(format!("## {label}"));
    lines.push(format!("$ {command}"));
    lines.push(run_diagnostic_shell(
        command,
        RESOURCE_DIAGNOSTIC_TIMEOUT_SECS,
    ));
    lines.push(String::new());
}

fn collect_resource_diagnostics(project: Option<&str>, candidate_app: Option<&str>) -> String {
    let mut lines = Vec::new();
    lines.push("# Resource Diagnostics Snapshot".to_string());
    lines.push(format!(
        "timestamp_utc: {}",
        run_diagnostic_shell(
            "date -u +%Y-%m-%dT%H:%M:%SZ",
            RESOURCE_DIAGNOSTIC_TIMEOUT_SECS
        )
    ));
    lines.push(String::new());

    append_diagnostic_section(&mut lines, "Host Uptime", "uptime || true");
    append_diagnostic_section(&mut lines, "Host Memory", "free -m || true");
    append_diagnostic_section(
        &mut lines,
        "Host PID Max",
        "cat /proc/sys/kernel/pid_max 2>/dev/null || true",
    );
    append_diagnostic_section(
        &mut lines,
        "Host Task Count",
        "ps -eLf 2>/dev/null | wc -l || true",
    );
    append_diagnostic_section(
        &mut lines,
        "Incus Running Build Containers",
        "incus list --format csv -c ns 2>/dev/null | awk -F, '/^psht-.*-build-[0-9]+,RUNNING$/ {count++} END {print count+0}' || true",
    );
    append_diagnostic_section(
        &mut lines,
        "Kernel Pressure Signals",
        "dmesg -T 2>/dev/null | egrep -i 'oom|out of memory|fork|cgroup|pids' | tail -n 50 || true",
    );

    if let Some(project) = project {
        let escaped_project = shell_quote(project);
        append_diagnostic_section(
            &mut lines,
            "Project Default Profile",
            &format!("incus --project {escaped_project} profile show default 2>/dev/null || true"),
        );
    }

    if let Some(candidate_app) = candidate_app {
        let project = project.unwrap_or("default");
        let instance = instance_name_from_app_ref(candidate_app);
        let escaped_project = shell_quote(project);
        let escaped_instance = shell_quote(&instance);
        append_diagnostic_section(
            &mut lines,
            "Candidate Expanded Config",
            &format!(
                "incus --project {escaped_project} config show {escaped_instance} --expanded 2>/dev/null || true"
            ),
        );
        append_diagnostic_section(
            &mut lines,
            "Candidate Cgroup Limits",
            &format!(
                "incus --project {escaped_project} exec {escaped_instance} -- sh -c {} 2>/dev/null || true",
                shell_quote(
                    "ulimit -a
echo '---'
cat /sys/fs/cgroup/pids.max 2>/dev/null || true
cat /sys/fs/cgroup/pids.current 2>/dev/null || true
cat /sys/fs/cgroup/memory.max 2>/dev/null || true
cat /sys/fs/cgroup/memory.current 2>/dev/null || true
ps -eLf 2>/dev/null | wc -l || true"
                )
            ),
        );
    }

    lines.join("\n")
}

fn emit_resource_diagnostics(app: &str, candidate_app: &str, project: &str, context: &str) {
    let candidate_instance = instance_name_from_app_ref(candidate_app);
    eprintln!("-----> Resource diagnostics captured ({context})");
    eprintln!("       App: {app}");
    eprintln!("       Candidate: {candidate_instance}");
    eprintln!("       Project: {project}");
    let snapshot = collect_resource_diagnostics(Some(project), Some(candidate_app));
    eprintln!("{snapshot}");
}

fn wait_for_container_operation_quiet(
    app: &str,
    project: &str,
    deploy_app: Option<&str>,
) -> Result<(), String> {
    let policy = busy_op_policy();
    let initial_wait_window =
        Duration::from_millis(CONTAINER_OP_INITIAL_WAIT_CHECKS as u64 * CONTAINER_OP_WAIT_SLEEP_MS);
    let recheck_window =
        Duration::from_millis(CONTAINER_OP_RECHECK_WAIT_CHECKS as u64 * CONTAINER_OP_WAIT_SLEEP_MS);
    let wait_sleep = Duration::from_millis(CONTAINER_OP_WAIT_SLEEP_MS);
    let reconcile_budget = Duration::from_secs(blocked_op_budget_secs());
    let takeover_sleep = Duration::from_millis(takeover_retry_ms().max(CONTAINER_OP_WAIT_SLEEP_MS));
    let max_cancel_per_cycle = takeover_max_cancel_per_cycle();
    let started = Instant::now();
    let mut seen_at: HashMap<String, Instant> = HashMap::new();
    let mut canceled_ids: BTreeMap<String, usize> = BTreeMap::new();
    let mut attempted_cancel_ids: BTreeSet<String> = BTreeSet::new();
    let mut attempted_force_targets: BTreeSet<String> = BTreeSet::new();
    let mut forced_actions = Vec::new();
    let mut skipped_actions = Vec::new();
    let mut announced_wait = false;
    let mut last_heartbeat = Duration::from_secs(0);
    let mut phase = "waiting";
    loop {
        if let Some(deploy_app) = deploy_app {
            check_deploy_interrupt(deploy_app, "waiting for active container operation")?;
        }
        let blocking = container::list_blocking_operations_in_project(app, project)?;
        if blocking.is_empty() {
            if announced_wait {
                eprintln!("       Active operation finished");
            }
            if announced_wait {
                update_reconcile_phase_from_lease(app, "Reconciling", None);
            }
            return Ok(());
        }

        let now = Instant::now();
        for op in &blocking {
            seen_at.entry(op.id.clone()).or_insert(now);
        }
        if !announced_wait {
            eprintln!("       Waiting for active container operation to finish...");
            announced_wait = true;
        }
        let elapsed = started.elapsed();
        if elapsed
            >= last_heartbeat.saturating_add(Duration::from_secs(CONTAINER_OP_HEARTBEAT_SECS))
        {
            let ids = blocking
                .iter()
                .map(|op| op.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            eprintln!(
                "       Blocked ({phase}, {}s/{}s): {ids}",
                elapsed.as_secs(),
                reconcile_budget.as_secs()
            );
            last_heartbeat = elapsed;
        }

        update_reconcile_phase_from_lease(
            app,
            "Blocked",
            Some("container has active blocking operation(s)"),
        );
        append_reconcile_attempt_record(
            app,
            "wait-for-container-operation",
            "blocked",
            serde_json::json!({
                "policy": format!("{policy:?}").to_ascii_lowercase(),
                "phase": phase,
                "blocking_ops": blocking_ops_json(&blocking, &seen_at),
            }),
        );

        if elapsed < initial_wait_window {
            phase = "waiting";
            thread::sleep(wait_sleep);
            continue;
        }

        if policy == BusyOpPolicy::Diagnose {
            return Err(blocked_operation_error(
                app,
                project,
                reconcile_budget.as_secs(),
                phase,
                &blocking,
                &seen_at,
                &attempted_cancel_ids,
                &forced_actions,
                &skipped_actions,
            ));
        }

        let mut canceled_this_cycle = Vec::new();
        phase = "canceling";
        for op in &blocking {
            if canceled_this_cycle.len() >= max_cancel_per_cycle {
                break;
            }
            if !op.may_cancel {
                continue;
            }
            if !attempted_cancel_ids.insert(op.id.clone()) {
                continue;
            }
            if let Err(err) = container::cancel_operation_in_project(project, &op.id) {
                eprintln!(
                    "       Warning: failed to cancel blocking operation {}: {err}",
                    op.id
                );
            } else {
                let count = canceled_ids.entry(op.id.clone()).or_insert(0);
                *count = count.saturating_add(1);
                canceled_this_cycle.push(op.id.clone());
            }
        }

        if !canceled_this_cycle.is_empty() {
            eprintln!(
                "       Requested cancellation for {} blocking operation(s): {}",
                canceled_this_cycle.len(),
                canceled_this_cycle.join(", ")
            );
            append_reconcile_attempt_record(
                app,
                "wait-for-container-operation",
                "takeover",
                serde_json::json!({
                    "policy": format!("{policy:?}").to_ascii_lowercase(),
                    "canceled_ops": canceled_this_cycle,
                }),
            );
            update_reconcile_phase_from_lease(app, "Reconciling", None);
            thread::sleep(takeover_sleep);
            continue;
        }

        if elapsed < initial_wait_window.saturating_add(recheck_window) {
            phase = "rechecking";
            thread::sleep(wait_sleep);
            continue;
        }

        phase = "force-reset";
        let mut forced_this_cycle = Vec::new();
        for op in &blocking {
            if op.may_cancel {
                continue;
            }
            let targets = blocking_op_targets(op, app);
            if op.instance_names.is_empty() {
                skipped_actions.push(format!(
                    "op {} had no parsed target; using fallback target {}",
                    op.id, targets[0]
                ));
            }
            for target_instance in targets {
                if !attempted_force_targets.insert(target_instance.clone()) {
                    continue;
                }
                if target_is_currently_serving(&target_instance, deploy_app) {
                    let reason = format!(
                        "skipped reset for serving target {target_instance} (op {})",
                        op.id
                    );
                    skipped_actions.push(reason.clone());
                    eprintln!("       {reason}; cutover must finish before destructive cleanup");
                    continue;
                }
                let mut action_steps = Vec::new();
                if container::exists_instance_in_project(&target_instance, project) {
                    let _ = container::exec_cmd_in_instance_project(
                        &target_instance,
                        project,
                        "tailscale down >/dev/null 2>&1 || true",
                    );
                    let _ = container::exec_cmd_in_instance_project(
                        &target_instance,
                        project,
                        "systemctl stop tailscaled >/dev/null 2>&1 || true",
                    );
                    match container::force_stop_instance_in_project(&target_instance, project) {
                        Ok(()) => action_steps.push("force-stop ok".to_string()),
                        Err(err) => action_steps.push(format!("force-stop failed: {err}")),
                    }
                    match container::delete_instance_in_project(&target_instance, project) {
                        Ok(()) => action_steps.push("delete ok".to_string()),
                        Err(err) => action_steps.push(format!("delete failed: {err}")),
                    }
                } else {
                    action_steps.push("target already absent".to_string());
                }
                let action_record = format!(
                    "{} (op {}): {}",
                    target_instance,
                    op.id,
                    action_steps.join("; ")
                );
                forced_this_cycle.push(action_record.clone());
                forced_actions.push(action_record);
            }
        }
        if !forced_this_cycle.is_empty() {
            eprintln!(
                "       Requested forced reset for {} target instance(s)",
                forced_this_cycle.len()
            );
            append_reconcile_attempt_record(
                app,
                "wait-for-container-operation",
                "takeover",
                serde_json::json!({
                    "policy": format!("{policy:?}").to_ascii_lowercase(),
                    "forced_targets": forced_this_cycle,
                }),
            );
            update_reconcile_phase_from_lease(app, "Reconciling", None);
            thread::sleep(takeover_sleep);
            continue;
        }

        if elapsed >= reconcile_budget {
            return Err(blocked_operation_error(
                app,
                project,
                reconcile_budget.as_secs(),
                phase,
                &blocking,
                &seen_at,
                &attempted_cancel_ids,
                &forced_actions,
                &skipped_actions,
            ));
        }

        thread::sleep(takeover_sleep);
    }
}

fn ensure_create_prereqs(project: &str) -> Result<(), String> {
    run_cmd_capture(
        "incus",
        &["--project", project, "profile", "show", "default"],
    )?;
    run_cmd_capture(
        "incus",
        &["--project", project, "image", "info", "images:ubuntu/24.04"],
    )?;
    Ok(())
}

fn cleanup_container_for_rebuild(app: &str, project: &str) -> Result<(), String> {
    wait_for_container_operation_quiet(app, project, None)?;

    if !container::exists(app) {
        return Ok(());
    }

    if container::is_running(app).unwrap_or(false) {
        let _ = container::stop(app);
    }

    for _ in 0..CONTAINER_DELETE_RETRY_CHECKS {
        if !container::exists(app) {
            return Ok(());
        }
        if let Err(err) = container::delete(app) {
            if !container::exists(app) {
                return Ok(());
            }
            eprintln!("       Retry delete after error: {err}");
        }
        thread::sleep(Duration::from_millis(CONTAINER_OP_WAIT_SLEEP_MS));
    }

    if container::exists(app) {
        return Err(format!(
            "failed to delete container '{app}' after waiting for background operations"
        ));
    }
    Ok(())
}

fn join_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_string()
    } else {
        values.join(", ")
    }
}

fn ensure_proxy_attached_with_recovery(
    target_app_ref: &str,
    project: &str,
    host_port: u16,
    container_port: u16,
) -> Result<(), String> {
    let target_instance = instance_name_from_app_ref(target_app_ref);
    let family_app = deploy_app_family_from_app_ref(target_app_ref);
    let first_err =
        match container::add_proxy_in_project(&target_instance, host_port, container_port, project)
        {
            Ok(()) => return Ok(()),
            Err(err) => err,
        };

    let owners = container::proxy_port_owners_in_project(project, host_port)?;
    if owners.is_empty() {
        return Err(format!(
            "failed to attach proxy device to {target_instance} on :{host_port}: {first_err}; no proxy owners discovered in project {project}"
        ));
    }

    let mut removed = Vec::new();
    let mut non_family_owners = Vec::new();
    let mut remove_errors = Vec::new();
    for owner in owners {
        if owner == target_instance {
            continue;
        }
        if !is_instance_in_app_family(&family_app, &owner) {
            non_family_owners.push(owner);
            continue;
        }
        match container::remove_proxy_in_project(&owner, project) {
            Ok(()) => removed.push(owner),
            Err(err) => remove_errors.push(format!("{owner}: {err}")),
        }
    }

    if !remove_errors.is_empty() {
        eprintln!(
            "       Warning: failed removing stale proxy owners for :{host_port}: {}",
            remove_errors.join(" | ")
        );
    }

    match container::add_proxy_in_project(&target_instance, host_port, container_port, project) {
        Ok(()) => {
            if !removed.is_empty() || !non_family_owners.is_empty() {
                eprintln!(
                    "       Recovered proxy :{host_port} for {target_instance} (removed: {}; non-family owners: {})",
                    join_or_none(&removed),
                    join_or_none(&non_family_owners)
                );
            }
            Ok(())
        }
        Err(retry_err) => {
            let remaining_non_family = container::proxy_port_owners_in_project(project, host_port)
                .unwrap_or_else(|_| non_family_owners.clone())
                .into_iter()
                .filter(|owner| {
                    owner != &target_instance && !is_instance_in_app_family(&family_app, owner)
                })
                .collect::<Vec<_>>();
            Err(format!(
                "failed to attach proxy device to {target_instance} on :{host_port} after same-app cleanup: {retry_err}; initial error: {first_err}; removed owners: {}; non-family owners: {}; remove errors: {}",
                join_or_none(&removed),
                join_or_none(&remaining_non_family),
                join_or_none(&remove_errors)
            ))
        }
    }
}

fn deploy_app_family_from_app_ref(app_ref: &str) -> String {
    let app_ref = app_ref.trim();
    if app_ref.is_empty() {
        return app_ref.to_string();
    }

    for marker in ["-build-", "-prev-", "-failed-"] {
        if let Some((base, suffix)) = app_ref.rsplit_once(marker)
            && !base.is_empty()
            && !suffix.is_empty()
            && suffix.chars().all(|ch| ch.is_ascii_digit())
        {
            return base.to_string();
        }
    }

    app_ref.to_string()
}

fn is_instance_in_app_family(app: &str, instance_name: &str) -> bool {
    let Some(candidate_app_ref) = app_ref_from_instance_name(instance_name) else {
        return false;
    };
    candidate_app_ref == app || is_transient_deploy_app_for(app, &candidate_app_ref)
}

fn prune_family_instance(instance_name: &str, project: &str) -> Result<(), String> {
    let Some(app_ref) = app_ref_from_instance_name(instance_name) else {
        return Err(format!(
            "invalid app family instance name '{instance_name}'"
        ));
    };

    if container::is_running(&app_ref).unwrap_or(false) {
        let _ = container::exec_cmd(&app_ref, "tailscale down >/dev/null 2>&1 || true");
        let _ = container::exec_cmd(
            &app_ref,
            "systemctl stop tailscaled >/dev/null 2>&1 || true",
        );
    }
    let _ = container::remove_proxy_in_project(instance_name, project);
    let _ = container::remove_storage_mount(&app_ref);
    let _ = container::remove_tailscale_state_mount(&app_ref);
    cleanup_container_for_rebuild(&app_ref, project)
}

fn reconcile_family_instances_strict(
    app: &str,
    keep_instance_names: &BTreeSet<String>,
    project: &str,
) -> Result<(), String> {
    let containers = container::list()?;
    let mut failures = Vec::new();

    for container in containers {
        if !is_instance_in_app_family(app, &container.name) {
            continue;
        }
        if keep_instance_names.contains(&container.name) {
            continue;
        }

        if let Err(err) = prune_family_instance(&container.name, project) {
            failures.push(format!("{}: {err}", container.name));
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "failed to reconcile app family instances for '{app}': {}",
            failures.join(" | ")
        ))
    }
}

fn cleanup_pending_detail(app: &str) -> Option<String> {
    deploy_commands::cleanup_pending_detail(app)
}

fn app_desired_state(app: &str) -> Result<&'static str, String> {
    deploy_commands::app_desired_state(app)
}

fn set_app_desired_state(app: &str, desired_state: &str) -> Result<(), String> {
    deploy_commands::set_app_desired_state(app, desired_state)
}

fn init_stacks_in(dir: &Path) -> Result<(), String> {
    admin_commands::init_stacks_in(dir)
}

fn psht_user_shell_path() -> Option<PathBuf> {
    admin_commands::psht_user_shell_path()
}

fn cleanup_all_owned_tailscale_devices(app: &str) -> Result<(), String> {
    admin_commands::cleanup_all_owned_tailscale_devices(app)
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".to_string())
}

pub fn cleanup_previous(app: &str) -> Result<(), String> {
    deploy_commands::cleanup_previous(app)
}

pub fn deploy(
    app: &str,
    git_ref: Option<&str>,
    git_sha: Option<&str>,
    force: bool,
) -> Result<(), String> {
    deploy_commands::deploy(app, git_ref, git_sha, force)
}

pub fn push(app: &str, force: bool) -> Result<(), String> {
    deploy_commands::push(app, force)
}

pub fn env_command(app: &str, assignments: &[String]) -> Result<(), String> {
    lifecycle_commands::env_command(app, assignments)
}

pub fn env_unset(app: &str, names: &[String]) -> Result<(), String> {
    lifecycle_commands::env_unset(app, names)
}

pub fn ps() -> Result<(), String> {
    observability_commands::ps()
}

pub fn health() -> Result<(), String> {
    observability_commands::health()
}

pub fn logs(app: &str, follow: bool) -> Result<(), String> {
    observability_commands::logs(app, follow)
}

pub fn debug_resources(app: Option<&str>, candidate: Option<&str>) -> Result<(), String> {
    observability_commands::debug_resources(app, candidate)
}

pub fn setup() -> Result<(), String> {
    admin_commands::setup()
}

pub fn update() -> Result<(), String> {
    admin_commands::update()
}

pub fn print_cli() -> Result<(), String> {
    admin_commands::print_cli()
}

pub fn init_stacks() -> Result<(), String> {
    admin_commands::init_stacks()
}

pub fn bootstrap() -> Result<(), String> {
    admin_commands::bootstrap()
}

pub fn upgrade_server() -> Result<(), String> {
    admin_commands::upgrade_server()
}

pub fn doctor() -> Result<(), String> {
    observability_commands::doctor()
}

pub fn tailscale_status(app: &str) -> Result<(), String> {
    admin_commands::tailscale_status(app)
}

pub fn tailscale_up(app: &str) -> Result<(), String> {
    admin_commands::tailscale_up(app)
}

pub fn tailscale_down(app: &str) -> Result<(), String> {
    admin_commands::tailscale_down(app)
}

pub fn web(bind: &str, port: u16) -> Result<(), String> {
    web_ui::serve(bind, port)
}

pub fn web_start(bind: &str, port: u16) -> Result<(), String> {
    admin_commands::web_start(bind, port)
}

pub fn web_stop() -> Result<(), String> {
    admin_commands::web_stop()
}

pub fn daemon() -> Result<(), String> {
    admin_commands::daemon()
}

pub fn supervise() -> Result<(), String> {
    admin_commands::supervise()
}

pub fn stop(app: &str) -> Result<(), String> {
    lifecycle_commands::stop(app)
}

pub fn start(app: &str) -> Result<(), String> {
    lifecycle_commands::start(app)
}

pub fn restart(app: &str) -> Result<(), String> {
    lifecycle_commands::restart(app)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DestroyOptions {
    pub keep_storage: bool,
}

pub fn destroy_with_options(app: &str, options: DestroyOptions) -> Result<(), String> {
    lifecycle_commands::destroy(app, options)
}

pub fn destroy(app: &str) -> Result<(), String> {
    destroy_with_options(app, DestroyOptions::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    #[test]
    fn allocate_port_is_deterministic() {
        let port1 = allocate_port("myapp");
        let port2 = allocate_port("myapp");
        assert_eq!(port1, port2);
    }

    #[test]
    fn allocate_port_in_valid_range() {
        for name in &["app1", "app2", "webapp", "api", "test-long-name"] {
            let port = allocate_port(name);
            assert!(
                port >= 3001 && port < 4001,
                "port {port} out of range for {name}"
            );
        }
    }

    #[test]
    fn allocate_port_different_apps_likely_differ() {
        let port1 = allocate_port("myapp");
        let port2 = allocate_port("other");
        // Not guaranteed, but very likely to differ
        assert_ne!(port1, port2);
    }

    #[test]
    fn default_forge_url_points_to_github() {
        assert_eq!(DEFAULT_FORGE_URL, "https://github.com/nakajima/psht");
    }

    #[test]
    fn binary_version_parses_cli_output() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("psht");
        fs::write(&bin, "#!/bin/sh\necho 'psht 9.9.9'\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(binary_version(&bin).as_deref(), Some("9.9.9"));
        assert!(binary_matches_version(&bin, "9.9.9"));
        assert!(!binary_matches_version(&bin, "0.0.1"));
    }

    #[test]
    fn home_dir_under_home() {
        let dir = home_dir();
        assert!(!dir.to_string_lossy().is_empty());
    }

    #[test]
    fn builds_dir_under_home() {
        let dir = builds_dir();
        assert!(dir.to_string_lossy().ends_with("builds"));
    }

    #[test]
    fn repos_dir_under_home() {
        let dir = repos_dir();
        assert!(dir.to_string_lossy().ends_with("repos"));
    }

    #[test]
    fn stacks_dir_under_home() {
        let dir = stacks_dir();
        assert!(dir.to_string_lossy().ends_with("stacks"));
    }

    #[test]
    fn stack_hash_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("test.sh");
        fs::write(&script, "#!/bin/sh\necho hello").unwrap();
        let hash1 = stack_hash(&script).unwrap();
        let hash2 = stack_hash(&script).unwrap();
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn stack_hash_changes_with_content() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("test.sh");
        fs::write(&script, "#!/bin/sh\necho hello").unwrap();
        let hash1 = stack_hash(&script).unwrap();
        fs::write(&script, "#!/bin/sh\necho world").unwrap();
        let hash2 = stack_hash(&script).unwrap();
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn stack_hash_is_hex_string() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("test.sh");
        fs::write(&script, "#!/bin/sh\necho hello").unwrap();
        let hash = stack_hash(&script).unwrap();
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn stack_hash_errors_on_missing_file() {
        let result = stack_hash(Path::new("/nonexistent/file.sh"));
        assert!(result.is_err());
    }

    #[test]
    fn apt_packages_fingerprint_is_order_insensitive() {
        let a = vec!["git".to_string(), "curl".to_string()];
        let b = vec!["curl".to_string(), "git".to_string()];
        assert_eq!(apt_packages_fingerprint(&a), apt_packages_fingerprint(&b));
    }

    #[test]
    fn apt_packages_fingerprint_ignores_blanks_and_duplicates() {
        let packages = vec![
            " curl ".to_string(),
            "".to_string(),
            "curl".to_string(),
            "git".to_string(),
            "   ".to_string(),
        ];
        let canonical = vec!["curl".to_string(), "git".to_string()];
        assert_eq!(
            apt_packages_fingerprint(&packages),
            apt_packages_fingerprint(&canonical)
        );
    }

    #[test]
    fn apt_packages_fingerprint_none_when_empty() {
        let packages = vec!["".to_string(), "  ".to_string()];
        assert!(apt_packages_fingerprint(&packages).is_none());
    }

    #[test]
    fn setup_hash_includes_apt_fingerprint_when_present() {
        assert_eq!(
            setup_hash("deadbeef", Some("cafebabe")),
            "deadbeef:cafebabe"
        );
        assert_eq!(setup_hash("deadbeef", None), "deadbeef");
    }

    #[test]
    fn binary_hash_cache_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = binary_hash_path_in(tmp.path(), "myapp");
        write_binary_hash_to(&path, "deadbeef").unwrap();
        assert_eq!(read_binary_hash_from(&path).as_deref(), Some("deadbeef"));
    }

    #[test]
    fn binary_payload_hash_none_without_marker() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("app"), "#!/bin/sh\necho ok\n").unwrap();
        let hash = binary_payload_hash(tmp.path()).unwrap();
        assert!(hash.is_none());
    }

    #[test]
    fn binary_payload_hash_changes_with_binary_content() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".psht-start-command"), "./app\n").unwrap();
        fs::write(tmp.path().join("app"), "first").unwrap();
        let hash1 = binary_payload_hash(tmp.path()).unwrap().unwrap();
        fs::write(tmp.path().join("app"), "second").unwrap();
        let hash2 = binary_payload_hash(tmp.path()).unwrap().unwrap();
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn binary_payload_hash_changes_with_start_command() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("app"), "same-bits").unwrap();
        fs::write(tmp.path().join(".psht-start-command"), "./app\n").unwrap();
        let hash1 = binary_payload_hash(tmp.path()).unwrap().unwrap();
        fs::write(tmp.path().join(".psht-start-command"), "./app --debug\n").unwrap();
        let hash2 = binary_payload_hash(tmp.path()).unwrap().unwrap();
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn read_build_number_defaults_to_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing.build");
        assert_eq!(read_build_number_from(&path), 0);
    }

    #[test]
    fn read_build_number_invalid_defaults_to_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.build");
        fs::write(&path, "nope\n").unwrap();
        assert_eq!(read_build_number_from(&path), 0);
    }

    #[test]
    fn increment_build_number_is_monotonic() {
        let tmp = tempfile::tempdir().unwrap();
        let n1 = increment_build_number_in(tmp.path(), "myapp").unwrap();
        let n2 = increment_build_number_in(tmp.path(), "myapp").unwrap();
        let n3 = increment_build_number_in(tmp.path(), "myapp").unwrap();
        assert_eq!((n1, n2, n3), (1, 2, 3));
    }

    fn unique_test_app(prefix: &str) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}-{}-{now}", std::process::id())
    }

    #[test]
    fn git_deploy_state_round_trip() {
        let app = unique_test_app("deploy-state");
        let target = GitCheckoutTarget {
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
        };
        write_git_deploy_state(&app, &target, GitDeployStatus::Success).unwrap();
        let state = GitDeployState {
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
            status: GitDeployStatus::Success,
        };
        let loaded = read_git_deploy_state(&app).unwrap().unwrap();
        assert_eq!(loaded, state);
        clear_git_deploy_state(&app).unwrap();
        assert!(read_git_deploy_state(&app).unwrap().is_none());
    }

    #[test]
    fn pending_git_target_round_trip() {
        let app = unique_test_app("pending-request");
        let target = GitCheckoutTarget {
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
        };
        let request = PendingGitDeployRequest::from_target(&target, false, None, None);
        write_pending_git_request(&app, &request).unwrap();
        let loaded = read_pending_git_request(&app).unwrap().unwrap().target();
        assert_eq!(loaded, target);
        let taken = take_pending_git_request(&app).unwrap().unwrap().target();
        assert_eq!(taken, target);
        assert!(take_pending_git_request(&app).unwrap().is_none());
    }

    #[test]
    fn pending_git_request_round_trip_preserves_force_metadata() {
        let app = unique_test_app("pending-force");
        let request = PendingGitDeployRequest {
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
            force: true,
            request_id: Some("req-123".to_string()),
            interrupt_requested_at: Some(123),
        };

        write_pending_git_request(&app, &request).unwrap();
        let loaded = read_pending_git_request(&app).unwrap().unwrap();
        assert_eq!(loaded, request);
    }

    #[test]
    fn deploy_interrupt_state_round_trip() {
        let app = unique_test_app("interrupt");
        let state = DeployInterruptState {
            request_id: "req-123".to_string(),
            requested_at: 123,
            target_sha: "deadbeef".to_string(),
        };
        request_deploy_interrupt(&app, &state).unwrap();
        let loaded = read_deploy_interrupt(&app).unwrap().unwrap();
        assert_eq!(loaded, state);
        clear_deploy_interrupt(&app).unwrap();
        assert!(read_deploy_interrupt(&app).unwrap().is_none());
    }

    #[test]
    fn upgrade_check_state_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("upgrade-check.toml");
        let state = UpgradeCheckState {
            checked_at: 123,
            latest: "0.3.0".to_string(),
        };
        write_upgrade_check_state_to(&path, &state).unwrap();
        let loaded = read_upgrade_check_state_from(&path).unwrap().unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn cleanup_job_state_round_trip() {
        let app = unique_test_app("cleanup-job");
        let state = CleanupJobState {
            app: app.clone(),
            active_instance_at_schedule: "psht-myapp-build-100".to_string(),
            scheduled_previous_instance: "psht-myapp-build-99".to_string(),
            attempts: 2,
            last_error: Some("busy".to_string()),
            scheduled_at: 100,
            updated_at: 101,
        };
        write_cleanup_job(&app, &state).unwrap();
        let loaded = read_cleanup_job(&app).unwrap().unwrap();
        assert_eq!(loaded, state);
        clear_cleanup_job(&app).unwrap();
        assert!(read_cleanup_job(&app).unwrap().is_none());
    }

    #[test]
    fn parse_deploy_lock_metadata_parses_fields() {
        let parsed = parse_deploy_lock_metadata("pid=123\ncreated=10\nupdated=12\n");
        assert_eq!(parsed.pid, Some(123));
        assert_eq!(parsed.created, Some(10));
        assert_eq!(parsed.updated, Some(12));
    }

    #[test]
    fn parse_deploy_lock_metadata_ignores_invalid_values() {
        let parsed = parse_deploy_lock_metadata("pid=nope\ncreated=\nupdated=abc\n");
        assert_eq!(parsed.pid, None);
        assert_eq!(parsed.created, None);
        assert_eq!(parsed.updated, None);
    }

    #[test]
    fn refresh_deploy_lock_heartbeat_updates_updated_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let path = deploy_lock_path_in(tmp.path(), "myapp");
        let metadata = DeployLockMetadata {
            pid: Some(std::process::id()),
            created: Some(100),
            updated: Some(100),
        };
        write_deploy_lock_metadata(&path, &metadata).unwrap();

        refresh_deploy_lock_heartbeat_at(&path, std::process::id()).unwrap();
        let loaded = read_deploy_lock_metadata_from(&path).unwrap().unwrap();

        assert_eq!(loaded.pid, Some(std::process::id()));
        assert_eq!(loaded.created, Some(100));
        assert!(loaded.updated.unwrap_or(0) >= 100);
    }

    #[test]
    fn deploy_lock_is_exclusive_until_guard_drops() {
        let tmp = tempfile::tempdir().unwrap();
        let path = deploy_lock_path_in(tmp.path(), "myapp");
        let guard = try_acquire_deploy_lock_at(&path).unwrap();
        assert!(guard.is_some());
        let second = try_acquire_deploy_lock_at(&path).unwrap();
        assert!(second.is_none());
        drop(guard);
        let third = try_acquire_deploy_lock_at(&path).unwrap();
        assert!(third.is_some());
    }

    #[test]
    fn cleanup_lock_is_exclusive_until_guard_drops() {
        let tmp = tempfile::tempdir().unwrap();
        let path = cleanup_lock_path_in(tmp.path(), "myapp");
        let guard = try_acquire_cleanup_lock_at(&path).unwrap();
        assert!(guard.is_some());
        let second = try_acquire_cleanup_lock_at(&path).unwrap();
        assert!(second.is_none());
        drop(guard);
        let third = try_acquire_cleanup_lock_at(&path).unwrap();
        assert!(third.is_some());
    }

    #[test]
    fn transient_deploy_app_for_detected() {
        assert!(is_transient_deploy_app_for(
            "hyperlinked",
            "hyperlinked-build-100"
        ));
        assert!(is_transient_deploy_app_for(
            "hyperlinked",
            "hyperlinked-prev-100"
        ));
        assert!(is_transient_deploy_app_for(
            "hyperlinked",
            "hyperlinked-failed-100"
        ));
        assert!(!is_transient_deploy_app_for(
            "hyperlinked",
            "hyperlinked-build-next"
        ));
        assert!(!is_transient_deploy_app_for(
            "hyperlinked",
            "other-build-100"
        ));
    }

    #[test]
    fn deploy_app_family_is_derived_from_transient_refs() {
        assert_eq!(
            deploy_app_family_from_app_ref("hyperlinked-build-100"),
            "hyperlinked"
        );
        assert_eq!(
            deploy_app_family_from_app_ref("hyperlinked-prev-100"),
            "hyperlinked"
        );
        assert_eq!(
            deploy_app_family_from_app_ref("hyperlinked-failed-100"),
            "hyperlinked"
        );
        assert_eq!(deploy_app_family_from_app_ref("hyperlinked"), "hyperlinked");
        assert_eq!(
            deploy_app_family_from_app_ref("hyperlinked-build-next"),
            "hyperlinked-build-next"
        );
    }

    #[test]
    fn instance_membership_detects_full_app_family() {
        assert!(is_instance_in_app_family("hyperlinked", "psht-hyperlinked"));
        assert!(is_instance_in_app_family(
            "hyperlinked",
            "psht-hyperlinked-build-1772593344"
        ));
        assert!(is_instance_in_app_family(
            "hyperlinked",
            "psht-hyperlinked-prev-1772593344"
        ));
        assert!(is_instance_in_app_family(
            "hyperlinked",
            "psht-hyperlinked-failed-1772593344"
        ));
        assert!(!is_instance_in_app_family(
            "hyperlinked",
            "psht-other-build-1772593344"
        ));
    }

    #[test]
    fn git_target_already_succeeded_checks_success_and_sha() {
        let target = GitCheckoutTarget {
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
        };
        let success_same_sha = GitDeployState {
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
            status: GitDeployStatus::Success,
        };
        let failed_same_sha = GitDeployState {
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
            status: GitDeployStatus::Failed,
        };
        let success_other_sha = GitDeployState {
            ref_name: "refs/heads/main".to_string(),
            sha: "beadfeed".to_string(),
            status: GitDeployStatus::Success,
        };

        assert!(git_target_already_succeeded_with_state(
            Some(&success_same_sha),
            &target
        ));
        assert!(!git_target_already_succeeded_with_state(
            Some(&failed_same_sha),
            &target
        ));
        assert!(!git_target_already_succeeded_with_state(
            Some(&success_other_sha),
            &target
        ));
        assert!(!git_target_already_succeeded_with_state(None, &target));
    }

    #[test]
    fn should_process_pending_request_honors_force_for_same_sha() {
        let active = GitCheckoutTarget {
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
        };
        let non_forced = PendingGitDeployRequest {
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
            force: false,
            request_id: None,
            interrupt_requested_at: None,
        };
        let forced = PendingGitDeployRequest {
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
            force: true,
            request_id: Some("req-123".to_string()),
            interrupt_requested_at: Some(123),
        };
        assert!(!should_process_pending_request(Some(&active), &non_forced));
        assert!(should_process_pending_request(Some(&active), &forced));
    }

    #[test]
    fn pending_force_request_is_ours_supports_legacy_force_without_request_id() {
        let pending = PendingGitDeployRequest {
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
            force: true,
            request_id: None,
            interrupt_requested_at: None,
        };
        assert!(pending_force_request_is_ours(
            Some(&pending),
            "req-123",
            "deadbeef"
        ));
        assert!(!pending_force_request_is_ours(
            Some(&pending),
            "req-123",
            "cafebabe"
        ));
    }

    #[test]
    fn deploy_interrupted_error_prefix_is_detected() {
        let state = DeployInterruptState {
            request_id: "req-123".to_string(),
            requested_at: 123,
            target_sha: "deadbeef".to_string(),
        };
        let msg = deploy_interrupted_error("cutover start", &state);
        assert!(is_deploy_interrupted_error(&msg));
        assert!(!is_deploy_interrupted_error("some other error"));
    }

    #[test]
    fn resolve_stack_uses_custom_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let code_dir = tmp.path().join("code");
        let stacks = tmp.path().join("stacks");
        fs::create_dir_all(&code_dir).unwrap();
        fs::create_dir_all(&stacks).unwrap();
        fs::write(code_dir.join("psht-stack.sh"), "#!/bin/sh\ncustom setup").unwrap();

        let (name, path) = resolve_stack_in("myapp", &code_dir, "bun", &stacks).unwrap();
        assert_eq!(name, "myapp");
        assert_eq!(path, stacks.join("myapp.sh"));
    }

    #[test]
    fn resolve_stack_falls_back_to_builtin() {
        let tmp = tempfile::tempdir().unwrap();
        let code_dir = tmp.path().join("code");
        let stacks = tmp.path().join("stacks");
        fs::create_dir_all(&code_dir).unwrap();
        fs::create_dir_all(&stacks).unwrap();

        let (name, path) = resolve_stack_in("myapp", &code_dir, "bun", &stacks).unwrap();
        assert_eq!(name, "bun");
        assert_eq!(path, stacks.join("bun.sh"));
    }

    #[test]
    fn resolve_stack_saves_custom_to_stacks_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let code_dir = tmp.path().join("code");
        let stacks = tmp.path().join("stacks");
        fs::create_dir_all(&code_dir).unwrap();
        fs::create_dir_all(&stacks).unwrap();
        let content = "#!/bin/sh\napt install ffmpeg";
        fs::write(code_dir.join("psht-stack.sh"), content).unwrap();

        resolve_stack_in("myapp", &code_dir, "bun", &stacks).unwrap();

        let saved = fs::read_to_string(stacks.join("myapp.sh")).unwrap();
        assert_eq!(saved, content);
    }

    #[test]
    fn init_stacks_writes_all_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("stacks");
        init_stacks_in(&dir).unwrap();
        for (name, _) in STACKS {
            assert!(dir.join(format!("{name}.sh")).exists(), "missing {name}.sh");
        }
    }

    #[test]
    fn init_stacks_content_matches_embedded() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("stacks");
        init_stacks_in(&dir).unwrap();
        for (name, content) in STACKS {
            let written = fs::read_to_string(dir.join(format!("{name}.sh"))).unwrap();
            assert_eq!(&written, *content, "content mismatch for {name}.sh");
        }
    }

    #[test]
    fn init_stacks_creates_dir_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("deep").join("nested").join("stacks");
        assert!(!dir.exists());
        init_stacks_in(&dir).unwrap();
        assert!(dir.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), STACKS.len());
    }

    #[test]
    fn init_stacks_overwrites_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("stacks");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("bun.sh"), "stale content").unwrap();
        init_stacks_in(&dir).unwrap();
        let content = fs::read_to_string(dir.join("bun.sh")).unwrap();
        assert_ne!(content, "stale content");
        assert_eq!(content, STACKS.iter().find(|(n, _)| *n == "bun").unwrap().1);
    }

    #[test]
    fn parse_version_codename_from_os_release() {
        let os_release = "NAME=Ubuntu\nVERSION_CODENAME=noble\n";
        assert_eq!(
            parse_version_codename(os_release),
            Some("noble".to_string())
        );
    }

    #[test]
    fn parse_version_codename_handles_quotes() {
        let os_release = "NAME=Ubuntu\nVERSION_CODENAME=\"jammy\"\n";
        assert_eq!(
            parse_version_codename(os_release),
            Some("jammy".to_string())
        );
    }

    #[test]
    fn profile_has_root_disk_detects_root_device() {
        let profile = r#"name: default
devices:
  eth0:
    type: nic
    nictype: bridged
  root:
    path: /
    pool: default
    type: disk
"#;
        assert!(profile_has_root_disk(profile));
    }

    #[test]
    fn profile_has_root_disk_detects_non_root_named_device() {
        let profile = r#"name: default
devices:
  data:
    path: /data
    pool: default
    type: disk
  disk0:
    path: /
    pool: default
    type: disk
"#;
        assert!(profile_has_root_disk(profile));
    }

    #[test]
    fn profile_has_root_disk_returns_false_without_root_path_disk() {
        let profile = r#"name: default
devices:
  root:
    path: /
    pool: default
    type: nic
  disk0:
    path: /data
    pool: default
    type: disk
"#;
        assert!(!profile_has_root_disk(profile));
    }

    #[test]
    fn profile_has_nic_detects_nic_device() {
        let profile = r#"name: default
devices:
  eth0:
    type: nic
    network: incusbr0
  root:
    path: /
    pool: default
    type: disk
"#;
        assert!(profile_has_nic(profile));
    }

    #[test]
    fn profile_has_nic_returns_false_without_nic() {
        let profile = r#"name: default
devices:
  root:
    path: /
    pool: default
    type: disk
"#;
        assert!(!profile_has_nic(profile));
    }

    #[test]
    fn app_storage_volume_name_formats_correctly() {
        assert_eq!(app_storage_volume_name("myapp"), "psht-storage-myapp");
        assert_eq!(app_storage_volume_name("my-app"), "psht-storage-my-app");
    }

    #[test]
    fn app_tailscale_volume_name_formats_correctly() {
        assert_eq!(app_tailscale_volume_name("myapp"), "psht-tailscale-myapp");
        assert_eq!(app_tailscale_volume_name("my-app"), "psht-tailscale-my-app");
    }

    #[test]
    fn ensure_line_in_file_appends_once() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("shells");
        fs::write(&path, "/bin/sh\n").unwrap();

        ensure_line_in_file(&path, "/opt/psht/bin/psht-server").unwrap();
        ensure_line_in_file(&path, "/opt/psht/bin/psht-server").unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let count = contents
            .lines()
            .filter(|line| *line == "/opt/psht/bin/psht-server")
            .count();
        assert_eq!(count, 1, "line should only be written once");
    }

    #[test]
    fn path_is_world_executable_checks_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bin");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("psht");
        fs::write(&file, "#!/bin/sh\necho ok\n").unwrap();

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(path_is_world_executable(&file).unwrap());

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!path_is_world_executable(&file).unwrap());
    }

    #[test]
    fn install_binary_atomically_creates_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("psht-server.src");
        let dst = tmp.path().join("bin").join("psht-server");
        fs::write(&src, "new-binary").unwrap();

        install_binary_atomically(&src, &dst, 0o755, "psht-server").unwrap();

        assert_eq!(fs::read_to_string(&dst).unwrap(), "new-binary");
        let mode = fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn install_binary_atomically_overwrites_existing_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("psht-server.src");
        let dst = tmp.path().join("bin").join("psht-server");
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::write(&src, "new-binary").unwrap();
        fs::write(&dst, "old-binary").unwrap();

        install_binary_atomically(&src, &dst, 0o755, "psht-server").unwrap();

        assert_eq!(fs::read_to_string(&dst).unwrap(), "new-binary");
    }

    #[test]
    fn write_oauth_config_writes_expected_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tailscale-oauth");
        write_oauth_config(&path, "cid", "secret").unwrap();
        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("TS_OAUTH_CLIENT_ID=cid"));
        assert!(contents.contains("TS_OAUTH_CLIENT_SECRET=secret"));
    }

    #[test]
    fn parse_tailscale_dns_name_trims_trailing_dot() {
        let json = r#"{"Self":{"DNSName":"psht.tailnet.ts.net."}}"#;
        assert_eq!(
            parse_tailscale_dns_name(json),
            Some("psht.tailnet.ts.net".to_string())
        );
    }

    #[test]
    fn parse_tailscale_dns_name_ignores_empty() {
        let json = r#"{"Self":{"DNSName":""}}"#;
        assert!(parse_tailscale_dns_name(json).is_none());
    }

    #[test]
    fn tailscale_ssh_enabled_from_status_json_accepts_self_ssh_flag() {
        let json = r#"{"Self":{"SSH":true}}"#;
        assert!(tailscale_ssh_enabled_from_status_json(json).unwrap());
    }

    #[test]
    fn tailscale_ssh_enabled_from_status_json_accepts_capabilities_ssh() {
        let json = r#"{"Self":{"Capabilities":["https://tailscale.com/cap/ssh"]}}"#;
        assert!(tailscale_ssh_enabled_from_status_json(json).unwrap());
    }

    #[test]
    fn tailscale_ssh_enabled_from_status_json_accepts_capmap_ssh() {
        let json = r#"{"Self":{"CapMap":{"https://tailscale.com/cap/ssh":null}}}"#;
        assert!(tailscale_ssh_enabled_from_status_json(json).unwrap());
    }

    #[test]
    fn tailscale_hostname_is_exact_matches_label_only() {
        assert!(tailscale_hostname_is_exact(
            "hyperlinked.tail.ts.net",
            "hyperlinked"
        ));
        assert!(tailscale_hostname_is_exact(
            "Hyperlinked.tail.ts.net.",
            "hyperlinked"
        ));
        assert!(!tailscale_hostname_is_exact(
            "hyperlinked-1.tail.ts.net",
            "hyperlinked"
        ));
        assert!(!tailscale_hostname_is_exact(
            "other.tail.ts.net",
            "hyperlinked"
        ));
    }

    #[test]
    fn acquire_exact_tailscale_hostname_uses_state_when_exact_and_healthy() {
        let mut state_calls = 0usize;
        let mut auth_calls = 0usize;
        let mut reset_calls = 0usize;
        let mut sleep_calls = 0usize;
        let mut health_calls = 0usize;

        let name = acquire_exact_tailscale_hostname_with_retry(
            "candidate",
            "hyperlinked",
            |_container, _app| {
                state_calls += 1;
                Ok(TailscaleJoinAttempt::from_dns_name(Some(
                    "hyperlinked.tail.ts.net".to_string(),
                )))
            },
            |_container, _app| {
                auth_calls += 1;
                Ok(TailscaleJoinAttempt::from_dns_name(Some(
                    "unexpected.tail.ts.net".to_string(),
                )))
            },
            |_container| {
                health_calls += 1;
                Ok(())
            },
            |_container| {
                reset_calls += 1;
                Ok(())
            },
            |_duration| {
                sleep_calls += 1;
            },
        )
        .unwrap();

        assert_eq!(name.as_deref(), Some("hyperlinked.tail.ts.net"));
        assert_eq!(state_calls, 1);
        assert_eq!(auth_calls, 0);
        assert_eq!(health_calls, 1);
        assert_eq!(reset_calls, 0);
        assert_eq!(sleep_calls, 0);
    }

    #[test]
    fn acquire_exact_tailscale_hostname_switches_to_auth_when_state_has_no_dns() {
        let mut state_calls = 0usize;
        let mut auth_calls = 0usize;
        let mut reset_calls = 0usize;
        let mut sleep_calls = 0usize;

        let name = acquire_exact_tailscale_hostname_with_retry(
            "candidate",
            "hyperlinked",
            |_container, _app| {
                state_calls += 1;
                Ok(TailscaleJoinAttempt::from_dns_name(None))
            },
            |_container, _app| {
                auth_calls += 1;
                Ok(TailscaleJoinAttempt::from_dns_name(Some(
                    "hyperlinked.tail.ts.net".to_string(),
                )))
            },
            |_container| Ok(()),
            |_container| {
                reset_calls += 1;
                Ok(())
            },
            |_duration| {
                sleep_calls += 1;
            },
        )
        .unwrap();

        assert_eq!(name.as_deref(), Some("hyperlinked.tail.ts.net"));
        assert_eq!(state_calls, 1);
        assert_eq!(auth_calls, 1);
        assert_eq!(reset_calls, 1);
        assert_eq!(sleep_calls, 1);
    }

    #[test]
    fn acquire_exact_tailscale_hostname_switches_to_auth_when_state_is_unhealthy() {
        let mut state_calls = 0usize;
        let mut auth_calls = 0usize;
        let mut reset_calls = 0usize;
        let mut sleep_calls = 0usize;
        let mut health_calls = 0usize;

        let name = acquire_exact_tailscale_hostname_with_retry(
            "candidate",
            "hyperlinked",
            |_container, _app| {
                state_calls += 1;
                Ok(TailscaleJoinAttempt::from_dns_name(Some(
                    "hyperlinked.tail.ts.net".to_string(),
                )))
            },
            |_container, _app| {
                auth_calls += 1;
                Ok(TailscaleJoinAttempt::from_dns_name(Some(
                    "hyperlinked.tail.ts.net".to_string(),
                )))
            },
            |_container| {
                health_calls += 1;
                if health_calls == 1 {
                    Err("state unhealthy".to_string())
                } else {
                    Ok(())
                }
            },
            |_container| {
                reset_calls += 1;
                Ok(())
            },
            |_duration| {
                sleep_calls += 1;
            },
        )
        .unwrap();

        assert_eq!(name.as_deref(), Some("hyperlinked.tail.ts.net"));
        assert_eq!(state_calls, 1);
        assert_eq!(auth_calls, 1);
        assert_eq!(health_calls, 2);
        assert_eq!(reset_calls, 1);
        assert_eq!(sleep_calls, 1);
    }

    #[test]
    fn acquire_exact_tailscale_hostname_falls_back_to_auth_key_immediately_when_state_requires_login()
     {
        let mut state_calls = 0usize;
        let mut auth_calls = 0usize;
        let mut reset_calls = 0usize;
        let mut sleep_calls = 0usize;

        let name = acquire_exact_tailscale_hostname_with_retry(
            "candidate",
            "hyperlinked",
            |_container, _app| {
                state_calls += 1;
                Err("tailscale state requires login (state: NeedsLogin)".to_string())
            },
            |_container, _app| {
                auth_calls += 1;
                Ok(TailscaleJoinAttempt::from_dns_name(Some(
                    "hyperlinked.tail.ts.net".to_string(),
                )))
            },
            |_container| Ok(()),
            |_container| {
                reset_calls += 1;
                Ok(())
            },
            |_duration| {
                sleep_calls += 1;
            },
        )
        .unwrap();

        assert_eq!(name.as_deref(), Some("hyperlinked.tail.ts.net"));
        assert_eq!(state_calls, 1);
        assert_eq!(auth_calls, 1);
        assert_eq!(reset_calls, 1);
        assert_eq!(sleep_calls, 1);
    }

    #[test]
    fn acquire_exact_tailscale_hostname_stays_on_auth_key_after_state_requires_login() {
        let mut state_calls = 0usize;
        let mut auth_calls = 0usize;
        let mut reset_calls = 0usize;
        let mut sleep_calls = 0usize;
        let mut auth_results = std::collections::VecDeque::from([
            Ok(Some("hyperlinked-1.tail.ts.net".to_string())),
            Ok(Some("hyperlinked.tail.ts.net".to_string())),
        ]);

        let name = acquire_exact_tailscale_hostname_with_retry(
            "candidate",
            "hyperlinked",
            |_container, _app| {
                state_calls += 1;
                Err("tailscale state requires login (state: NoState)".to_string())
            },
            |_container, _app| {
                auth_calls += 1;
                auth_results
                    .pop_front()
                    .expect("auth result should exist for each auth attempt")
                    .map(TailscaleJoinAttempt::from_dns_name)
            },
            |_container| Ok(()),
            |_container| {
                reset_calls += 1;
                Ok(())
            },
            |_duration| {
                sleep_calls += 1;
            },
        )
        .unwrap();

        assert_eq!(name.as_deref(), Some("hyperlinked.tail.ts.net"));
        assert_eq!(state_calls, 1);
        assert_eq!(auth_calls, 2);
        assert_eq!(reset_calls, 2);
        assert_eq!(sleep_calls, 2);
    }

    #[test]
    fn acquire_exact_tailscale_hostname_fails_fast_when_cleanup_lookup_unavailable_and_non_exact() {
        let mut state_calls = 0usize;
        let mut auth_calls = 0usize;

        let err = acquire_exact_tailscale_hostname_with_retry(
            "candidate",
            "hyperlinked",
            |_container, _app| {
                state_calls += 1;
                Err("tailscale state requires login (state: NeedsLogin)".to_string())
            },
            |_container, _app| {
                auth_calls += 1;
                Ok(TailscaleJoinAttempt {
                    dns_name: Some("hyperlinked-1.tail.ts.net".to_string()),
                    cleanup_lookup_error: Some("device list unavailable".to_string()),
                })
            },
            |_container| Ok(()),
            |_container| Ok(()),
            |_duration| {},
        )
        .unwrap_err();

        assert!(err.contains("ownership cleanup was unavailable"));
        assert!(err.contains("hyperlinked-1.tail.ts.net"));
        assert_eq!(state_calls, 1);
        assert_eq!(auth_calls, 1);
    }

    #[test]
    fn acquire_exact_tailscale_hostname_does_not_fail_fast_when_cleanup_permission_denied() {
        let mut state_calls = 0usize;
        let mut auth_calls = 0usize;
        let mut reset_calls = 0usize;
        let mut sleep_calls = 0usize;
        let mut auth_results = std::collections::VecDeque::from([
            TailscaleJoinAttempt {
                dns_name: Some("hyperlinked-1.tail.ts.net".to_string()),
                cleanup_lookup_error: Some(
                    "failed to list tailscale devices (http 403): calling actor does not have enough permissions"
                        .to_string(),
                ),
            },
            TailscaleJoinAttempt {
                dns_name: Some("hyperlinked.tail.ts.net".to_string()),
                cleanup_lookup_error: Some(
                    "failed to list tailscale devices (http 403): calling actor does not have enough permissions"
                        .to_string(),
                ),
            },
        ]);

        let name = acquire_exact_tailscale_hostname_with_retry(
            "candidate",
            "hyperlinked",
            |_container, _app| {
                state_calls += 1;
                Err("tailscale state is unusable (state: NoState)".to_string())
            },
            |_container, _app| {
                auth_calls += 1;
                Ok(auth_results
                    .pop_front()
                    .expect("auth result should exist for each auth attempt"))
            },
            |_container| Ok(()),
            |_container| {
                reset_calls += 1;
                Ok(())
            },
            |_duration| {
                sleep_calls += 1;
            },
        )
        .unwrap();

        assert_eq!(name.as_deref(), Some("hyperlinked.tail.ts.net"));
        assert_eq!(state_calls, 1);
        assert_eq!(auth_calls, 2);
        assert_eq!(reset_calls, 2);
        assert_eq!(sleep_calls, 2);
    }

    #[test]
    fn tailscale_api_permission_denied_detects_403_responses() {
        assert!(tailscale_api_permission_denied(
            "failed to list tailscale devices (http 403): {\"message\":\"calling actor does not have enough permissions to perform this function\"}"
        ));
        assert!(!tailscale_api_permission_denied(
            "failed to list tailscale devices (http 500): upstream unavailable"
        ));
    }

    #[test]
    fn join_tailscale_for_repair_with_fallback_prefers_state_with_dns_name() {
        let mut state_calls = 0usize;
        let mut auth_calls = 0usize;

        let (name, created_via) = join_tailscale_for_repair_with_fallback(
            || {
                state_calls += 1;
                Ok(Some("hyperlinked.tail.ts.net".to_string()))
            },
            || {
                auth_calls += 1;
                Ok(Some("unexpected.tail.ts.net".to_string()))
            },
        )
        .unwrap();

        assert_eq!(name.as_deref(), Some("hyperlinked.tail.ts.net"));
        assert_eq!(created_via, "state");
        assert_eq!(state_calls, 1);
        assert_eq!(auth_calls, 0);
    }

    #[test]
    fn join_tailscale_for_repair_with_fallback_uses_auth_key_when_state_has_no_dns() {
        let mut state_calls = 0usize;
        let mut auth_calls = 0usize;

        let (name, created_via) = join_tailscale_for_repair_with_fallback(
            || {
                state_calls += 1;
                Ok(None)
            },
            || {
                auth_calls += 1;
                Ok(Some("hyperlinked.tail.ts.net".to_string()))
            },
        )
        .unwrap();

        assert_eq!(name.as_deref(), Some("hyperlinked.tail.ts.net"));
        assert_eq!(created_via, "auth_key");
        assert_eq!(state_calls, 1);
        assert_eq!(auth_calls, 1);
    }

    #[test]
    fn join_tailscale_for_repair_with_fallback_uses_auth_key_when_state_errors() {
        let mut state_calls = 0usize;
        let mut auth_calls = 0usize;

        let (name, created_via) = join_tailscale_for_repair_with_fallback(
            || {
                state_calls += 1;
                Err("state unavailable".to_string())
            },
            || {
                auth_calls += 1;
                Ok(Some("hyperlinked.tail.ts.net".to_string()))
            },
        )
        .unwrap();

        assert_eq!(name.as_deref(), Some("hyperlinked.tail.ts.net"));
        assert_eq!(created_via, "auth_key");
        assert_eq!(state_calls, 1);
        assert_eq!(auth_calls, 1);
    }

    #[test]
    fn tailscale_conflict_label_matches_exact_and_numeric_suffix() {
        assert!(tailscale_conflict_label_for_app(
            "hyperlinked",
            "hyperlinked"
        ));
        assert!(tailscale_conflict_label_for_app(
            "hyperlinked-7",
            "hyperlinked"
        ));
        assert!(!tailscale_conflict_label_for_app(
            "hyperlinked-stage",
            "hyperlinked"
        ));
        assert!(!tailscale_conflict_label_for_app(
            "other-hyperlinked-1",
            "hyperlinked"
        ));
    }

    #[test]
    fn parse_tailscale_self_snapshot_extracts_identity_fields() {
        let json = r#"{
            "BackendState":"Running",
            "Health":[],
            "Self":{
                "ID":"n123",
                "HostName":"hyperlinked",
                "DNSName":"hyperlinked.tail.ts.net.",
                "Online":true,
                "TailscaleIPs":["100.64.1.2"]
            }
        }"#;
        let snapshot = parse_tailscale_self_snapshot(json).unwrap();
        assert_eq!(snapshot.device_id.as_deref(), Some("n123"));
        assert_eq!(snapshot.hostname_label.as_deref(), Some("hyperlinked"));
        assert_eq!(
            snapshot.dns_name.as_deref(),
            Some("hyperlinked.tail.ts.net")
        );
        assert_eq!(snapshot.backend_state, "Running");
        assert!(snapshot.online);
        assert_eq!(snapshot.ips, vec!["100.64.1.2".to_string()]);
    }

    #[test]
    fn retry_attempt_budget_is_bounded_and_non_zero() {
        let budget = retry_attempt_budget(30, Duration::from_millis(1_000));
        assert!(budget >= 2);
        assert_eq!(budget, 31);
    }

    #[test]
    fn tailscale_self_status_summary_outputs_self_only_fields() {
        let json = r#"{
            "BackendState":"Running",
            "Self":{
                "HostName":"hyperlinked",
                "DNSName":"hyperlinked.tail.ts.net.",
                "Online":true,
                "TailscaleIPs":["100.64.1.2"]
            },
            "Peer":{
                "abc":{"HostName":"other","DNSName":"other.tail.ts.net.","Online":true}
            }
        }"#;
        let summary = tailscale_self_status_summary_from_json("hyperlinked", json).unwrap();
        assert!(summary.contains("App: hyperlinked"));
        assert!(summary.contains("Host: hyperlinked"));
        assert!(summary.contains("DNS: hyperlinked.tail.ts.net"));
        assert!(summary.contains("State: Running"));
        assert!(summary.contains("Online: yes"));
        assert!(summary.contains("IPs: 100.64.1.2"));
        assert!(!summary.contains("other.tail.ts.net"));
    }

    #[test]
    fn tailscale_self_status_summary_includes_repair_hint_when_unhealthy() {
        let json = r#"{
            "BackendState":"NeedsLogin",
            "Self":{
                "HostName":"hyperlinked",
                "DNSName":"hyperlinked.tail.ts.net.",
                "Online":false,
                "TailscaleIPs":["100.64.1.2"]
            },
            "Health":["not logged in"]
        }"#;
        let summary = tailscale_self_status_summary_from_json("hyperlinked", json).unwrap();
        assert!(summary.contains("Online: no"));
        assert!(summary.contains("Health: not logged in"));
        assert!(summary.contains("Repair: psht tailscale up hyperlinked"));
    }

    #[test]
    fn parse_latest_release_version_url_parses_tag_url() {
        let url = "https://example.com/org/repo/releases/tag/v1.2.3";
        assert_eq!(
            parse_latest_release_version_url(url).as_deref(),
            Some("1.2.3")
        );
    }

    #[test]
    fn parse_latest_release_version_url_rejects_latest_path() {
        let url = "https://example.com/org/repo/releases/latest";
        assert!(parse_latest_release_version_url(url).is_none());
    }

    #[test]
    fn parse_version_components_handles_prefix_and_suffixes() {
        assert_eq!(parse_version_components("v1.2.3"), Some(vec![1, 2, 3]));
        assert_eq!(
            parse_version_components("1.2.3-beta.1+build.7"),
            Some(vec![1, 2, 3])
        );
    }

    #[test]
    fn parse_version_components_rejects_invalid() {
        assert!(parse_version_components("").is_none());
        assert!(parse_version_components("latest").is_none());
        assert!(parse_version_components("1..3").is_none());
    }

    #[test]
    fn version_is_newer_compares_numeric_segments() {
        assert!(version_is_newer("0.2.29", "0.2.28"));
        assert!(version_is_newer("1.0.0", "0.99.99"));
        assert!(!version_is_newer("0.2.28", "0.2.28"));
        assert!(!version_is_newer("0.2.27", "0.2.28"));
    }

    #[test]
    fn busy_op_policy_from_raw_parses_supported_values() {
        assert_eq!(
            busy_op_policy_from_raw(Some("diagnose")),
            BusyOpPolicy::Diagnose
        );
        assert_eq!(busy_op_policy_from_raw(Some("force")), BusyOpPolicy::Force);
        assert_eq!(busy_op_policy_from_raw(Some("AUTO")), BusyOpPolicy::Auto);
        assert_eq!(busy_op_policy_from_raw(None), BusyOpPolicy::Auto);
    }

    #[test]
    fn update_script_contains_json_manifest_for_rust_native_cli() {
        let manifest = cli_update_manifest();
        let script = update_script("psht", &manifest).unwrap();
        let json = cli_update_manifest_json(&manifest).unwrap();
        assert!(script.contains(&json));
        assert!(script.contains("PSHT_BIN=$(command -v psht)"));
    }

    #[test]
    fn setup_script_bootstraps_cli_then_runs_setup() {
        let manifest = cli_update_manifest();
        let script = setup_script("psht", &manifest).unwrap();
        let json = cli_update_manifest_json(&manifest).unwrap();
        assert!(script.contains(&json));
        assert!(script.contains("\"$PSHT_BIN\" setup"));
        assert!(script.contains("host = \"psht\""));
    }

    fn run_git(args: &[&str], cwd: &Path) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {} failed", args.join(" "));
    }

    fn git_output(args: &[&str], cwd: &Path) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn seed_remote_with_branch(branch: &str) -> (tempfile::TempDir, PathBuf, PathBuf, String) {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        let remote = tmp.path().join("remote.git");
        fs::create_dir_all(&work).unwrap();

        run_git(&["init"], &work);
        run_git(&["config", "user.name", "psht tests"], &work);
        run_git(&["config", "user.email", "psht-tests@example.com"], &work);
        fs::write(work.join("README.md"), "hello\n").unwrap();
        run_git(&["add", "README.md"], &work);
        run_git(&["commit", "-m", "init"], &work);
        let sha = git_output(&["rev-parse", "HEAD"], &work);

        let remote_str = remote.to_string_lossy().to_string();
        run_git(&["init", "--bare", &remote_str], tmp.path());
        run_git(&["branch", "-M", branch], &work);
        run_git(&["remote", "add", "origin", &remote_str], &work);
        run_git(&["push", "origin", branch], &work);

        (tmp, remote, work, sha)
    }

    #[test]
    fn parse_git_checkout_target_requires_ref_and_sha_pair() {
        let err = parse_git_checkout_target(Some("refs/heads/main"), None).unwrap_err();
        assert!(err.contains("both --ref and --sha"));
        let err = parse_git_checkout_target(None, Some("deadbeef")).unwrap_err();
        assert!(err.contains("both --ref and --sha"));
    }

    #[test]
    fn parse_git_checkout_target_rejects_empty_fields() {
        let err = parse_git_checkout_target(Some(" "), Some("deadbeef")).unwrap_err();
        assert!(err.contains("deploy ref is empty"));
        let err = parse_git_checkout_target(Some("refs/heads/main"), Some(" ")).unwrap_err();
        assert!(err.contains("deploy sha is empty"));
    }

    #[test]
    fn checkout_code_in_uses_branch_ref_without_bare_head_update() {
        let (tmp, remote, _work, sha) = seed_remote_with_branch("main");
        let build = tmp.path().join("build");
        let target = GitCheckoutTarget {
            ref_name: "refs/heads/main".to_string(),
            sha,
        };

        checkout_code_in(&remote, &build, Some(&target)).unwrap();

        let readme = fs::read_to_string(build.join("README.md")).unwrap();
        assert_eq!(readme, "hello\n");
    }

    #[test]
    fn checkout_code_in_supports_annotated_tag_refs() {
        let (tmp, remote, work, _sha) = seed_remote_with_branch("main");
        run_git(&["tag", "-a", "v1", "-m", "v1"], &work);
        run_git(&["push", "origin", "refs/tags/v1"], &work);
        let tag_oid = git_output(&["rev-parse", "refs/tags/v1"], &work);

        let build = tmp.path().join("build-tag");
        let target = GitCheckoutTarget {
            ref_name: "refs/tags/v1".to_string(),
            sha: tag_oid.clone(),
        };
        checkout_code_in(&remote, &build, Some(&target)).unwrap();

        let resolved_tag = git_output(&["rev-parse", "refs/tags/v1"], &build);
        assert_eq!(resolved_tag, tag_oid);
    }

    #[test]
    fn scrub_git_local_env_removes_repo_local_git_vars() {
        let output = {
            let mut cmd = Command::new("env");
            cmd.env("HOME", "/tmp/psht-home");
            cmd.env("GIT_DIR", ".");
            cmd.env("GIT_WORK_TREE", "/tmp/worktree");
            scrub_git_local_env(&mut cmd);
            cmd.output().unwrap()
        };
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("HOME=/tmp/psht-home"));
        assert!(!stdout.contains("GIT_DIR=."));
        assert!(!stdout.contains("GIT_WORK_TREE=/tmp/worktree"));
    }

    #[test]
    fn command_entrypoints_reject_invalid_app_name() {
        for result in [
            deploy("bad/name", None, None, false),
            push("bad/name", false),
            logs("bad/name", false),
            stop("bad/name"),
            start("bad/name"),
            restart("bad/name"),
            destroy("bad/name"),
            env_command("bad/name", &[]),
            env_unset("bad/name", &[String::from("A")]),
            tailscale_status("bad/name"),
            tailscale_up("bad/name"),
            tailscale_down("bad/name"),
        ] {
            let err = result.expect_err("should reject invalid app name");
            assert!(err.contains("invalid app name"), "unexpected error: {err}");
        }
    }

    #[test]
    fn health_delegation_triggers_only_for_root_without_recursion() {
        assert!(should_delegate_health_to_psht("0", false, true));
        assert!(!should_delegate_health_to_psht("0", true, true));
        assert!(!should_delegate_health_to_psht("1001", false, true));
        assert!(!should_delegate_health_to_psht("0", false, false));
    }

    #[test]
    fn has_deploy_suffix_requires_numeric_suffix() {
        assert!(has_deploy_suffix("demo-build-1772425113", "-build-"));
        assert!(has_deploy_suffix("demo-prev-1772425113", "-prev-"));
        assert!(has_deploy_suffix("demo-failed-1772425113", "-failed-"));
        assert!(!has_deploy_suffix("demo-build-next", "-build-"));
        assert!(!has_deploy_suffix("demo-build-", "-build-"));
    }

    #[test]
    fn transient_deploy_app_names_are_detected() {
        assert!(is_transient_deploy_app_name("hyperlinked-build-1772425113"));
        assert!(is_transient_deploy_app_name("hyperlinked-prev-1772425113"));
        assert!(is_transient_deploy_app_name(
            "hyperlinked-failed-1772425113"
        ));
        assert!(!is_transient_deploy_app_name("hyperlinked-build-cache"));
        assert!(!is_transient_deploy_app_name("hyperlinked"));
    }

    #[test]
    fn canonical_app_name_filters_transient_containers() {
        assert_eq!(
            canonical_app_name_from_container("psht-hyperlinked").as_deref(),
            Some("hyperlinked")
        );
        assert!(canonical_app_name_from_container("psht-hyperlinked-build-1772425113").is_none());
        assert!(canonical_app_name_from_container("psht-hyperlinked-prev-1772425113").is_none());
        assert!(canonical_app_name_from_container("psht-hyperlinked-failed-1772425113").is_none());
        assert!(canonical_app_name_from_container("other-service").is_none());
    }

    #[test]
    fn ps_container_state_interprets_runtime_status_strings() {
        assert_eq!(ps_container_state("Running"), PsContainerState::Running);
        assert_eq!(ps_container_state("running"), PsContainerState::Running);
        assert_eq!(ps_container_state("Missing"), PsContainerState::Missing);
        assert_eq!(ps_container_state("missing"), PsContainerState::Missing);
        assert_eq!(ps_container_state("Stopped"), PsContainerState::Stopped);
        assert_eq!(ps_container_state("Frozen"), PsContainerState::Stopped);
    }

    #[test]
    fn ps_status_from_parts_requires_live_app_process_for_running() {
        assert_eq!(
            ps_status_from_parts(PsContainerState::Running, Some(true)),
            "Running"
        );
        assert_eq!(
            ps_status_from_parts(PsContainerState::Running, Some(false)),
            "Down"
        );
        assert_eq!(
            ps_status_from_parts(PsContainerState::Running, None),
            "Down"
        );
        assert_eq!(
            ps_status_from_parts(PsContainerState::Stopped, None),
            "Stopped"
        );
        assert_eq!(
            ps_status_from_parts(PsContainerState::Missing, None),
            "Missing"
        );
    }

    #[test]
    fn app_ref_from_instance_name_handles_prefixed_and_unprefixed_values() {
        assert_eq!(
            app_ref_from_instance_name("psht-hyperlinked").as_deref(),
            Some("hyperlinked")
        );
        assert_eq!(
            app_ref_from_instance_name("hyperlinked-build-123").as_deref(),
            Some("hyperlinked-build-123")
        );
        assert!(app_ref_from_instance_name("").is_none());
    }

    #[test]
    fn app_runtime_state_round_trip() {
        let app = unique_test_app("runtime-state");
        write_app_runtime_state(&app, "myapp-build-123", Some("myapp")).unwrap();
        let loaded = read_app_runtime_state(&app).unwrap().unwrap();
        assert_eq!(loaded.active_instance, "psht-myapp-build-123");
        assert_eq!(loaded.previous_instance.as_deref(), Some("psht-myapp"));
        assert!(loaded.runtime_project.as_deref().is_some());
        assert!(loaded.updated_at > 0);
        clear_app_runtime_state(&app).unwrap();
    }

    #[test]
    fn desired_state_round_trip_defaults_to_running() {
        let app = unique_test_app("desired-state");
        assert_eq!(app_desired_state(&app).unwrap(), DESIRED_STATE_RUNNING);
        set_app_desired_state(&app, DESIRED_STATE_STOPPED).unwrap();
        assert_eq!(app_desired_state(&app).unwrap(), DESIRED_STATE_STOPPED);
        set_app_desired_state(&app, "weird").unwrap();
        assert_eq!(app_desired_state(&app).unwrap(), DESIRED_STATE_RUNNING);
    }

    #[test]
    fn stopped_container_is_unhealthy() {
        let report = check_app_health("hyperlinked", "hyperlinked", "Stopped");
        assert!(!report.healthy);
        assert_eq!(report.app, "hyperlinked");
        assert!(
            report
                .details
                .iter()
                .any(|detail| detail.contains("container status is Stopped"))
        );
    }

    #[test]
    fn app_runner_script_contains_exports_and_exec() {
        let mut vars = BTreeMap::new();
        vars.insert("HELLO".to_string(), "world".to_string());
        let script = app_runner_script_content(3737, "bun run index.ts", &vars).unwrap();
        assert!(script.starts_with("#!/bin/sh"));
        assert!(script.contains("cd /app"));
        assert!(script.contains("export PORT=3737"));
        assert!(script.contains("export HELLO='world'"));
        assert!(script.contains("exec sh -c 'bun run index.ts'"));
    }

    #[test]
    fn app_service_unit_references_runner_and_log_paths() {
        let unit = app_service_unit_content();
        assert!(unit.contains(APP_SERVICE_RUNNER_PATH));
        assert!(unit.contains(APP_PROCESS_LOG_PATH));
        assert!(unit.contains("Restart=always"));
    }

    #[test]
    fn app_process_probe_checks_pid_liveness() {
        let cmd = app_process_probe_cmd();
        assert!(cmd.contains(APP_PROCESS_PID_PATH));
        assert!(cmd.contains("kill -0"));
        assert!(cmd.contains("stat -c %Y"));
        assert!(cmd.contains("/proc/uptime"));
        assert!(cmd.contains("date +%s"));
        assert!(cmd.contains("echo alive"));
    }

    #[test]
    fn stop_app_process_cmd_targets_process_group_and_waits() {
        let cmd = stop_app_process_cmd_with_limits(40, 10, "0.2");
        assert!(cmd.contains("kill -TERM -- \"-$pid\""));
        assert!(cmd.contains("kill -KILL -- \"-$pid\""));
        assert!(cmd.contains("while kill -0 \"$pid\" 2>/dev/null; do"));
        assert!(cmd.contains("if [ \"$i\" -ge 40 ]"));
        assert!(cmd.contains("if [ \"$j\" -ge 10 ]"));
        assert!(cmd.contains("rm -f /var/psht/app.pid"));
    }

    #[test]
    fn stop_port_listeners_cmd_uses_ss_and_process_group_signals() {
        let cmd = stop_port_listeners_cmd_with_limits(3430, 40, 10, "0.2");
        assert!(cmd.contains("command -v ss"));
        assert!(cmd.contains("sport = :3430"));
        assert!(cmd.contains("sed -n 's/.*pid=\\([0-9][0-9]*\\).*/\\1/p'"));
        assert!(cmd.contains("kill -TERM -- \"-$pid\""));
        assert!(cmd.contains("kill -KILL -- \"-$pid\""));
        assert!(cmd.contains("listener process(es) on port 3430 did not exit"));
    }

    #[test]
    fn parse_env_assignment_accepts_empty_value() {
        let (name, value) = parse_env_assignment("A=").unwrap();
        assert_eq!(name, "A");
        assert_eq!(value, "");
    }

    #[test]
    fn parse_env_assignment_rejects_invalid_name() {
        let err = parse_env_assignment("1BAD=value").unwrap_err();
        assert!(err.contains("invalid env name"));
    }

    #[test]
    fn env_vars_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = env_path_in(tmp.path(), "myapp");
        let mut vars = BTreeMap::new();
        vars.insert("A".to_string(), "1".to_string());
        vars.insert("B".to_string(), "2".to_string());
        write_env_vars_to(&path, &vars).unwrap();
        let loaded = read_env_vars_from(&path).unwrap();
        assert_eq!(loaded, vars);
    }

    #[test]
    fn required_env_check_reports_missing() {
        let required = vec!["DATABASE_URL".to_string(), "JWT_SECRET".to_string()];
        let mut vars = BTreeMap::new();
        vars.insert("DATABASE_URL".to_string(), "postgres://x".to_string());
        let err = ensure_required_env_present(&required, &vars).unwrap_err();
        assert!(err.contains("JWT_SECRET"));
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("echo 'hi'"), "'echo '\"'\"'hi'\"'\"''");
    }

    #[test]
    fn write_start_command_cmd_rejects_empty() {
        let err = write_start_command_cmd(" \n ").unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn write_start_command_cmd_targets_metadata_path() {
        let cmd = write_start_command_cmd("./app --flag").unwrap();
        assert!(cmd.contains(START_COMMAND_PATH));
        assert!(cmd.contains("printf '%s\\n'"));
    }

    #[test]
    fn write_ssh_login_env_cmd_writes_profile_exports() {
        let mut vars = BTreeMap::new();
        vars.insert("A".to_string(), "1".to_string());
        vars.insert("B".to_string(), "two words".to_string());
        let cmd = write_ssh_login_env_cmd(&vars).unwrap();
        assert!(cmd.contains(SSH_LOGIN_ENV_PATH));
        assert!(cmd.contains("mkdir -p /etc/profile.d"));
        assert!(cmd.contains("chmod 644"));
    }

    #[test]
    fn write_ssh_login_env_cmd_rejects_invalid_name() {
        let mut vars = BTreeMap::new();
        vars.insert("1BAD".to_string(), "value".to_string());
        let err = ssh_login_env_content(&vars).unwrap_err();
        assert!(err.contains("invalid env name"));
    }

    #[test]
    fn write_ssh_login_env_cmd_handles_empty_vars() {
        let vars = BTreeMap::new();
        let content = ssh_login_env_content(&vars).unwrap();
        assert!(content.contains("Generated by psht"));
        assert!(!content.contains("export "));
    }

    #[test]
    fn ssh_login_env_content_writes_quoted_exports() {
        let mut vars = BTreeMap::new();
        vars.insert("A".to_string(), "1".to_string());
        vars.insert("B".to_string(), "two words".to_string());
        let content = ssh_login_env_content(&vars).unwrap();
        assert!(content.contains("export A='1'"));
        assert!(content.contains("export B='two words'"));
    }

    #[test]
    fn supervise_service_unit_sets_execstart_and_home() {
        let unit =
            supervise_service_unit_content("/opt/psht/bin/psht-server", Path::new("/home/psht"));
        assert!(unit.contains("ExecStart=/opt/psht/bin/psht-server daemon"));
        assert!(unit.contains("Environment=HOME=/home/psht"));
        assert!(unit.contains("User=psht"));
        assert!(unit.contains("Restart=always"));
    }

    #[test]
    fn web_service_unit_sets_execstart_bind_and_port() {
        let unit = web_service_unit_content(
            "/opt/psht/bin/psht-server",
            Path::new("/home/psht"),
            "100.64.0.1",
            8788,
        );
        assert!(unit.contains(
            "ExecStart=/opt/psht/bin/psht-server web serve --bind 100.64.0.1 --port 8788"
        ));
        assert!(unit.contains("Environment=HOME=/home/psht"));
        assert!(unit.contains("User=psht"));
        assert!(unit.contains("Restart=always"));
    }

    #[test]
    fn run_hook_skips_missing_or_blank_commands() {
        run_hook("myapp", "preinstall", None).unwrap();
        run_hook("myapp", "postinstall", Some(" \n\t ")).unwrap();
    }

    #[test]
    fn app_workdir_command_wraps_nonempty_command() {
        let cmd = app_workdir_command("cargo build --release").unwrap();
        assert_eq!(cmd, "cd /app && cargo build --release");
    }

    #[test]
    fn app_workdir_command_skips_blank_command() {
        assert!(app_workdir_command(" \n\t ").is_none());
    }

    #[test]
    fn apt_install_command_skips_empty_packages() {
        assert!(apt_install_command(&[]).is_none());
        assert!(apt_install_command(&["  ".to_string()]).is_none());
    }

    #[test]
    fn apt_install_command_builds_noninteractive_install() {
        let cmd = apt_install_command(&["curl".to_string(), "libssl-dev".to_string()]).unwrap();
        assert!(cmd.contains("DEBIAN_FRONTEND=noninteractive"));
        assert!(cmd.contains("max_attempts="));
        assert!(cmd.contains("status=1"));
        assert!(cmd.contains("apt-get update"));
        assert!(cmd.contains("apt-get install -y"));
        assert!(cmd.contains("'curl'"));
        assert!(cmd.contains("'libssl-dev'"));
    }

    #[test]
    fn extract_missing_apt_packages_parses_common_resolution_errors() {
        let missing = extract_missing_apt_packages(
            "Package chromium-browser is not available, but is referred to by another package.\nE: Unable to locate package foo-dev\nE: Package 'bar' has no installation candidate\nE: Package baz has no installation candidate\n",
        );
        assert_eq!(
            missing,
            vec![
                "chromium-browser".to_string(),
                "foo-dev".to_string(),
                "bar".to_string(),
                "baz".to_string()
            ]
        );
    }

    #[test]
    fn apt_policy_output_indicates_unavailable_handles_missing_candidates() {
        assert!(apt_policy_output_indicates_unavailable(
            "chromium-browser:\n  Installed: (none)\n  Candidate: (none)\n"
        ));
        assert!(apt_policy_output_indicates_unavailable(
            "N: Unable to locate package chromium-browser"
        ));
        assert!(!apt_policy_output_indicates_unavailable(
            "curl:\n  Installed: (none)\n  Candidate: 8.5.0\n"
        ));
    }

    #[test]
    fn logged_command_wrapper_preserves_explicit_exit_status() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("install.log");
        let wrapped =
            logged_command_wrapper("printf 'hello\\n'; exit 7", &log_path.to_string_lossy());
        let status = Command::new("sh")
            .args(["-c", &wrapped])
            .status()
            .expect("failed to execute wrapped shell");
        assert_eq!(status.code(), Some(7));
        let log = fs::read_to_string(&log_path).expect("failed to read log");
        assert!(log.contains("hello"));
    }

    #[test]
    fn logged_command_wrapper_preserves_successful_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("setup.log");
        let wrapped = logged_command_wrapper("printf 'ok\\n'; exit 0", &log_path.to_string_lossy());
        let status = Command::new("sh")
            .args(["-c", &wrapped])
            .status()
            .expect("failed to execute wrapped shell");
        assert_eq!(status.code(), Some(0));
        let log = fs::read_to_string(&log_path).expect("failed to read log");
        assert!(log.contains("ok"));
    }

    #[test]
    fn resource_pressure_error_detection_matches_fork_failures() {
        assert!(is_resource_pressure_error(
            "dpkg: unrecoverable fatal error, aborting: fork failed: Resource temporarily unavailable"
        ));
        assert!(is_resource_pressure_error(
            "cannot allocate memory while forking"
        ));
        assert!(!is_resource_pressure_error(
            "cargo build failed: unresolved import"
        ));
    }

    #[test]
    fn normalize_candidate_app_ref_handles_prefixed_values() {
        assert_eq!(
            normalize_candidate_app_ref("psht-hyperlinked-build-123").as_deref(),
            Some("hyperlinked-build-123")
        );
        assert_eq!(
            normalize_candidate_app_ref("hyperlinked-build-123").as_deref(),
            Some("hyperlinked-build-123")
        );
        assert!(normalize_candidate_app_ref("   ").is_none());
    }

    #[test]
    fn setup_failure_classifier_distinguishes_transient_and_nonretryable() {
        assert_eq!(
            classify_setup_failure("dpkg: fork failed: Resource temporarily unavailable"),
            SetupFailureClass::Transient
        );
        assert_eq!(
            classify_setup_failure("apt-get update failed to fetch index"),
            SetupFailureClass::Transient
        );
        assert_eq!(
            classify_setup_failure(
                "setup script failed: package foo has no installation candidate"
            ),
            SetupFailureClass::NonRetryable
        );
    }

    #[test]
    fn strip_internal_deploy_error_markers_handles_setup_markers() {
        let transient_marked = mark_setup_transient_failure("transient setup".to_string());
        assert_eq!(
            strip_internal_deploy_error_markers(&transient_marked),
            "transient setup"
        );

        let nonretryable_marked = mark_setup_nonretryable_failure("nonretryable setup".to_string());
        assert_eq!(
            strip_internal_deploy_error_markers(&nonretryable_marked),
            "nonretryable setup"
        );
    }
}
