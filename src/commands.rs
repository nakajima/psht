use std::collections::{BTreeMap, hash_map::DefaultHasher};
use std::env;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::app_name;
use crate::caddy;
use crate::container;
use crate::detect;
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
const APP_PROCESS_PID_PATH: &str = "/var/psht/app.pid";
const APP_PROCESS_LOG_PATH: &str = "/var/psht/app.log";
const INSTALL_LOG_PATH: &str = "/var/psht/install.log";
const APP_PROCESS_POLL_SLEEP: &str = "0.2";
const APP_PROCESS_STOP_TERM_CHECKS: u32 = 40;
const APP_PROCESS_STOP_KILL_CHECKS: u32 = 10;
const APP_PROCESS_START_WAIT_CHECKS: u32 = 25;
const APP_PROCESS_EARLY_EXIT_CHECK_GRACE_SECS: u64 = 3;
const APP_LOG_TAIL_LINES: u32 = 40;
const INSTALL_LOG_TAIL_LINES: u32 = 80;
const CONTAINER_OP_WAIT_CHECKS: u32 = 80;
const CONTAINER_OP_WAIT_SLEEP_MS: u64 = 500;
const CONTAINER_DELETE_RETRY_CHECKS: u32 = 20;
const DEPLOY_LOCK_STALE_SECS: u64 = 6 * 60 * 60;
const UPGRADE_CHECK_TTL_SECS: u64 = 6 * 60 * 60;
const TAILSCALE_ONLINE_WAIT_SECS: u64 = 20;
const TAILSCALE_ONLINE_WAIT_POLL_MS: u64 = 500;
const DEPLOY_TCP_READY_TIMEOUT_SECS: u64 = 60;
const DEPLOY_PROGRESS_HEARTBEAT_SECS: u64 = 10;

fn home_dir() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/home/psht".to_string()))
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

fn git_deploy_state_dir() -> PathBuf {
    home_dir().join("deploy-state")
}

fn upgrade_check_state_path() -> PathBuf {
    home_dir().join(".psht").join("upgrade-check.toml")
}

fn deploy_lock_dir() -> PathBuf {
    home_dir().join("deploy-locks")
}

fn deploy_pending_dir() -> PathBuf {
    home_dir().join("deploy-pending")
}

fn app_runtime_state_dir() -> PathBuf {
    home_dir().join(".psht").join("apps")
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

fn run_cmd_in(program: &str, args: &[&str], cwd: &Path) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
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

fn run_cmd_capture_in(program: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
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
    Some(name.trim_end_matches('.').to_string())
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

fn tailscale_ssh_enabled() -> Result<bool, String> {
    let json = run_cmd_capture("tailscale", &["status", "--json"])?;
    let value: serde_json::Value = serde_json::from_str(&json)
        .map_err(|e| format!("failed to parse tailscale status: {e}"))?;

    if value
        .pointer("/Self/SSH")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(true);
    }

    Ok(json.contains("\"SSH\":true"))
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
    let json = run_cmd_capture("incus", &["storage", "list", "--format=json"])?;
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
    if let Ok(pool) = run_cmd_capture(
        "incus",
        &["profile", "device", "get", "default", "root", "pool"],
    ) {
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

fn ensure_app_storage_volume(app: &str) -> Result<(String, String), String> {
    let pool = default_storage_pool()?;
    let volume = app_storage_volume_name(app);
    let show_args = vec!["storage", "volume", "show", pool.as_str(), volume.as_str()];
    if !command_succeeds("incus", &show_args) {
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
    if !command_succeeds("incus", &show_args) {
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct GitCheckoutTarget {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum GitDeployStatus {
    Pending,
    Success,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct GitDeployState {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
    status: GitDeployStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct AppRuntimeState {
    active_instance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_instance: Option<String>,
    updated_at: u64,
}

fn app_runtime_state_path_in(dir: &Path, app: &str) -> PathBuf {
    dir.join(format!("{app}.toml"))
}

fn app_runtime_state_path(app: &str) -> PathBuf {
    app_runtime_state_path_in(&app_runtime_state_dir(), app)
}

fn app_ref_from_instance_name(instance: &str) -> Option<String> {
    let trimmed = instance.trim();
    if trimmed.is_empty() {
        return None;
    }
    let app_ref = trimmed.strip_prefix("psht-").unwrap_or(trimmed).trim();
    if app_ref.is_empty() {
        return None;
    }
    Some(app_ref.to_string())
}

fn instance_name_from_app_ref(app_ref: &str) -> String {
    let app_ref = app_ref.trim();
    if app_ref.starts_with("psht-") {
        app_ref.to_string()
    } else {
        format!("psht-{app_ref}")
    }
}

fn read_app_runtime_state_from(path: &Path) -> Result<Option<AppRuntimeState>, String> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
    };
    let state: AppRuntimeState =
        toml::from_str(&content).map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
    Ok(Some(state))
}

fn read_app_runtime_state(app: &str) -> Result<Option<AppRuntimeState>, String> {
    read_app_runtime_state_from(&app_runtime_state_path(app))
}

fn write_app_runtime_state_to(path: &Path, state: &AppRuntimeState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let content = toml::to_string_pretty(state)
        .map_err(|e| format!("failed to serialize {}: {e}", path.display()))?;
    fs::write(path, content).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

fn write_app_runtime_state(
    app: &str,
    active_app_ref: &str,
    previous_app_ref: Option<&str>,
) -> Result<(), String> {
    let state = AppRuntimeState {
        active_instance: instance_name_from_app_ref(active_app_ref),
        previous_instance: previous_app_ref.map(instance_name_from_app_ref),
        updated_at: now_unix_secs(),
    };
    write_app_runtime_state_to(&app_runtime_state_path(app), &state)
}

fn clear_app_runtime_state(app: &str) -> Result<(), String> {
    let path = app_runtime_state_path(app);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to remove {}: {e}", path.display())),
    }
}

fn read_all_app_runtime_states() -> Result<Vec<(String, AppRuntimeState)>, String> {
    let dir = app_runtime_state_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("failed to read {}: {e}", dir.display())),
    };

    let mut states = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("failed to read entry in {}: {e}", dir.display()))?;
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|value| value.to_str()) else {
            continue;
        };
        if ext != "toml" {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(state) = read_app_runtime_state_from(&path)? else {
            continue;
        };
        states.push((stem.to_string(), state));
    }
    states.sort_by(|(left, _), (right, _)| left.cmp(right));
    Ok(states)
}

fn resolve_active_app_ref(app: &str) -> Result<Option<String>, String> {
    if let Some(state) = read_app_runtime_state(app)? {
        if let Some(active_app_ref) = app_ref_from_instance_name(&state.active_instance)
            && container::exists(&active_app_ref)
        {
            return Ok(Some(active_app_ref));
        }
        if let Some(previous_instance) = state.previous_instance.as_deref()
            && let Some(previous_app_ref) = app_ref_from_instance_name(previous_instance)
            && container::exists(&previous_app_ref)
        {
            write_app_runtime_state(app, &previous_app_ref, None)?;
            return Ok(Some(previous_app_ref));
        }
        if container::exists(app) {
            write_app_runtime_state(app, app, None)?;
            return Ok(Some(app.to_string()));
        }
        return Ok(None);
    }

    if container::exists(app) {
        write_app_runtime_state(app, app, None)?;
        return Ok(Some(app.to_string()));
    }

    Ok(None)
}

fn resolve_existing_active_app_ref(app: &str) -> Result<String, String> {
    let Some(active_app_ref) = resolve_active_app_ref(app)? else {
        return Err(format!("app '{app}' not found"));
    };
    Ok(active_app_ref)
}

fn git_deploy_state_path_in(dir: &Path, app: &str) -> PathBuf {
    dir.join(format!("{app}.toml"))
}

fn git_deploy_state_path(app: &str) -> PathBuf {
    git_deploy_state_path_in(&git_deploy_state_dir(), app)
}

fn deploy_lock_path_in(dir: &Path, app: &str) -> PathBuf {
    dir.join(format!("{app}.lock"))
}

fn deploy_lock_path(app: &str) -> PathBuf {
    deploy_lock_path_in(&deploy_lock_dir(), app)
}

fn pending_git_target_path_in(dir: &Path, app: &str) -> PathBuf {
    dir.join(format!("{app}.toml"))
}

fn pending_git_target_path(app: &str) -> PathBuf {
    pending_git_target_path_in(&deploy_pending_dir(), app)
}

struct DeployLockGuard {
    path: PathBuf,
}

impl Drop for DeployLockGuard {
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

fn write_new_lock_file(path: &Path) -> Result<(), std::io::Error> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let body = format!("pid={}\ncreated={}\n", std::process::id(), now_unix_secs());
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

fn read_git_deploy_state_from(path: &Path) -> Result<Option<GitDeployState>, String> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
    };
    let state: GitDeployState =
        toml::from_str(&content).map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
    Ok(Some(state))
}

fn read_git_deploy_state(app: &str) -> Result<Option<GitDeployState>, String> {
    read_git_deploy_state_from(&git_deploy_state_path(app))
}

fn write_git_deploy_state_to(path: &Path, state: &GitDeployState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let content = toml::to_string_pretty(state)
        .map_err(|e| format!("failed to serialize {}: {e}", path.display()))?;
    fs::write(path, content).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

fn write_git_deploy_state(
    app: &str,
    target: &GitCheckoutTarget,
    status: GitDeployStatus,
) -> Result<(), String> {
    let state = GitDeployState {
        ref_name: target.ref_name.clone(),
        sha: target.sha.clone(),
        status,
    };
    write_git_deploy_state_to(&git_deploy_state_path(app), &state)
}

fn clear_git_deploy_state_at(path: &Path) -> Result<(), String> {
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to remove {}: {e}", path.display())),
    }
}

fn clear_git_deploy_state(app: &str) -> Result<(), String> {
    clear_git_deploy_state_at(&git_deploy_state_path(app))
}

fn write_pending_git_target_to(path: &Path, target: &GitCheckoutTarget) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let content = toml::to_string_pretty(target)
        .map_err(|e| format!("failed to serialize {}: {e}", path.display()))?;
    fs::write(path, content).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

fn write_pending_git_target(app: &str, target: &GitCheckoutTarget) -> Result<(), String> {
    write_pending_git_target_to(&pending_git_target_path(app), target)
}

fn read_pending_git_target_from(path: &Path) -> Result<Option<GitCheckoutTarget>, String> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("failed to read {}: {e}", path.display())),
    };
    let target: GitCheckoutTarget =
        toml::from_str(&content).map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
    Ok(Some(target))
}

fn take_pending_git_target_from(path: &Path) -> Result<Option<GitCheckoutTarget>, String> {
    let target = read_pending_git_target_from(path)?;
    if target.is_none() {
        return Ok(None);
    }
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(format!("failed to remove {}: {e}", path.display())),
    }
    Ok(target)
}

fn take_pending_git_target(app: &str) -> Result<Option<GitCheckoutTarget>, String> {
    take_pending_git_target_from(&pending_git_target_path(app))
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
            let status = Command::new("git")
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
            let status = Command::new("git")
                .args(["clone", "--no-checkout"])
                .arg(repo_dir)
                .arg(build_dir)
                .status()
                .map_err(|e| format!("failed to checkout code: {e}"))?;
            if !status.success() {
                return Err("git clone failed".to_string());
            }

            let checkout_target = checkout_ref_target(&target.ref_name);
            run_cmd_in(
                "git",
                &["checkout", "--detach", &checkout_target],
                build_dir,
            )
            .map_err(|e| format!("failed to checkout {}: {e}", target.ref_name))?;

            let object_type =
                run_cmd_capture_in("git", &["cat-file", "-t", &target.sha], build_dir)
                    .map_err(|e| format!("failed to resolve pushed object {}: {e}", target.sha))?;
            if object_type == "commit" {
                let head = run_cmd_capture_in("git", &["rev-parse", "HEAD"], build_dir)?;
                if head != target.sha {
                    return Err(format!(
                        "checked out commit {head} does not match pushed commit {}",
                        target.sha
                    ));
                }
            } else if object_type == "tag" {
                let resolved =
                    run_cmd_capture_in("git", &["rev-parse", &target.ref_name], build_dir)
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

fn deploy_instance_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs();
    format!("{ts}")
}

fn candidate_app_name(app: &str, deploy_id: &str) -> String {
    format!("{app}-build-{deploy_id}")
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

fn start_cmd(port: u16, cmd: &str, vars: &BTreeMap<String, String>) -> Result<String, String> {
    let escaped = shell_quote(cmd);
    let exports = start_exports(port, vars)?;
    Ok(format!(
        "mkdir -p /var/psht && cd /app && {exports} && {{ setsid sh -c {escaped} > {APP_PROCESS_LOG_PATH} 2>&1 < /dev/null & echo $! > {APP_PROCESS_PID_PATH}; }}"
    ))
}

fn app_process_probe_cmd() -> String {
    format!("test -s {APP_PROCESS_PID_PATH} && kill -0 $(cat {APP_PROCESS_PID_PATH}) 2>/dev/null")
}

fn app_process_is_running(app: &str) -> Result<bool, String> {
    let probe = app_process_probe_cmd();
    let output = container::exec_output(app, &format!("if {probe}; then echo alive; fi; true"))?;
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
    let launch = start_cmd(port, cmd, vars)?;
    container::exec_cmd(app, &launch)?;
    for _ in 0..APP_PROCESS_START_WAIT_CHECKS {
        if app_process_is_running(app)? {
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

fn run_install_command_with_logging(app: &str, command: &str, label: &str) -> Result<(), String> {
    let wrapped = format!(
        r#"mkdir -p /var/psht
status_file="$(mktemp)"
( {command}; echo "$?" > "$status_file" ) 2>&1 | tee {INSTALL_LOG_PATH}
status="$(cat "$status_file" 2>/dev/null || echo 1)"
rm -f "$status_file"
exit "$status""#
    );
    container::exec_cmd_rolling(app, &wrapped, 5).map_err(|e| {
        let mut message = format!("{label} failed: {e}");
        if let Some(log_excerpt) = install_log_tail(app, INSTALL_LOG_TAIL_LINES) {
            message.push_str("\nLast install/build log lines:\n");
            message.push_str(&log_excerpt);
        }
        message
    })
}

fn run_hook(app: &str, phase: &str, command: Option<&str>) -> Result<(), String> {
    let Some(command) = command.and_then(app_workdir_command) else {
        return Ok(());
    };
    eprintln!("-----> Running {phase} hook");
    container::exec_cmd_rolling(app, &command, 5).map_err(|e| format!("{phase} hook failed: {e}"))
}

fn apt_install_command(packages: &[String]) -> Option<String> {
    let packages: Vec<String> = packages
        .iter()
        .map(|pkg| pkg.trim())
        .filter(|pkg| !pkg.is_empty())
        .map(ToString::to_string)
        .collect();
    if packages.is_empty() {
        return None;
    }
    let quoted = packages
        .iter()
        .map(|pkg| shell_quote(pkg))
        .collect::<Vec<_>>()
        .join(" ");
    Some(format!(
        "export DEBIAN_FRONTEND=noninteractive && apt-get update -qq && apt-get install -y -qq {quoted}"
    ))
}

fn install_apt_packages(app: &str, packages: &[String]) -> Result<(), String> {
    let Some(command) = apt_install_command(packages) else {
        return Ok(());
    };
    eprintln!("-----> Installing apt packages");
    container::exec_cmd_rolling(app, &command, 5)
        .map_err(|e| format!("apt package install failed: {e}"))
}

fn wait_for_container_operation_quiet(app: &str, project: &str) -> Result<(), String> {
    let mut announced_wait = false;
    for _ in 0..CONTAINER_OP_WAIT_CHECKS {
        if !container::has_running_operation_in_project(app, project)? {
            if announced_wait {
                eprintln!("       Active operation finished");
            }
            return Ok(());
        }
        if !announced_wait {
            eprintln!("       Waiting for active container operation to finish...");
            announced_wait = true;
        }
        thread::sleep(Duration::from_millis(CONTAINER_OP_WAIT_SLEEP_MS));
    }
    Err(format!(
        "container '{app}' is busy with an active operation; retry deploy after it completes"
    ))
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
    wait_for_container_operation_quiet(app, project)?;

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

fn deploy_once(app: &str, target: Option<&GitCheckoutTarget>, force: bool) -> Result<(), String> {
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

    eprintln!("-----> Checking out code");
    if let Some(target) = target {
        eprintln!("       Ref: {} ({})", target.ref_name, target.sha);
    }
    let result = (|| {
        let build_dir = checkout_code(app, target)?;
        deploy_from(app, &build_dir)
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
            if let Some(target) = target
                && let Err(write_err) = write_git_deploy_state(app, target, GitDeployStatus::Failed)
            {
                eprintln!("       Warning: failed to record failed git deploy state: {write_err}");
            }
            Err(err)
        }
    }
}

fn queue_pending_git_deploy(app: &str, target: &GitCheckoutTarget) -> Result<(), String> {
    write_pending_git_target(app, target)?;
    if let Err(err) = write_git_deploy_state(app, target, GitDeployStatus::Pending) {
        eprintln!("       Warning: failed to record pending git deploy state: {err}");
    }
    Ok(())
}

pub fn deploy(
    app: &str,
    git_ref: Option<&str>,
    git_sha: Option<&str>,
    force: bool,
) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    eprintln!("-----> Deploying {app}");
    warn_if_upgrade_available();
    let mut target = parse_git_checkout_target(git_ref, git_sha)?;

    let Some(_deploy_lock) = try_acquire_deploy_lock(app)? else {
        if let Some(target) = target.as_ref() {
            queue_pending_git_deploy(app, target)?;
            eprintln!("-----> Deploy already in progress; replaced pending deploy target");
            eprintln!("       Ref: {} ({})", target.ref_name, target.sha);
            return Ok(());
        }
        return Err(format!(
            "deploy already in progress for '{app}'; retry after it completes"
        ));
    };

    let mut active_force = force;
    loop {
        let result = deploy_once(app, target.as_ref(), active_force);
        let Some(pending_target) = take_pending_git_target(app)? else {
            return result;
        };
        if target.as_ref().map(|v| v.sha.as_str()) == Some(pending_target.sha.as_str()) {
            return result;
        }

        eprintln!("-----> Processing pending deploy target");
        eprintln!(
            "       Ref: {} ({})",
            pending_target.ref_name, pending_target.sha
        );
        target = Some(pending_target);
        active_force = false;
    }
}

pub fn push(app: &str, force: bool) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    eprintln!("-----> Deploying {app}");
    warn_if_upgrade_available();
    let Some(_deploy_lock) = try_acquire_deploy_lock(app)? else {
        return Err(format!(
            "deploy already in progress for '{app}'; retry after it completes"
        ));
    };

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

    deploy_from(app, &code_dir)?;
    if let Err(err) = clear_git_deploy_state(app) {
        eprintln!("       Warning: failed to clear git deploy state: {err}");
    }
    Ok(())
}

fn deploy_from(app: &str, code_dir: &Path) -> Result<(), String> {
    if resolve_active_app_ref(app)?.is_some() {
        deploy_from_blue_green(app, code_dir)
    } else {
        deploy_from_in_place(app, code_dir)
    }
}

fn deploy_from_blue_green(app: &str, code_dir: &Path) -> Result<(), String> {
    let deploy_started = Instant::now();
    let current_uid = run_cmd_capture("id", &["-u"])?;
    let current_project = format!("user-{}", current_uid.trim());
    if command_succeeds("incus", &["project", "show", &current_project]) {
        ensure_project_default_profile(&current_project)?;
    }
    init_stacks_in(&stacks_dir())?;
    let old_active_app = resolve_active_app_ref(app)?
        .ok_or_else(|| format!("app '{app}' not found for blue/green deploy"))?;

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

    if code_dir.join("psht-stack.sh").exists() {
        eprintln!("       Using custom stack");
    }

    let (stack, script_path) = resolve_stack(app, code_dir, config.stack())?;
    let hash = stack_hash(&script_path)?;
    let port = allocate_port(app);
    let skip_tailscale = env::var_os("PSHT_SKIP_TAILSCALE").is_some();
    let (storage_pool, storage_volume) = ensure_app_storage_volume(app)?;

    let deploy_id = deploy_instance_id();
    let candidate_app = candidate_app_name(app, &deploy_id);

    if container::exists(&candidate_app) {
        let _ = cleanup_container_for_rebuild(&candidate_app, &current_project);
    }

    eprintln!("-----> Preparing candidate container");
    eprintln!("       Candidate: {candidate_app}");
    eprintln!("       Traffic remains on current container");
    wait_for_container_operation_quiet(&old_active_app, &current_project)?;

    let build_candidate_result = (|| -> Result<(), String> {
        if container::image_exists_in_project(&stack, &hash, &current_project) {
            eprintln!("-----> Creating candidate from cached image");
            container::create_from_image_in_project(
                &candidate_app,
                &stack,
                &hash,
                &current_project,
            )?;
            if skip_tailscale {
                eprintln!("-----> Skipping tailscale setup in candidate");
            } else {
                eprintln!("-----> Installing tailscale in candidate");
                tailscale::install_in_container(&candidate_app)?;
            }
        } else {
            eprintln!("-----> Creating candidate container");
            eprintln!("       First run may take a while while Ubuntu image downloads");
            ensure_create_prereqs(&current_project)?;
            container::create_in_project(&candidate_app, &current_project)?;

            if skip_tailscale {
                eprintln!("-----> Skipping tailscale setup in candidate");
            } else {
                eprintln!("-----> Installing tailscale in candidate");
                tailscale::install_in_container(&candidate_app)?;
            }

            eprintln!("-----> Setting up candidate runtime");
            container::push_file(
                &candidate_app,
                &script_path.to_string_lossy(),
                "/tmp/setup.sh",
            )?;
            container::exec_cmd_rolling(
                &candidate_app,
                "chmod +x /tmp/setup.sh && /tmp/setup.sh",
                5,
            )?;

            eprintln!("-----> Caching stack image");
            if let Err(e) =
                container::publish_image_in_project(&candidate_app, &stack, &hash, &current_project)
            {
                eprintln!("       Warning: failed to cache stack image: {e}");
            }
        }

        container::exec_cmd(
            &candidate_app,
            &format!("echo -n '{hash}' > /etc/psht-setup-hash"),
        )?;

        eprintln!("-----> Building candidate");
        container::push_code(&candidate_app, &code_dir.to_string_lossy())?;
        install_apt_packages(&candidate_app, &config.apt_packages)?;
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
        Ok(())
    })();

    if let Err(err) = build_candidate_result {
        let _ = cleanup_container_for_rebuild(&candidate_app, &current_project);
        return Err(err);
    }

    eprintln!("-----> Switching traffic");
    eprintln!("       Traffic remains on current container until cutover is complete");
    let mut old_proxy_removed = false;
    let mut old_storage_detached = false;
    let mut candidate_storage_attached = false;
    let mut candidate_proxy_attached = false;

    let cutover_result = (|| -> Result<Option<String>, String> {
        eprintln!("       Stopping current app process");
        stop_app_process_on_port(&old_active_app, port)?;

        eprintln!("       Removing old proxy device");
        container::remove_proxy(&old_active_app)?;
        old_proxy_removed = true;

        eprintln!("       Detaching storage from current container");
        if let Err(e) = container::remove_storage_mount(&old_active_app) {
            eprintln!("       Warning: failed to detach storage from current container: {e}");
        }
        old_storage_detached = true;

        eprintln!("       Attaching storage to candidate container");
        container::ensure_storage_mount(&candidate_app, &storage_pool, &storage_volume)?;
        candidate_storage_attached = true;

        eprintln!("       Attaching proxy to new active container");
        container::add_proxy(&candidate_app, port, port)?;
        candidate_proxy_attached = true;

        eprintln!("       Starting app in new active container");
        launch_app_process(&candidate_app, port, &config.start_command, &app_env)?;

        eprintln!("-----> Waiting for candidate readiness");
        wait_for_tcp_listener(
            &candidate_app,
            port,
            DEPLOY_TCP_READY_TIMEOUT_SECS,
            "candidate readiness",
        )?;

        let tailnet_hostname = if skip_tailscale {
            None
        } else {
            eprintln!("       Connecting new active container to tailnet");
            let name = tailscale::join_in_container(&candidate_app)?;
            if let Err(e) = tailscale::expose_http_in_container(&candidate_app, port) {
                eprintln!("       Warning: failed to expose tailnet HTTP on :80: {e}");
            }
            name
        };

        caddy::add(app, port)?;
        write_app_runtime_state(app, &candidate_app, Some(&old_active_app))?;

        Ok(tailnet_hostname)
    })();

    let tailnet_hostname = match cutover_result {
        Ok(name) => name,
        Err(err) => {
            eprintln!("-----> Cutover failed; rolling back traffic");

            if candidate_proxy_attached {
                let _ = container::remove_proxy(&candidate_app);
            }
            if candidate_storage_attached {
                let _ = container::remove_storage_mount(&candidate_app);
            }

            if old_storage_detached {
                let _ = container::ensure_storage_mount(
                    &old_active_app,
                    &storage_pool,
                    &storage_volume,
                );
            }
            if old_proxy_removed {
                let _ = container::add_proxy(&old_active_app, port, port);
            }

            if let Ok(restored_start_command) = read_start_command(&old_active_app) {
                let _ =
                    launch_app_process(&old_active_app, port, &restored_start_command, &app_env);
            }
            if !skip_tailscale && tailscale::dns_name_in_container(&old_active_app).is_some() {
                let _ = tailscale::expose_http_in_container(&old_active_app, port);
            }
            let _ = caddy::add(app, port);
            let _ = cleanup_container_for_rebuild(&candidate_app, &current_project);

            return Err(format!(
                "deploy cutover failed and rollback was applied: {err}"
            ));
        }
    };

    eprintln!("       Cleaning up previous active container");
    if !skip_tailscale && let Err(e) = container::exec_cmd(&old_active_app, "tailscale down") {
        eprintln!("       Warning: failed to bring tailscale down on previous container: {e}");
    }
    if let Err(e) = cleanup_container_for_rebuild(&old_active_app, &current_project) {
        eprintln!("       Warning: failed to clean previous active container: {e}");
    } else if let Err(e) = write_app_runtime_state(app, &candidate_app, None) {
        eprintln!("       Warning: failed to update app runtime state after cleanup: {e}");
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
        DEPLOY_TCP_READY_TIMEOUT_SECS,
        "post-cutover verification",
    )?;

    eprintln!(
        "=====> App {app} deployed on port {port} (build {build_number}, {}s)",
        deploy_started.elapsed().as_secs()
    );
    Ok(())
}

fn deploy_from_in_place(app: &str, code_dir: &Path) -> Result<(), String> {
    let current_uid = run_cmd_capture("id", &["-u"])?;
    let current_project = format!("user-{}", current_uid.trim());
    if command_succeeds("incus", &["project", "show", &current_project]) {
        ensure_project_default_profile(&current_project)?;
    }
    init_stacks_in(&stacks_dir())?;

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

    if code_dir.join("psht-stack.sh").exists() {
        eprintln!("       Using custom stack");
    }

    let (stack, script_path) = resolve_stack(app, code_dir, config.stack())?;
    let hash = stack_hash(&script_path)?;
    let skip_tailscale = env::var_os("PSHT_SKIP_TAILSCALE").is_some();

    let port = allocate_port(app);
    let mut tailnet_hostname = if skip_tailscale {
        None
    } else {
        tailscale::dns_name_in_container(app)
    };
    let needs_setup = if container::exists(app) {
        let remote_hash = container::exec_output(app, "cat /etc/psht-setup-hash 2>/dev/null")
            .unwrap_or_default()
            .trim()
            .to_string();
        if remote_hash == hash {
            eprintln!("-----> Reusing container");
            stop_app_process_on_port(app, port)?;
            false
        } else {
            eprintln!("-----> Rebuilding container");
            cleanup_container_for_rebuild(app, &current_project)?;
            true
        }
    } else {
        true
    };

    if needs_setup {
        wait_for_container_operation_quiet(app, &current_project)?;

        if container::image_exists_in_project(&stack, &hash, &current_project) {
            eprintln!("-----> Creating container from cached image");
            container::create_from_image_in_project(app, &stack, &hash, &current_project)?;

            if skip_tailscale {
                eprintln!("-----> Skipping tailscale setup");
            } else {
                eprintln!("-----> Installing tailscale");
                tailscale::install_in_container(app)?;
            }
        } else {
            eprintln!("-----> Creating container");
            eprintln!("       First run may take a while while Ubuntu image downloads");
            ensure_create_prereqs(&current_project)?;
            container::create_in_project(app, &current_project)?;

            if skip_tailscale {
                eprintln!("-----> Skipping tailscale setup");
            } else {
                eprintln!("-----> Installing tailscale");
                tailscale::install_in_container(app)?;
            }

            eprintln!("-----> Setting up runtime");
            container::push_file(app, &script_path.to_string_lossy(), "/tmp/setup.sh")?;
            container::exec_cmd_rolling(app, "chmod +x /tmp/setup.sh && /tmp/setup.sh", 5)?;

            eprintln!("-----> Caching stack image");
            if let Err(e) =
                container::publish_image_in_project(app, &stack, &hash, &current_project)
            {
                eprintln!("       Warning: failed to cache stack image: {e}");
            }
        }

        container::exec_cmd(app, &format!("echo -n '{hash}' > /etc/psht-setup-hash"))?;

        if skip_tailscale {
            eprintln!("-----> Skipping tailnet connection");
        } else {
            eprintln!("-----> Connecting to tailnet");
            tailnet_hostname = tailscale::join_in_container(app)?;
        }

        let port = allocate_port(app);
        eprintln!("-----> Setting up port forwarding on :{port}");
        container::add_proxy(app, port, port)?;
    }

    let (storage_pool, storage_volume) = ensure_app_storage_volume(app)?;
    container::ensure_storage_mount(app, &storage_pool, &storage_volume)?;

    eprintln!("-----> Pushing code to container");
    container::push_code(app, &code_dir.to_string_lossy())?;
    install_apt_packages(app, &config.apt_packages)?;
    persist_start_command(app, &config.start_command)?;
    persist_required_env(app, &config.required_env)?;
    run_hook(app, "preinstall", config.preinstall_command.as_deref())?;

    if let Some(command) = app_workdir_command(&config.install_command) {
        eprintln!("-----> Installing dependencies");
        run_install_command_with_logging(app, &command, "dependency install")?;
    }

    run_hook(app, "postinstall", config.postinstall_command.as_deref())?;

    eprintln!("-----> Starting app");
    launch_app_process(app, port, &config.start_command, &app_env)?;

    if !skip_tailscale {
        tailnet_hostname = tailnet_hostname.or_else(|| tailscale::dns_name_in_container(app));
    }
    if !skip_tailscale && tailnet_hostname.is_some() {
        if let Err(e) = tailscale::expose_http_in_container(app, port) {
            eprintln!("       Warning: failed to expose tailnet HTTP on :80: {e}");
        }
    }

    caddy::add(app, port)?;
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

fn app_is_running(app: &str) -> Result<bool, String> {
    let Some(active_app) = resolve_active_app_ref(app)? else {
        return Ok(false);
    };
    container::is_running(&active_app)
}

fn restart_app_process(app: &str, vars: &BTreeMap<String, String>) -> Result<(), String> {
    let active_app = resolve_existing_active_app_ref(app)?;
    let required_env = read_required_env(&active_app)?;
    ensure_required_env_present(&required_env, vars)?;
    let start = read_start_command(&active_app)?;
    let port = allocate_port(app);
    stop_app_process_on_port(&active_app, port)?;
    launch_app_process(&active_app, port, &start, vars)?;
    if tailscale::dns_name_in_container(&active_app).is_some()
        && let Err(e) = tailscale::expose_http_in_container(&active_app, port)
    {
        eprintln!("       Warning: failed to expose tailnet HTTP on :80: {e}");
    }
    Ok(())
}

pub fn env_command(app: &str, assignments: &[String]) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let mut vars = read_env_vars(app)?;

    if assignments.is_empty() {
        if vars.is_empty() {
            println!("No environment variables configured for {app}.");
            return Ok(());
        }
        for (name, value) in &vars {
            println!("{name}={value}");
        }
        return Ok(());
    }

    for assignment in assignments {
        let (name, value) = parse_env_assignment(assignment)?;
        vars.insert(name, value);
    }

    let running = app_is_running(app)?;
    if running {
        let active_app = resolve_existing_active_app_ref(app)?;
        let required_env = read_required_env(&active_app)?;
        ensure_required_env_present(&required_env, &vars)?;
    }

    write_env_vars(app, &vars)?;
    eprintln!("-----> Saved {} env var(s) for {app}", assignments.len());

    if running {
        eprintln!("-----> Restarting {app} to apply environment changes");
        restart_app_process(app, &vars)?;
        eprintln!("=====> {app} restarted");
    } else {
        eprintln!("       {app} is not running; changes will apply on next start/deploy");
    }

    Ok(())
}

pub fn env_unset(app: &str, names: &[String]) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    if names.is_empty() {
        return Err("env-unset requires at least one NAME".to_string());
    }

    let mut vars = read_env_vars(app)?;
    let mut parsed_names = Vec::new();
    for name in names {
        let parsed = parse_env_name(name)?;
        if parsed_names.iter().any(|v| v == &parsed) {
            continue;
        }
        parsed_names.push(parsed);
    }

    for name in &parsed_names {
        vars.remove(name);
    }

    let running = app_is_running(app)?;
    if running {
        let active_app = resolve_existing_active_app_ref(app)?;
        let required_env = read_required_env(&active_app)?;
        ensure_required_env_present(&required_env, &vars)?;
    }

    if vars.is_empty() {
        remove_env_vars(app)?;
    } else {
        write_env_vars(app, &vars)?;
    }
    eprintln!("-----> Unset {} env var(s) for {app}", parsed_names.len());

    if running {
        eprintln!("-----> Restarting {app} to apply environment changes");
        restart_app_process(app, &vars)?;
        eprintln!("=====> {app} restarted");
    } else {
        eprintln!("       {app} is not running; changes will apply on next start/deploy");
    }

    Ok(())
}

pub fn ps() -> Result<(), String> {
    let containers = container::list()?;
    let apps = app_targets_from_runtime_state(&containers)?;
    if apps.is_empty() {
        println!("No apps running.");
        return Ok(());
    }
    println!("{:<20} {:<10}", "APP", "STATUS");
    for (app, _active_app, status) in apps {
        println!("{:<20} {:<10}", app, status);
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct AppHealthReport {
    app: String,
    healthy: bool,
    details: Vec<String>,
}

fn has_deploy_suffix(app: &str, marker: &str) -> bool {
    let Some((base, suffix)) = app.rsplit_once(marker) else {
        return false;
    };
    !base.is_empty() && !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit())
}

fn is_transient_deploy_app_name(app: &str) -> bool {
    has_deploy_suffix(app, "-build-")
        || has_deploy_suffix(app, "-prev-")
        || has_deploy_suffix(app, "-failed-")
}

fn canonical_app_name_from_container(container_name: &str) -> Option<String> {
    let app = container_name.strip_prefix("psht-")?;
    if app.is_empty() || is_transient_deploy_app_name(app) {
        return None;
    }
    Some(app.to_string())
}

fn app_targets_from_runtime_state(
    containers: &[container::ContainerInfo],
) -> Result<Vec<(String, Option<String>, String)>, String> {
    let mut status_by_name = BTreeMap::new();
    for container in containers {
        status_by_name.insert(container.name.clone(), container.status.clone());
    }

    let mut targets: BTreeMap<String, (Option<String>, String)> = BTreeMap::new();
    for (app, state) in read_all_app_runtime_states()? {
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

fn app_port_listening(app: &str, port: u16) -> Result<bool, String> {
    let output = container::exec_output(
        app,
        &format!(
            "if ss -ltn \"sport = :{port}\" 2>/dev/null | grep -q LISTEN; then echo ready; fi; true"
        ),
    )?;
    Ok(output.trim() == "ready")
}

fn check_app_health(app: &str, active_app: &str, container_status: &str) -> AppHealthReport {
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

    let app_running = match app_process_is_running(active_app) {
        Ok(true) => {
            details.push("app process running".to_string());
            true
        }
        Ok(false) => {
            healthy = false;
            details.push("app process not running".to_string());
            false
        }
        Err(err) => {
            healthy = false;
            details.push(format!("failed to check app process: {err}"));
            false
        }
    };

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

    if app_running {
        let port = allocate_port(app);
        match app_port_listening(active_app, port) {
            Ok(true) => details.push(format!("tcp :{port} listening")),
            Ok(false) => {
                healthy = false;
                details.push(format!("tcp :{port} is not listening"));
            }
            Err(err) => {
                healthy = false;
                details.push(format!("failed to check tcp :{port}: {err}"));
            }
        }
    }

    AppHealthReport {
        app: app.to_string(),
        healthy,
        details,
    }
}

pub fn health() -> Result<(), String> {
    eprintln!("-----> Checking app health");
    let containers = container::list()?;
    let apps = app_targets_from_runtime_state(&containers)?;
    if apps.is_empty() {
        let uid = run_cmd_capture("id", &["-u"]).unwrap_or_else(|_| "?".to_string());
        println!(
            "No deployed apps found for uid {} (HOME={}).",
            uid.trim(),
            home_dir().display()
        );
        if uid.trim() == "0" {
            println!("Hint: deploy state is user-scoped. Try: sudo -u psht psht-server health");
        }
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

    if unhealthy.is_empty() {
        eprintln!("=====> All app containers are healthy");
        return Ok(());
    }

    Err(format!(
        "{} app(s) unhealthy: {}",
        unhealthy.len(),
        unhealthy.join(", ")
    ))
}

pub fn logs(app: &str, follow: bool) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let active_app = resolve_existing_active_app_ref(app)?;
    container::logs(&active_app, follow)
}

fn setup_script(hostname: &str) -> String {
    let version = env!("CARGO_PKG_VERSION");
    let forge_url = configured_forge_url();
    format!(
        r#"#!/bin/sh
set -e

VERSION="{version}"
FORGE_URL="${{PSHT_FORGE_URL:-{forge_url}}}"
FORGE_URL="${{FORGE_URL%/}}"
SOURCE_URL="${{PSHT_SOURCE_URL:-$FORGE_URL}}"
SOURCE_URL="${{SOURCE_URL%/}}"

detect_target() {{
  os=$(uname -s)
  arch=$(uname -m)
  case "$os/$arch" in
    Linux/x86_64|Linux/amd64) echo "x86_64-unknown-linux-gnu" ;;
    Linux/aarch64|Linux/arm64) echo "aarch64-unknown-linux-gnu" ;;
    Darwin/x86_64|Darwin/amd64) echo "x86_64-apple-darwin" ;;
    Darwin/aarch64|Darwin/arm64) echo "aarch64-apple-darwin" ;;
    *) echo "unsupported platform: $os/$arch" >&2; exit 1 ;;
  esac
}}

install_cli() {{
  install_dir="$1"
  target=$(detect_target)
  asset_url="$FORGE_URL/releases/download/v$VERSION/psht-$VERSION-$target.tar.gz"
  tmpdir=$(mktemp -d)
  if curl -fsSL "$asset_url" -o "$tmpdir/psht.tar.gz" 2>/dev/null; then
    tar xzf "$tmpdir/psht.tar.gz" -C "$tmpdir"
    install -m 755 "$tmpdir/psht" "$install_dir/psht"
    rm -rf "$tmpdir"
    return 0
  fi

  echo "warning: no prebuilt psht release for $target at $asset_url" >&2
  if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found; install Rust toolchain or use a forge with prebuilt assets for $target" >&2
    rm -rf "$tmpdir"
    exit 1
  fi

  source_root="$tmpdir/source-root"
  echo "-----> building psht from source (this can take a few minutes)" >&2
  cargo install --git "$SOURCE_URL" --tag "v$VERSION" --root "$source_root" --bin psht
  install -m 755 "$source_root/bin/psht" "$install_dir/psht"
  rm -rf "$tmpdir"
}}

# Find or install psht CLI
# The server binary also has a `setup` command that prints this script.
# Reusing it here would recurse and only print the script again.
if command -v psht >/dev/null 2>&1 && psht __is-cli >/dev/null 2>&1; then
  PSHT_BIN=$(command -v psht)
else
  printf "Install psht CLI to (default: ~/.local/bin): " >&2
  read -r install_dir < /dev/tty
  install_dir="${{install_dir:-$HOME/.local/bin}}"
  mkdir -p "$install_dir"
  install_cli "$install_dir"
  PSHT_BIN="$install_dir/psht"
  case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) echo "NOTE: Add $install_dir to your PATH: export PATH=\"$install_dir:\$PATH\"" >&2 ;;
  esac
  echo "Installed psht CLI to $PSHT_BIN" >&2
fi

# Write default host
mkdir -p "$HOME/.psht"
config="$HOME/.psht/config.toml"
if [ ! -f "$config" ]; then
  echo 'host = "{hostname}"' > "$config"
fi

# Set up project
"$PSHT_BIN" setup"#
    )
}

pub fn setup() -> Result<(), String> {
    println!("{}", setup_script(&hostname()));
    Ok(())
}

fn update_script(hostname: &str) -> String {
    let version = env!("CARGO_PKG_VERSION");
    let forge_url = configured_forge_url();
    format!(
        r#"#!/bin/sh
set -e
PSHT_BIN=$(command -v psht) || {{ echo "psht not found. Run: ssh psht@{hostname} setup | sh" >&2; exit 1; }}
FORGE_URL="${{PSHT_FORGE_URL:-{forge_url}}}"
FORGE_URL="${{FORGE_URL%/}}"
SOURCE_URL="${{PSHT_SOURCE_URL:-$FORGE_URL}}"
SOURCE_URL="${{SOURCE_URL%/}}"

detect_target() {{
  os=$(uname -s)
  arch=$(uname -m)
  case "$os/$arch" in
    Linux/x86_64|Linux/amd64) echo "x86_64-unknown-linux-gnu" ;;
    Linux/aarch64|Linux/arm64) echo "aarch64-unknown-linux-gnu" ;;
    Darwin/x86_64|Darwin/amd64) echo "x86_64-apple-darwin" ;;
    Darwin/aarch64|Darwin/arm64) echo "aarch64-apple-darwin" ;;
    *) echo "unsupported platform: $os/$arch" >&2; exit 1 ;;
  esac
}}

current=$("$PSHT_BIN" --version 2>/dev/null | awk '{{print $2}}') || current=""
if [ "$current" = "{version}" ]; then
  echo "psht {version} (up to date)" >&2
  exit 0
fi

target=$(detect_target)
asset_url="$FORGE_URL/releases/download/v{version}/psht-{version}-$target.tar.gz"
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT
candidate="$tmpdir/psht"
if curl -fsSL "$asset_url" -o "$tmpdir/psht.tar.gz" 2>/dev/null; then
  tar xzf "$tmpdir/psht.tar.gz" -C "$tmpdir"
else
  echo "warning: no prebuilt psht release for $target at $asset_url" >&2
  if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found; install Rust toolchain or use a forge with prebuilt assets for $target" >&2
    exit 1
  fi
  source_root="$tmpdir/source-root"
  echo "-----> building psht from source (this can take a few minutes)" >&2
  cargo install --git "$SOURCE_URL" --tag "v{version}" --root "$source_root" --bin psht
  candidate="$source_root/bin/psht"
fi
if [ ! -x "$candidate" ]; then
  echo "error: downloaded archive missing executable psht binary" >&2
  exit 1
fi
candidate_version=$("$candidate" --version 2>/dev/null | awk '{{print $2}}') || candidate_version=""
if [ "$candidate_version" != "{version}" ]; then
  echo "error: downloaded psht ${{candidate_version:-unknown}}, expected {version}" >&2
  exit 1
fi
staged="$tmpdir/psht.new"
install -m 755 "$candidate" "$staged"
mv "$staged" "$PSHT_BIN"
installed=$("$PSHT_BIN" --version 2>/dev/null | awk '{{print $2}}') || installed=""
if [ "$installed" != "{version}" ]; then
  echo "error: installed psht ${{installed:-unknown}}, expected {version}" >&2
  exit 1
fi
echo "psht $installed (updated)" >&2"#
    )
}

pub fn update() -> Result<(), String> {
    println!("{}", update_script(&hostname()));
    Ok(())
}

pub fn print_cli() -> Result<(), String> {
    let cli = ensure_cli_binary()?;
    let mut file =
        fs::File::open(&cli).map_err(|e| format!("failed to open {}: {e}", cli.display()))?;
    let mut stdout = std::io::stdout().lock();
    std::io::copy(&mut file, &mut stdout)
        .map_err(|e| format!("failed to stream {}: {e}", cli.display()))?;
    stdout
        .flush()
        .map_err(|e| format!("failed to flush stdout: {e}"))?;
    Ok(())
}

fn init_stacks_in(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("failed to create stacks dir: {e}"))?;
    for (name, content) in STACKS {
        fs::write(dir.join(format!("{name}.sh")), content)
            .map_err(|e| format!("failed to write {name}.sh: {e}"))?;
    }
    Ok(())
}

pub fn init_stacks() -> Result<(), String> {
    init_stacks_in(&stacks_dir())
}

fn write_oauth_config(path: &Path, client_id: &str, client_secret: &str) -> Result<(), String> {
    if client_id.is_empty() || client_secret.is_empty() {
        return Err("OAuth client ID and secret are required".to_string());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let content =
        format!("TS_OAUTH_CLIENT_ID={client_id}\nTS_OAUTH_CLIENT_SECRET={client_secret}\n");
    fs::write(path, content).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(())
}

pub fn bootstrap() -> Result<(), String> {
    if run_cmd_capture("id", &["-u"])? != "0" {
        return Err("Run this command as root: sudo psht-server bootstrap".to_string());
    }

    let psht_user = "psht";
    let psht_home = PathBuf::from(format!("/home/{psht_user}"));
    let skip_tailscale = env::var_os("PSHT_SKIP_TAILSCALE").is_some();

    let current_bin = current_psht_binary()?;
    let psht_bin = prepare_server_binary(&current_bin)?;
    let psht_bin_str = psht_bin.to_string_lossy().to_string();
    let psht_dir = current_bin
        .parent()
        .ok_or_else(|| "failed to determine psht binary directory".to_string())?;

    if !command_exists("incus") {
        eprintln!("-----> Installing Incus");
        if !command_exists("curl") {
            run_cmd("apt-get", &["update"])?;
            run_cmd("apt-get", &["install", "-y", "curl"])?;
        }

        fs::create_dir_all("/etc/apt/keyrings")
            .map_err(|e| format!("failed to create /etc/apt/keyrings: {e}"))?;
        run_cmd(
            "curl",
            &[
                "-fsSL",
                "https://pkgs.zabbly.com/key.asc",
                "-o",
                "/etc/apt/keyrings/zabbly.asc",
            ],
        )?;

        let codename = os_release_codename()?;
        let arch = run_cmd_capture("dpkg", &["--print-architecture"])?;
        let source = format!(
            "Enabled: yes\nTypes: deb\nURIs: https://pkgs.zabbly.com/incus/stable\nSuites: {codename}\nComponents: main\nArchitectures: {arch}\nSigned-By: /etc/apt/keyrings/zabbly.asc\n"
        );
        fs::write(
            "/etc/apt/sources.list.d/zabbly-incus-stable.sources",
            source,
        )
        .map_err(|e| {
            format!("failed to write /etc/apt/sources.list.d/zabbly-incus-stable.sources: {e}")
        })?;

        run_cmd("apt-get", &["update"])?;
        run_cmd("apt-get", &["install", "-y", "incus"])?;
    }

    let _ = Command::new("systemctl")
        .args(["start", "incus.socket", "incus-user.socket"])
        .status();

    if !command_succeeds("incus", &["profile", "show", "default"]) {
        eprintln!("-----> Initializing Incus");
        run_cmd("incus", &["admin", "init", "--minimal"])?;
    }

    if !skip_tailscale {
        if !command_exists("tailscale") {
            return Err(
                "Tailscale is not installed. Install it first: https://tailscale.com/download/linux"
                    .to_string(),
            );
        }
        if !command_succeeds("tailscale", &["status"]) {
            return Err("Tailscale is not connected. Run: sudo tailscale up --ssh".to_string());
        }
        if !tailscale_ssh_enabled()? {
            eprintln!("-----> Enabling Tailscale SSH");
            run_cmd("tailscale", &["up", "--ssh"])?;
        }
        eprintln!("-----> Tailscale SSH is active");
    }

    let oauth_config = psht_home.join(".config/tailscale-oauth");
    if !skip_tailscale {
        if oauth_config.exists() {
            eprintln!("-----> Tailscale OAuth already configured");
        } else if let (Some(client_id), Some(client_secret)) = (
            env::var("TS_OAUTH_CLIENT_ID")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            env::var("TS_OAUTH_CLIENT_SECRET")
                .ok()
                .filter(|v| !v.trim().is_empty()),
        ) {
            eprintln!("-----> Setting up Tailscale OAuth from environment");
            write_oauth_config(&oauth_config, client_id.trim(), client_secret.trim())?;
        } else {
            println!();
            eprintln!("-----> Setting up Tailscale OAuth for container networking");
            println!();
            println!("       1. Ensure tag:psht exists in your ACL:");
            println!("          https://login.tailscale.com/admin/acls/visual/tags/add");
            println!();
            println!("       2. Create a credential at:");
            println!("          https://login.tailscale.com/admin/settings/oauth");
            println!("          Under Scopes > Keys, check Write and select tag:psht.");
            println!();

            let confirm = prompt_tty("       Have you completed the steps above? (y/n) ")?;
            if confirm != "y" && confirm != "Y" {
                return Err(
                    "Complete the steps above and re-run: sudo psht-server bootstrap".to_string(),
                );
            }

            println!();
            let client_id = prompt_tty("OAuth client ID: ")?;
            let client_secret = prompt_tty("OAuth client secret: ")?;
            write_oauth_config(&oauth_config, client_id.trim(), client_secret.trim())?;
        }
    }

    ensure_line_in_file(Path::new("/etc/shells"), &psht_bin_str)?;

    if !command_succeeds("id", &[psht_user]) {
        eprintln!("-----> Creating user {psht_user}");
        run_cmd("useradd", &["-m", "-s", &psht_bin_str, psht_user])?;
    } else {
        eprintln!("-----> User {psht_user} exists, updating shell");
        run_cmd("chsh", &["-s", &psht_bin_str, psht_user])?;
    }

    let owner = format!("{psht_user}:{psht_user}");
    // Ensure the service user can create runtime config (for example ~/.config/incus).
    fs::create_dir_all(&psht_home)
        .map_err(|e| format!("failed to create {}: {e}", psht_home.display()))?;
    let psht_config_dir = psht_home.join(".config");
    fs::create_dir_all(&psht_config_dir)
        .map_err(|e| format!("failed to create {}: {e}", psht_config_dir.display()))?;
    let psht_home_s = psht_home.to_string_lossy().to_string();
    let psht_config_dir_s = psht_config_dir.to_string_lossy().to_string();
    run_cmd("chown", &[&owner, &psht_home_s])?;
    run_cmd("chown", &["-R", &owner, &psht_config_dir_s])?;

    // Suppress MOTD/noise on SSH login for the psht service user.
    let hushlogin = psht_home.join(".hushlogin");
    if !hushlogin.exists() {
        fs::write(&hushlogin, "")
            .map_err(|e| format!("failed to write {}: {e}", hushlogin.display()))?;
    }
    let hushlogin_s = hushlogin.to_string_lossy().to_string();
    run_cmd("chown", &[&owner, &hushlogin_s])?;
    run_cmd("chmod", &["644", &hushlogin_s])?;

    if oauth_config.exists() {
        let oauth = oauth_config.to_string_lossy().to_string();
        run_cmd("chown", &[&owner, &oauth])?;
        run_cmd("chmod", &["600", &oauth])?;
    }

    let psht_cli_src = psht_dir.join("psht");
    let psht_bin_dir = psht_home.join("bin");
    fs::create_dir_all(&psht_bin_dir)
        .map_err(|e| format!("failed to create {}: {e}", psht_bin_dir.display()))?;
    if psht_cli_src.exists() {
        let psht_cli_dst = psht_bin_dir.join("psht");
        fs::copy(&psht_cli_src, &psht_cli_dst).map_err(|e| {
            format!(
                "failed to copy {} to {}: {e}",
                psht_cli_src.display(),
                psht_cli_dst.display()
            )
        })?;
        let cli_path = psht_cli_dst.to_string_lossy().to_string();
        let cli_dir = psht_bin_dir.to_string_lossy().to_string();
        run_cmd("chmod", &["755", &cli_path])?;
        run_cmd("chown", &[&owner, &cli_dir, &cli_path])?;
    }

    eprintln!("-----> Adding {psht_user} to incus group");
    run_cmd("usermod", &["-aG", "incus", psht_user])?;

    let mut incus_ready = false;
    for _ in 0..30 {
        if command_succeeds("incus", &["info"]) {
            incus_ready = true;
            break;
        }
        thread::sleep(Duration::from_secs(1));
    }
    if !incus_ready {
        return Err("incus did not become ready after 30 seconds".to_string());
    }

    let psht_uid = run_cmd_capture("id", &["-u", psht_user])?;
    let psht_project = format!("user-{}", psht_uid.trim());
    if !command_succeeds("incus", &["project", "show", &psht_project]) {
        run_cmd("incus", &["project", "create", &psht_project])?;
    }
    run_cmd(
        "incus",
        &["project", "set", &psht_project, "restricted=true"],
    )?;
    run_cmd(
        "incus",
        &[
            "project",
            "set",
            &psht_project,
            "restricted.devices.proxy=allow",
        ],
    )?;
    ensure_project_default_profile(&psht_project)?;

    eprintln!("-----> Setting up directories");
    let repos = psht_home.join("repos");
    let builds = psht_home.join("builds");
    let stacks = psht_home.join("stacks");
    fs::create_dir_all(&repos).map_err(|e| format!("failed to create {}: {e}", repos.display()))?;
    fs::create_dir_all(&builds)
        .map_err(|e| format!("failed to create {}: {e}", builds.display()))?;
    fs::create_dir_all(&stacks)
        .map_err(|e| format!("failed to create {}: {e}", stacks.display()))?;

    let repos_s = repos.to_string_lossy().to_string();
    let builds_s = builds.to_string_lossy().to_string();
    let stacks_s = stacks.to_string_lossy().to_string();
    init_stacks_in(&stacks)?;
    run_cmd("chown", &["-R", &owner, &repos_s, &builds_s, &stacks_s])?;

    let ts_hostname = if skip_tailscale {
        hostname()
    } else {
        run_cmd_capture("tailscale", &["status", "--json"])
            .ok()
            .and_then(|json| parse_tailscale_dns_name(&json))
            .unwrap_or_else(hostname)
    };

    println!();
    println!("=====> psht is ready!");
    println!("       Containers will join your tailnet as psht-<app>");
    println!();
    println!("Usage:");
    println!();
    println!("  cd your-app/");
    println!("  psht deploy");
    println!();
    println!("Commands:");
    println!("  ssh {psht_user}@{ts_hostname} ps");
    println!("  ssh {psht_user}@{ts_hostname} logs <app>");
    println!("  ssh {psht_user}@{ts_hostname} stop <app>");
    Ok(())
}

fn upgrade_script() -> String {
    let forge_url = configured_forge_url();
    let invoked_bin = env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let invoked_bin_quoted = shell_quote(&invoked_bin);
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

PSHT_USER="psht"
PSHT_HOME="/home/$PSHT_USER"
FORGE_URL="${{PSHT_FORGE_URL:-{forge_url}}}"
FORGE_URL="${{FORGE_URL%/}}"

log() {{ echo "-----> $*"; }}
err() {{ echo "ERROR: $*" >&2; exit 1; }}
detect_version() {{
    local bin="$1"
    "$bin" --version 2>/dev/null | awk '{{print $2}}' | head -n1
}}

PSHT_BIN=$(getent passwd "$PSHT_USER" | cut -d: -f7 || true)
if [[ -z "$PSHT_BIN" ]]; then
    PSHT_BIN=$(command -v psht-server) || err "psht-server not found in PATH"
fi
PSHT_BIN=$(realpath "$PSHT_BIN")
[[ -x "$PSHT_BIN" ]] || err "psht-server binary is not executable: $PSHT_BIN"

PATH_BIN=$(command -v psht-server || true)
if [[ -n "$PATH_BIN" ]]; then
    PATH_BIN=$(realpath "$PATH_BIN")
fi
INVOKED_BIN={invoked_bin_quoted}
if [[ -n "$INVOKED_BIN" ]]; then
    INVOKED_BIN=$(realpath "$INVOKED_BIN" 2>/dev/null || true)
fi

install_targets=("$PSHT_BIN")
if [[ -n "$PATH_BIN" && "$PATH_BIN" != "$PSHT_BIN" ]]; then
    install_targets+=("$PATH_BIN")
fi
if [[ -n "$INVOKED_BIN" && "$INVOKED_BIN" != "$PSHT_BIN" && "$INVOKED_BIN" != "${{PATH_BIN:-}}" ]]; then
    install_targets+=("$INVOKED_BIN")
fi

[[ $EUID -eq 0 ]] || err "Run this script as root: sudo psht-server upgrade"

CURRENT_VERSION=$(detect_version "$PSHT_BIN")
[[ -n "$CURRENT_VERSION" ]] || err "failed to detect installed psht-server version from $PSHT_BIN"

# Detect architecture
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
    aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
    *)       err "Unsupported architecture: $ARCH" ;;
esac

# Resolve latest version from forge.
log "Checking for updates"
LATEST=""
LATEST_URL=$(curl -fsSL -o /dev/null -w '%{{url_effective}}' "$FORGE_URL/releases/latest" 2>/dev/null || true)
if [[ -n "$LATEST_URL" ]]; then
    LATEST_TAG="${{LATEST_URL##*/}}"
    LATEST_TAG="${{LATEST_TAG%%\?*}}"
    if [[ -n "$LATEST_TAG" && "$LATEST_TAG" != "latest" ]]; then
        LATEST="${{LATEST_TAG#v}}"
    fi
fi

if [[ -z "$LATEST" ]]; then
    REPO_PATH=$(echo "$FORGE_URL" | sed -E 's#https?://[^/]+/##')
    if [[ -n "$REPO_PATH" && "$REPO_PATH" != "$FORGE_URL" ]]; then
        LATEST_API=$(curl -fsSL "$FORGE_URL/api/v1/repos/$REPO_PATH/releases/latest" 2>/dev/null || true)
        if [[ -n "$LATEST_API" ]]; then
            LATEST=$(echo "$LATEST_API" | grep -o '"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"' | head -n1 | cut -d'"' -f4 | sed 's/^v//')
        fi
    fi
fi

[[ -n "$LATEST" ]] || err "Failed to resolve latest release from $FORGE_URL (tried /releases/latest and /api/v1/repos/.../releases/latest)"

needs_upgrade=0
for target in "${{install_targets[@]}}"; do
    target_version=$(detect_version "$target" || true)
    if [[ -z "$target_version" || "$target_version" != "$LATEST" ]]; then
        needs_upgrade=1
        break
    fi
done
if [[ "$needs_upgrade" -eq 0 ]]; then
    echo "psht $LATEST (up to date)"
    exit 0
fi
if [[ "$CURRENT_VERSION" == "$LATEST" ]]; then
    log "Upgrading psht binaries to $LATEST"
else
    log "Upgrading psht $CURRENT_VERSION -> $LATEST"
fi

# Set up temp directory with cleanup
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

# Download both tarballs
BASE_URL="$FORGE_URL/releases/download/v$LATEST"
download_or_err() {{
    local url="$1"
    local out="$2"
    if ! curl -fsSL "$url" -o "$out"; then
        err "download failed: $url"
    fi
}}

log "Downloading psht $LATEST"
download_or_err "$BASE_URL/psht-server-${{LATEST}}-${{TARGET}}.tar.gz" "$TMPDIR/psht-server.tar.gz"
download_or_err "$BASE_URL/psht-${{LATEST}}-${{TARGET}}.tar.gz" "$TMPDIR/psht.tar.gz"

# Extract and install
tar xzf "$TMPDIR/psht-server.tar.gz" -C "$TMPDIR"
tar xzf "$TMPDIR/psht.tar.gz" -C "$TMPDIR"

candidate_version=$(detect_version "$TMPDIR/psht-server" || true)
[[ -n "$candidate_version" ]] || err "failed to detect downloaded psht-server version"
if [[ "$candidate_version" != "$LATEST" ]]; then
    err "downloaded psht-server ${{candidate_version:-unknown}}, expected $LATEST"
fi

log "Installing binaries"
for target in "${{install_targets[@]}}"; do
    install -m 755 "$TMPDIR/psht-server" "$target"
done
mkdir -p "$PSHT_HOME/bin"
install -m 755 "$TMPDIR/psht" "$PSHT_HOME/bin/psht"
chown "$PSHT_USER:$PSHT_USER" "$PSHT_HOME/bin/psht"

for target in "${{install_targets[@]}}"; do
    installed_version=$(detect_version "$target" || true)
    if [[ -z "$installed_version" || "$installed_version" != "$LATEST" ]]; then
        err "installed $target reports ${{installed_version:-unknown}}, expected $LATEST"
    fi
done

# Update incus
log "Updating incus"
apt-get update -qq && apt-get install -y -qq incus

# Refresh stacks
log "Refreshing stacks"
sudo -u "$PSHT_USER" "$PSHT_BIN" init-stacks

echo "=====> psht upgraded to $LATEST"
"#
    )
}

pub fn upgrade_server() -> Result<(), String> {
    let script = upgrade_script();
    let status = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| format!("failed to run upgrade: {e}"))?;
    if !status.success() {
        return Err("upgrade failed".to_string());
    }
    Ok(())
}

fn doctor_script() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"#!/usr/bin/env bash
set -uo pipefail

PSHT_USER="psht"
PSHT_HOME="/home/$PSHT_USER"
PSHT_USER_SHELL=$(getent passwd "$PSHT_USER" 2>/dev/null | cut -d: -f7 || true)
FAILED=0

pass() {{ echo "  [ok] $*"; }}
fail() {{ echo "  [FAIL] $*"; FAILED=1; }}

check() {{
    local desc="$1"; shift
    if "$@" &>/dev/null; then
        pass "$desc"
    else
        fail "$desc"
    fi
}}

echo "Installation:"
if [[ -n "$PSHT_USER_SHELL" ]]; then
    check "psht-server binary at $PSHT_USER_SHELL" test -x "$PSHT_USER_SHELL"
else
    fail "psht user shell path missing"
fi
check "psht CLI binary at \$PSHT_HOME/bin/psht" test -x "$PSHT_HOME/bin/psht"
if [[ -n "$PSHT_USER_SHELL" ]]; then
    INSTALLED_VERSION=$("$PSHT_USER_SHELL" --version 2>/dev/null | awk '{{print $2}}') || INSTALLED_VERSION=""
else
    INSTALLED_VERSION=""
fi
if [[ "$INSTALLED_VERSION" == "{version}" ]]; then
    pass "psht version {version}"
else
    fail "psht version: expected {version}, got ${{INSTALLED_VERSION:-unknown}}"
fi

echo ""
echo "System:"
check "psht user exists" id psht
if [[ -n "$PSHT_USER_SHELL" ]] && getent passwd psht | grep -q ":$PSHT_USER_SHELL$"; then
    pass "psht user shell is $PSHT_USER_SHELL"
else
    fail "psht user shell is not $PSHT_USER_SHELL"
fi
if [[ -n "$PSHT_USER_SHELL" ]] && grep -qx "$PSHT_USER_SHELL" /etc/shells 2>/dev/null; then
    pass "$PSHT_USER_SHELL listed in /etc/shells"
else
    fail "$PSHT_USER_SHELL not listed in /etc/shells"
fi
if id -nG psht 2>/dev/null | grep -qw incus; then
    pass "psht user in incus group"
else
    fail "psht user not in incus group"
fi

echo ""
echo "Incus:"
check "incus installed" command -v incus
check "incus responsive" incus info

if [[ -z "${{PSHT_SKIP_TAILSCALE:-}}" ]]; then
echo ""
echo "Tailscale:"
check "tailscale installed" command -v tailscale
check "tailscale connected" tailscale status
if tailscale status --json 2>/dev/null | grep -q '"SSH":true'; then
    pass "tailscale SSH enabled"
else
    fail "tailscale SSH not enabled"
fi
if [[ -f "$PSHT_HOME/.config/tailscale-oauth" ]]; then
    pass "OAuth config exists"
else
    fail "OAuth config missing at \$PSHT_HOME/.config/tailscale-oauth"
fi
fi

echo ""
echo "Directories & stacks:"
check "\$PSHT_HOME/repos exists" test -d "$PSHT_HOME/repos"
check "\$PSHT_HOME/builds exists" test -d "$PSHT_HOME/builds"
check "\$PSHT_HOME/stacks exists" test -d "$PSHT_HOME/stacks"
if ls "$PSHT_HOME/stacks"/*.sh &>/dev/null; then
    pass "stacks populated"
else
    fail "no .sh files in \$PSHT_HOME/stacks"
fi

echo ""
if [[ $FAILED -eq 0 ]]; then
    echo "All checks passed."
else
    echo "Some checks failed."
    exit 1
fi
"#
    )
}

pub fn doctor() -> Result<(), String> {
    let script = doctor_script();
    let status = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| format!("failed to run doctor: {e}"))?;
    if !status.success() {
        return Err("doctor checks failed".to_string());
    }
    Ok(())
}

fn resolve_active_app_for_tailscale(app: &str) -> Result<String, String> {
    resolve_existing_active_app_ref(app)
}

fn ensure_container_running_for_tailscale(app: &str) -> Result<String, String> {
    let active_app = resolve_active_app_for_tailscale(app)?;
    if container::is_running(&active_app)? {
        Ok(active_app)
    } else {
        Err(format!("app '{app}' is not running"))
    }
}

pub fn tailscale_status(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let active_app = ensure_container_running_for_tailscale(app)?;
    let status = container::exec_output(&active_app, "tailscale status --json")?;
    let summary = tailscale_self_status_summary_from_json(app, &status)?;
    println!("{summary}");
    Ok(())
}

pub fn tailscale_up(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let active_app = resolve_active_app_for_tailscale(app)?;
    if !container::is_running(&active_app)? {
        eprintln!("-----> Starting container");
        container::start(&active_app)?;
    }

    eprintln!("-----> Repairing tailscale in container");
    tailscale::install_in_container(&active_app)?;
    let _ = container::exec_cmd(&active_app, "tailscale down >/dev/null 2>&1 || true");
    let tailnet_hostname = tailscale::join_in_container(&active_app)?;
    let _ = container::exec_cmd(&active_app, "tailscale serve reset >/dev/null 2>&1 || true");
    let port = allocate_port(app);
    if let Err(e) = tailscale::expose_http_in_container(&active_app, port) {
        eprintln!("       Warning: failed to expose tailnet HTTP on :80: {e}");
    }
    let (_, _, health) =
        wait_for_tailscale_online(&active_app, Duration::from_secs(TAILSCALE_ONLINE_WAIT_SECS))?;
    if !health.is_empty() {
        eprintln!("       Warning: {}", health.join(" | "));
    }
    if let Some(name) = tailnet_hostname {
        eprintln!("=====> Tailscale ready: http://{name} (also http://{name}:{port})");
    } else {
        eprintln!("=====> Tailscale repaired for {app}");
    }
    Ok(())
}

pub fn tailscale_down(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let active_app = ensure_container_running_for_tailscale(app)?;
    eprintln!("-----> Bringing tailscale down in container");
    container::exec_cmd(&active_app, "tailscale down")
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".to_string())
}

pub fn stop(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let active_app = resolve_existing_active_app_ref(app)?;
    eprintln!("-----> Stopping {app}");
    container::stop(&active_app)?;
    eprintln!("=====> {app} stopped");
    Ok(())
}

pub fn start(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let active_app = resolve_existing_active_app_ref(app)?;
    eprintln!("-----> Starting {app}");
    if !container::is_running(&active_app)? {
        container::start(&active_app)?;
    }
    if app_process_is_running(&active_app)? {
        eprintln!("       {app} is already running; skipping launch");
        eprintln!("=====> {app} started");
        return Ok(());
    }
    let vars = read_env_vars(app)?;
    let required_env = read_required_env(&active_app)?;
    ensure_required_env_present(&required_env, &vars)?;
    let command = read_start_command(&active_app)?;
    let port = allocate_port(app);
    launch_app_process(&active_app, port, &command, &vars)?;
    if tailscale::dns_name_in_container(&active_app).is_some()
        && let Err(e) = tailscale::expose_http_in_container(&active_app, port)
    {
        eprintln!("       Warning: failed to expose tailnet HTTP on :80: {e}");
    }
    eprintln!("=====> {app} started");
    Ok(())
}

pub fn destroy(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let active_app = resolve_existing_active_app_ref(app)?;
    let runtime_state = read_app_runtime_state(app)?;
    eprintln!("-----> Destroying {app}");
    caddy::remove(app)?;
    if let Err(e) = container::remove_storage_mount(&active_app) {
        eprintln!("       Warning: failed to remove /storage mount before destroy: {e}");
    }
    container::stop(&active_app)?;
    container::delete(&active_app)?;

    if let Some(state) = runtime_state
        && let Some(previous_instance) = state.previous_instance
        && let Some(previous_app) = app_ref_from_instance_name(&previous_instance)
        && previous_app != active_app
        && container::exists(&previous_app)
    {
        let _ = container::stop(&previous_app);
        let _ = container::delete(&previous_app);
    }

    delete_app_storage_volume(app)?;
    if let Err(e) = remove_env_vars(app) {
        eprintln!("       Warning: failed to remove env vars: {e}");
    }
    if let Err(e) = clear_app_runtime_state(app) {
        eprintln!("       Warning: failed to clear app runtime state: {e}");
    }
    eprintln!("=====> {app} destroyed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
    fn setup_script_calls_psht_setup() {
        let script = setup_script("example.com");
        assert!(
            script.contains("\"$PSHT_BIN\" setup"),
            "script should delegate project setup to CLI via full path"
        );
    }

    #[test]
    fn setup_script_reads_from_tty() {
        let script = setup_script("example.com");
        assert!(
            script.contains("< /dev/tty"),
            "read should use /dev/tty so prompts work when piped"
        );
    }

    #[test]
    fn setup_script_tracks_binary_path() {
        let script = setup_script("example.com");
        assert!(
            script.contains("PSHT_BIN=$(command -v psht)"),
            "should capture existing binary path"
        );
        assert!(
            script.contains("psht __is-cli >/dev/null 2>&1"),
            "should verify existing binary is the local CLI"
        );
        assert!(
            script.contains("PSHT_BIN=\"$install_dir/psht\""),
            "should capture newly installed binary path"
        );
    }

    #[test]
    fn setup_script_has_no_help_text() {
        let script = setup_script("example.com");
        assert!(
            !script.contains("Commands:"),
            "script should not contain help text"
        );
    }

    #[test]
    fn setup_script_installs_cli() {
        let script = setup_script("example.com");
        let expected_forge = format!("FORGE_URL=\"${{PSHT_FORGE_URL:-{}}}\"", DEFAULT_FORGE_URL);
        assert!(
            script.contains("Install psht CLI"),
            "script should install the CLI"
        );
        assert!(
            script.contains("PSHT_FORGE_URL"),
            "script should support overriding forge URL via PSHT_FORGE_URL"
        );
        assert!(
            script.contains("PSHT_SOURCE_URL"),
            "script should support overriding source URL via PSHT_SOURCE_URL"
        );
        assert!(
            script.contains(&expected_forge),
            "script should default forge URL to the configured default"
        );
        assert!(
            script.contains(
                "asset_url=\"$FORGE_URL/releases/download/v$VERSION/psht-$VERSION-$target.tar.gz\""
            ),
            "script should download CLI tarball from forge releases"
        );
        assert!(
            script.contains("curl -fsSL \"$asset_url\" -o \"$tmpdir/psht.tar.gz\" 2>/dev/null"),
            "script should fetch CLI with curl"
        );
        assert!(
            script.contains("cargo install --git \"$SOURCE_URL\" --tag \"v$VERSION\" --root \"$source_root\" --bin psht"),
            "script should fall back to building from source when prebuilt CLI is missing"
        );
        assert!(
            script.contains("building psht from source (this can take a few minutes)"),
            "script should explain source fallback duration"
        );
        assert!(
            script.contains("tar xzf \"$tmpdir/psht.tar.gz\""),
            "script should extract downloaded CLI tarball"
        );
        assert!(
            script.contains("install -m 755 \"$tmpdir/psht\" \"$install_dir/psht\""),
            "script should install CLI binary"
        );
        assert!(
            script.contains("Darwin/aarch64|Darwin/arm64"),
            "script should support macOS arm64 target detection"
        );
        assert!(
            script.contains("Darwin/x86_64|Darwin/amd64"),
            "script should support macOS x86_64 target detection"
        );
    }

    #[test]
    fn setup_script_writes_default_host() {
        let script = setup_script("example.com");
        assert!(
            script.contains("host = \"example.com\""),
            "script should write default host to config"
        );
    }

    #[test]
    fn update_script_downloads_binary_from_forge() {
        let script = update_script("example.com");
        let expected_forge = format!("FORGE_URL=\"${{PSHT_FORGE_URL:-{}}}\"", DEFAULT_FORGE_URL);
        assert!(
            script.contains("PSHT_FORGE_URL"),
            "should support overriding forge URL via PSHT_FORGE_URL"
        );
        assert!(
            script.contains("PSHT_SOURCE_URL"),
            "should support overriding source URL via PSHT_SOURCE_URL"
        );
        assert!(
            script.contains(&expected_forge),
            "should default forge URL to the configured default"
        );
        assert!(
            script.contains("asset_url=\"$FORGE_URL/releases/download/v"),
            "should build release asset URL from forge"
        );
        assert!(
            script.contains("curl -fsSL \"$asset_url\" -o \"$tmpdir/psht.tar.gz\" 2>/dev/null"),
            "should download CLI tarball from forge"
        );
        assert!(
            script.contains("cargo install --git \"$SOURCE_URL\" --tag \"v"),
            "should fall back to source build when prebuilt CLI asset is missing"
        );
        assert!(
            script.contains("building psht from source (this can take a few minutes)"),
            "should explain source fallback duration"
        );
        assert!(
            script.contains("Darwin/aarch64|Darwin/arm64"),
            "update script should support macOS arm64 target detection"
        );
        assert!(
            script.contains("Darwin/x86_64|Darwin/amd64"),
            "update script should support macOS x86_64 target detection"
        );
    }

    #[test]
    fn default_forge_url_points_to_github() {
        assert_eq!(DEFAULT_FORGE_URL, "https://github.com/nakajima/psht");
    }

    #[test]
    fn update_script_replaces_atomically() {
        let script = update_script("example.com");
        assert!(
            !script.contains("rm -f \"$PSHT_BIN\""),
            "should not remove current binary before replacement is staged"
        );
        assert!(
            script.contains("install -m 755 \"$candidate\" \"$staged\""),
            "should install candidate to staged path first"
        );
        assert!(
            script.contains("mv \"$staged\" \"$PSHT_BIN\""),
            "should atomically swap staged binary into place"
        );
    }

    #[test]
    fn update_script_errors_if_not_installed() {
        let script = update_script("example.com");
        assert!(
            script.contains("psht not found"),
            "should error if psht is not installed"
        );
    }

    #[test]
    fn update_script_skips_if_up_to_date() {
        let script = update_script("example.com");
        assert!(
            script.contains("up to date"),
            "should say up to date when versions match"
        );
        assert!(
            script.contains(env!("CARGO_PKG_VERSION")),
            "should embed the current version"
        );
    }

    #[test]
    fn update_script_verifies_installed_version() {
        let script = update_script("example.com");
        assert!(
            script.contains("candidate_version=$(\"$candidate\" --version"),
            "should verify downloaded candidate version before replacement"
        );
        assert!(
            script.contains("downloaded psht ${candidate_version:-unknown}, expected"),
            "should fail when downloaded candidate version mismatches"
        );
        assert!(
            script.contains("installed=$(\"$PSHT_BIN\" --version"),
            "should read installed version after downloading"
        );
        assert!(
            script.contains("installed psht ${installed:-unknown}, expected"),
            "should fail when installed version does not match server version"
        );
        assert!(
            script.contains("psht $installed (updated)"),
            "should report the installed version on success"
        );
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

    #[test]
    fn git_deploy_state_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = git_deploy_state_path_in(tmp.path(), "myapp");
        let state = GitDeployState {
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
            status: GitDeployStatus::Success,
        };
        write_git_deploy_state_to(&path, &state).unwrap();
        let loaded = read_git_deploy_state_from(&path).unwrap().unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn clear_git_deploy_state_removes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = git_deploy_state_path_in(tmp.path(), "myapp");
        let state = GitDeployState {
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
            status: GitDeployStatus::Failed,
        };
        write_git_deploy_state_to(&path, &state).unwrap();
        assert!(path.exists());

        clear_git_deploy_state_at(&path).unwrap();
        assert!(!path.exists());
        assert!(read_git_deploy_state_from(&path).unwrap().is_none());
    }

    #[test]
    fn pending_git_target_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = pending_git_target_path_in(tmp.path(), "myapp");
        let target = GitCheckoutTarget {
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
        };
        write_pending_git_target_to(&path, &target).unwrap();
        let loaded = read_pending_git_target_from(&path).unwrap().unwrap();
        assert_eq!(loaded, target);
    }

    #[test]
    fn pending_git_target_overwrite_keeps_latest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = pending_git_target_path_in(tmp.path(), "myapp");
        let first = GitCheckoutTarget {
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
        };
        let second = GitCheckoutTarget {
            ref_name: "refs/heads/main".to_string(),
            sha: "cafebabe".to_string(),
        };
        write_pending_git_target_to(&path, &first).unwrap();
        write_pending_git_target_to(&path, &second).unwrap();
        let loaded = read_pending_git_target_from(&path).unwrap().unwrap();
        assert_eq!(loaded, second);
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
    fn take_pending_git_target_removes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = pending_git_target_path_in(tmp.path(), "myapp");
        let target = GitCheckoutTarget {
            ref_name: "refs/heads/main".to_string(),
            sha: "deadbeef".to_string(),
        };
        write_pending_git_target_to(&path, &target).unwrap();
        let taken = take_pending_git_target_from(&path).unwrap().unwrap();
        assert_eq!(taken, target);
        assert!(!path.exists());
    }

    #[test]
    fn take_pending_git_target_missing_file_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = pending_git_target_path_in(tmp.path(), "myapp");
        assert!(take_pending_git_target_from(&path).unwrap().is_none());
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
    fn upgrade_script_checks_root() {
        let script = upgrade_script();
        assert!(script.contains("EUID -eq 0"), "should check for root");
    }

    #[test]
    fn upgrade_script_detects_current_version_from_binary() {
        let script = upgrade_script();
        assert!(
            script.contains("detect_version \"$PSHT_BIN\""),
            "should detect current version from installed psht-server binary"
        );
    }

    #[test]
    fn upgrade_script_detects_architecture() {
        let script = upgrade_script();
        assert!(script.contains("uname -m"), "should detect architecture");
        assert!(
            script.contains("x86_64-unknown-linux-gnu"),
            "should map x86_64"
        );
        assert!(
            script.contains("aarch64-unknown-linux-gnu"),
            "should map aarch64"
        );
    }

    #[test]
    fn upgrade_script_fetches_latest_version() {
        let script = upgrade_script();
        assert!(
            script.contains("LATEST_URL=$(curl -fsSL -o /dev/null -w '%{url_effective}' \"$FORGE_URL/releases/latest\" 2>/dev/null || true)"),
            "should try latest release redirect first"
        );
        assert!(
            script.contains("$FORGE_URL/api/v1/repos/$REPO_PATH/releases/latest"),
            "should fallback to forge API latest endpoint"
        );
    }

    #[test]
    fn upgrade_script_skips_if_up_to_date() {
        let script = upgrade_script();
        assert!(
            script.contains("up to date"),
            "should skip if already on latest version"
        );
    }

    #[test]
    fn upgrade_script_downloads_both_binaries() {
        let script = upgrade_script();
        assert!(
            script.contains("psht-server-${"),
            "should download psht-server tarball"
        );
        assert!(
            script.contains("psht-${"),
            "should download psht CLI tarball"
        );
    }

    #[test]
    fn upgrade_script_reports_failed_download_url() {
        let script = upgrade_script();
        assert!(
            script.contains("download failed: $url"),
            "should include the failed download URL in errors"
        );
    }

    #[test]
    fn upgrade_script_installs_to_correct_paths() {
        let script = upgrade_script();
        assert!(
            script.contains("PSHT_BIN=$(getent passwd \"$PSHT_USER\""),
            "should resolve psht binary path from user shell"
        );
        assert!(
            script.contains("PSHT_BIN=$(command -v psht-server)"),
            "should fall back to command -v psht-server"
        );
        assert!(
            script.contains("install_targets=(\"$PSHT_BIN\")"),
            "should include psht shell binary in install targets"
        );
        assert!(
            script.contains("install -m 755 \"$TMPDIR/psht-server\" \"$target\""),
            "should install psht-server to all target binary paths"
        );
        assert!(
            script.contains("INVOKED_BIN="),
            "should include the invoked binary path as an install target"
        );
        assert!(
            script.contains("$PSHT_HOME/bin/psht"),
            "should install psht CLI to $PSHT_HOME/bin/psht"
        );
    }

    #[test]
    fn upgrade_script_updates_incus() {
        let script = upgrade_script();
        assert!(
            script.contains("apt-get install") && script.contains("incus"),
            "should update incus via apt"
        );
    }

    #[test]
    fn upgrade_script_refreshes_stacks() {
        let script = upgrade_script();
        assert!(
            script.contains("\"$PSHT_BIN\" init-stacks"),
            "should refresh stacks with the active psht binary"
        );
    }

    #[test]
    fn upgrade_script_cleans_up_tempdir() {
        let script = upgrade_script();
        assert!(
            script.contains("mktemp -d"),
            "should create a temp directory"
        );
        assert!(
            script.contains("trap") && script.contains("rm -rf"),
            "should clean up temp directory on exit"
        );
    }

    #[test]
    fn upgrade_script_upgrades_when_path_binary_is_stale() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        fn write_exec(path: &Path, contents: &str) {
            fs::write(path, contents).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn build_tarball(dir: &Path, tar_path: &Path, file_name: &str, contents: &str) {
            let src = dir.join(file_name);
            write_exec(&src, contents);
            let status = Command::new("tar")
                .args(["-czf"])
                .arg(tar_path)
                .args(["-C"])
                .arg(dir)
                .arg(file_name)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "failed to build tarball {}",
                tar_path.display()
            );
        }

        let tmp = tempfile::tempdir().unwrap();
        let fake_bin = tmp.path().join("fake-bin");
        let fake_home = tmp.path().join("fake-home");
        let assets = tmp.path().join("assets");
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(&fake_home).unwrap();
        fs::create_dir_all(&assets).unwrap();

        let path_bin = fake_bin.join("psht-server");
        let shell_bin = tmp.path().join("psht-shell-server");
        write_exec(
            &path_bin,
            "#!/usr/bin/env bash\nif [[ \"${1:-}\" == \"--version\" || \"${1:-}\" == \"-V\" ]]; then echo \"psht-server 0.2.7 (server)\"; exit 0; fi\nif [[ \"${1:-}\" == \"init-stacks\" ]]; then exit 0; fi\nexit 0\n",
        );
        write_exec(
            &shell_bin,
            "#!/usr/bin/env bash\nif [[ \"${1:-}\" == \"--version\" || \"${1:-}\" == \"-V\" ]]; then echo \"psht-server 0.2.8 (server)\"; exit 0; fi\nif [[ \"${1:-}\" == \"init-stacks\" ]]; then exit 0; fi\nexit 0\n",
        );

        let server_tar = assets.join("psht-server.tar.gz");
        let cli_tar = assets.join("psht-cli.tar.gz");
        build_tarball(
            &assets,
            &server_tar,
            "psht-server",
            "#!/usr/bin/env bash\nif [[ \"${1:-}\" == \"--version\" || \"${1:-}\" == \"-V\" ]]; then echo \"psht-server 0.2.8 (server)\"; exit 0; fi\nif [[ \"${1:-}\" == \"init-stacks\" ]]; then exit 0; fi\nexit 0\n",
        );
        build_tarball(
            &assets,
            &cli_tar,
            "psht",
            "#!/usr/bin/env bash\necho \"psht 0.2.8 (cli)\"\n",
        );

        write_exec(
            &fake_bin.join("getent"),
            "#!/usr/bin/env bash\nif [[ \"${1:-}\" == \"passwd\" && \"${2:-}\" == \"psht\" ]]; then\n  echo \"psht:x:1000:1000::/home/psht:$PSHT_TEST_SHELL_BIN\"\n  exit 0\nfi\nexit 2\n",
        );
        write_exec(
            &fake_bin.join("curl"),
            "#!/usr/bin/env bash\nset -euo pipefail\nout=\"\"\nwrite_fmt=\"\"\nurl=\"\"\nwhile [[ $# -gt 0 ]]; do\n  case \"$1\" in\n    -o) out=\"$2\"; shift 2 ;;\n    -w) write_fmt=\"$2\"; shift 2 ;;\n    -fsSL|-f|-s|-S|-L) shift ;;\n    *) url=\"$1\"; shift ;;\n  esac\ndone\nif [[ \"$url\" == *\"/releases/latest\" && \"$out\" == \"/dev/null\" ]]; then\n  printf \"https://example.com/org/repo/releases/tag/v0.2.8\"\n  exit 0\nfi\nif [[ \"$url\" == *\"psht-server-0.2.8-x86_64-unknown-linux-gnu.tar.gz\" ]]; then\n  cp \"$PSHT_TEST_SERVER_TARBALL\" \"$out\"\n  exit 0\nfi\nif [[ \"$url\" == *\"psht-0.2.8-x86_64-unknown-linux-gnu.tar.gz\" ]]; then\n  cp \"$PSHT_TEST_CLI_TARBALL\" \"$out\"\n  exit 0\nfi\necho \"unexpected curl URL: $url\" >&2\nexit 22\n",
        );
        write_exec(&fake_bin.join("apt-get"), "#!/usr/bin/env bash\nexit 0\n");
        write_exec(
            &fake_bin.join("sudo"),
            "#!/usr/bin/env bash\nset -euo pipefail\nif [[ \"${1:-}\" == \"-u\" ]]; then shift 2; fi\nexec \"$@\"\n",
        );
        write_exec(&fake_bin.join("chown"), "#!/usr/bin/env bash\nexit 0\n");

        let original_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{}", fake_bin.display(), original_path);

        let mut script = upgrade_script();
        script = script.replace(
            "PSHT_HOME=\"/home/$PSHT_USER\"",
            &format!("PSHT_HOME=\"{}\"", fake_home.display()),
        );
        script = script.replace(
            "[[ $EUID -eq 0 ]] || err \"Run this script as root: sudo psht-server upgrade\"",
            "true",
        );
        script = script
            .lines()
            .map(|line| {
                if line.starts_with("INVOKED_BIN=") {
                    "INVOKED_BIN=\"\"".to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        let output1 = Command::new("bash")
            .arg("-c")
            .arg(&script)
            .env("PATH", &test_path)
            .env("PSHT_FORGE_URL", "https://example.com/org/repo")
            .env(
                "PSHT_TEST_SHELL_BIN",
                shell_bin.to_string_lossy().to_string(),
            )
            .env(
                "PSHT_TEST_SERVER_TARBALL",
                server_tar.to_string_lossy().to_string(),
            )
            .env(
                "PSHT_TEST_CLI_TARBALL",
                cli_tar.to_string_lossy().to_string(),
            )
            .output()
            .unwrap();
        assert!(
            output1.status.success(),
            "first upgrade failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output1.stdout),
            String::from_utf8_lossy(&output1.stderr)
        );

        let path_version = Command::new(&path_bin).arg("--version").output().unwrap();
        assert!(
            String::from_utf8_lossy(&path_version.stdout).contains("0.2.8"),
            "PATH binary should be upgraded to 0.2.8"
        );

        let output2 = Command::new("bash")
            .arg("-c")
            .arg(&script)
            .env("PATH", &test_path)
            .env("PSHT_FORGE_URL", "https://example.com/org/repo")
            .env(
                "PSHT_TEST_SHELL_BIN",
                shell_bin.to_string_lossy().to_string(),
            )
            .env(
                "PSHT_TEST_SERVER_TARBALL",
                server_tar.to_string_lossy().to_string(),
            )
            .env(
                "PSHT_TEST_CLI_TARBALL",
                cli_tar.to_string_lossy().to_string(),
            )
            .output()
            .unwrap();
        assert!(
            output2.status.success(),
            "second upgrade failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output2.stdout),
            String::from_utf8_lossy(&output2.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output2.stdout).contains("up to date"),
            "second run should report up to date"
        );
    }

    #[test]
    fn upgrade_script_errors_when_downloaded_server_version_mismatches_latest() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        fn write_exec(path: &Path, contents: &str) {
            fs::write(path, contents).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn build_tarball(dir: &Path, tar_path: &Path, file_name: &str, contents: &str) {
            let src = dir.join(file_name);
            write_exec(&src, contents);
            let status = Command::new("tar")
                .args(["-czf"])
                .arg(tar_path)
                .args(["-C"])
                .arg(dir)
                .arg(file_name)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "failed to build tarball {}",
                tar_path.display()
            );
        }

        let tmp = tempfile::tempdir().unwrap();
        let fake_bin = tmp.path().join("fake-bin");
        let fake_home = tmp.path().join("fake-home");
        let assets = tmp.path().join("assets");
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(&fake_home).unwrap();
        fs::create_dir_all(&assets).unwrap();

        let path_bin = fake_bin.join("psht-server");
        let shell_bin = tmp.path().join("psht-shell-server");
        write_exec(
            &path_bin,
            "#!/usr/bin/env bash\nif [[ \"${1:-}\" == \"--version\" || \"${1:-}\" == \"-V\" ]]; then echo \"psht-server 0.2.7 (server)\"; exit 0; fi\nif [[ \"${1:-}\" == \"init-stacks\" ]]; then exit 0; fi\nexit 0\n",
        );
        write_exec(
            &shell_bin,
            "#!/usr/bin/env bash\nif [[ \"${1:-}\" == \"--version\" || \"${1:-}\" == \"-V\" ]]; then echo \"psht-server 0.2.7 (server)\"; exit 0; fi\nif [[ \"${1:-}\" == \"init-stacks\" ]]; then exit 0; fi\nexit 0\n",
        );

        let server_tar = assets.join("psht-server.tar.gz");
        let cli_tar = assets.join("psht-cli.tar.gz");
        build_tarball(
            &assets,
            &server_tar,
            "psht-server",
            "#!/usr/bin/env bash\nif [[ \"${1:-}\" == \"--version\" || \"${1:-}\" == \"-V\" ]]; then echo \"psht-server 0.2.7 (server)\"; exit 0; fi\nif [[ \"${1:-}\" == \"init-stacks\" ]]; then exit 0; fi\nexit 0\n",
        );
        build_tarball(
            &assets,
            &cli_tar,
            "psht",
            "#!/usr/bin/env bash\necho \"psht 0.2.8 (cli)\"\n",
        );

        write_exec(
            &fake_bin.join("getent"),
            "#!/usr/bin/env bash\nif [[ \"${1:-}\" == \"passwd\" && \"${2:-}\" == \"psht\" ]]; then\n  echo \"psht:x:1000:1000::/home/psht:$PSHT_TEST_SHELL_BIN\"\n  exit 0\nfi\nexit 2\n",
        );
        write_exec(
            &fake_bin.join("curl"),
            "#!/usr/bin/env bash\nset -euo pipefail\nout=\"\"\nurl=\"\"\nwhile [[ $# -gt 0 ]]; do\n  case \"$1\" in\n    -o) out=\"$2\"; shift 2 ;;\n    -w) shift 2 ;;\n    -fsSL|-f|-s|-S|-L) shift ;;\n    *) url=\"$1\"; shift ;;\n  esac\ndone\nif [[ \"$url\" == *\"/releases/latest\" && \"$out\" == \"/dev/null\" ]]; then\n  printf \"https://example.com/org/repo/releases/tag/v0.2.8\"\n  exit 0\nfi\nif [[ \"$url\" == *\"psht-server-0.2.8-x86_64-unknown-linux-gnu.tar.gz\" ]]; then\n  cp \"$PSHT_TEST_SERVER_TARBALL\" \"$out\"\n  exit 0\nfi\nif [[ \"$url\" == *\"psht-0.2.8-x86_64-unknown-linux-gnu.tar.gz\" ]]; then\n  cp \"$PSHT_TEST_CLI_TARBALL\" \"$out\"\n  exit 0\nfi\nexit 22\n",
        );
        write_exec(&fake_bin.join("apt-get"), "#!/usr/bin/env bash\nexit 0\n");
        write_exec(
            &fake_bin.join("sudo"),
            "#!/usr/bin/env bash\nset -euo pipefail\nif [[ \"${1:-}\" == \"-u\" ]]; then shift 2; fi\nexec \"$@\"\n",
        );
        write_exec(&fake_bin.join("chown"), "#!/usr/bin/env bash\nexit 0\n");

        let original_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{}", fake_bin.display(), original_path);

        let mut script = upgrade_script();
        script = script.replace(
            "PSHT_HOME=\"/home/$PSHT_USER\"",
            &format!("PSHT_HOME=\"{}\"", fake_home.display()),
        );
        script = script.replace(
            "[[ $EUID -eq 0 ]] || err \"Run this script as root: sudo psht-server upgrade\"",
            "true",
        );
        script = script
            .lines()
            .map(|line| {
                if line.starts_with("INVOKED_BIN=") {
                    "INVOKED_BIN=\"\"".to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        let output = Command::new("bash")
            .arg("-c")
            .arg(&script)
            .env("PATH", &test_path)
            .env("PSHT_FORGE_URL", "https://example.com/org/repo")
            .env(
                "PSHT_TEST_SHELL_BIN",
                shell_bin.to_string_lossy().to_string(),
            )
            .env(
                "PSHT_TEST_SERVER_TARBALL",
                server_tar.to_string_lossy().to_string(),
            )
            .env(
                "PSHT_TEST_CLI_TARBALL",
                cli_tar.to_string_lossy().to_string(),
            )
            .output()
            .unwrap();

        assert!(
            !output.status.success(),
            "upgrade should fail when downloaded server version mismatches latest"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("downloaded psht-server 0.2.7, expected 0.2.8"),
            "unexpected stderr:\n{stderr}"
        );
    }

    #[test]
    fn doctor_script_does_not_require_root() {
        let script = doctor_script();
        assert!(
            !script.contains("EUID -eq 0"),
            "doctor should not require root"
        );
    }

    #[test]
    fn doctor_script_checks_psht_binary() {
        let script = doctor_script();
        assert!(
            script.contains("PSHT_USER_SHELL=$(getent passwd \"$PSHT_USER\""),
            "should resolve psht binary from psht user's shell"
        );
        assert!(
            script.contains("test -x \"$PSHT_USER_SHELL\""),
            "should check psht binary executable path"
        );
    }

    #[test]
    fn doctor_script_checks_psht_cli_binary() {
        let script = doctor_script();
        assert!(
            script.contains("$PSHT_HOME/bin/psht"),
            "should check psht CLI binary"
        );
    }

    #[test]
    fn doctor_script_embeds_current_version() {
        let script = doctor_script();
        assert!(
            script.contains(env!("CARGO_PKG_VERSION")),
            "should embed the current version"
        );
    }

    #[test]
    fn doctor_script_checks_psht_user() {
        let script = doctor_script();
        assert!(script.contains("id psht"), "should check psht user exists");
    }

    #[test]
    fn doctor_script_checks_user_shell() {
        let script = doctor_script();
        assert!(
            script.contains("getent passwd psht | grep -q \":$PSHT_USER_SHELL$\""),
            "should check psht user shell"
        );
    }

    #[test]
    fn doctor_script_checks_etc_shells() {
        let script = doctor_script();
        assert!(script.contains("/etc/shells"), "should check /etc/shells");
    }

    #[test]
    fn doctor_script_checks_incus_group() {
        let script = doctor_script();
        assert!(
            script.contains("id -nG psht"),
            "should check incus group membership"
        );
    }

    #[test]
    fn doctor_script_checks_incus_installed() {
        let script = doctor_script();
        assert!(
            script.contains("command -v incus"),
            "should check incus is installed"
        );
    }

    #[test]
    fn doctor_script_checks_incus_responsive() {
        let script = doctor_script();
        assert!(
            script.contains("incus info"),
            "should check incus is responsive"
        );
    }

    #[test]
    fn doctor_script_checks_tailscale() {
        let script = doctor_script();
        assert!(
            script.contains("PSHT_SKIP_TAILSCALE"),
            "tailscale checks should be guarded by PSHT_SKIP_TAILSCALE"
        );
        assert!(
            script.contains("command -v tailscale"),
            "should check tailscale is installed"
        );
        assert!(
            script.contains("tailscale status"),
            "should check tailscale is connected"
        );
    }

    #[test]
    fn doctor_script_checks_directories() {
        let script = doctor_script();
        assert!(
            script.contains("$PSHT_HOME/repos"),
            "should check repos dir"
        );
        assert!(
            script.contains("$PSHT_HOME/builds"),
            "should check builds dir"
        );
        assert!(
            script.contains("$PSHT_HOME/stacks"),
            "should check stacks dir"
        );
    }

    #[test]
    fn doctor_script_checks_stacks() {
        let script = doctor_script();
        assert!(script.contains(".sh"), "should check for stack scripts");
    }

    #[test]
    fn doctor_script_exits_nonzero_on_failure() {
        let script = doctor_script();
        assert!(
            script.contains("exit 1"),
            "should exit non-zero when checks fail"
        );
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
    fn command_entrypoints_reject_invalid_app_name() {
        for result in [
            deploy("bad/name", None, None, false),
            push("bad/name", false),
            logs("bad/name", false),
            stop("bad/name"),
            start("bad/name"),
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
        let tmp = tempfile::tempdir().unwrap();
        let path = app_runtime_state_path_in(tmp.path(), "myapp");
        let state = AppRuntimeState {
            active_instance: "psht-myapp-build-123".to_string(),
            previous_instance: Some("psht-myapp".to_string()),
            updated_at: 1234,
        };
        write_app_runtime_state_to(&path, &state).unwrap();
        let loaded = read_app_runtime_state_from(&path).unwrap().unwrap();
        assert_eq!(loaded, state);
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
    fn start_cmd_backgrounds_with_pid_file() {
        // The start command must use { } grouping so launch + PID capture are
        // synchronous, while the app process is detached in a separate session.
        // and echo writes the pid synchronously before the group exits.
        let mut vars = BTreeMap::new();
        vars.insert("HELLO".to_string(), "world".to_string());
        let cmd = start_cmd(3737, "bun run index.ts", &vars).unwrap();
        assert!(cmd.starts_with(
            "mkdir -p /var/psht && cd /app && export PORT=3737 && export HELLO='world' && {"
        ));
        assert!(cmd.contains("export PORT=3737 &&"));
        assert!(cmd.contains("export HELLO='world' &&"));
        assert!(cmd.contains("setsid sh -c 'bun run index.ts'"));
        assert!(cmd.ends_with(&format!("& echo $! > {APP_PROCESS_PID_PATH}; }}")));
    }

    #[test]
    fn app_process_probe_checks_pid_liveness() {
        let cmd = app_process_probe_cmd();
        assert!(cmd.contains(APP_PROCESS_PID_PATH));
        assert!(cmd.contains("kill -0"));
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
        assert!(cmd.contains("apt-get update -qq"));
        assert!(cmd.contains("apt-get install -y -qq"));
        assert!(cmd.contains("'curl'"));
        assert!(cmd.contains("'libssl-dev'"));
    }
}
