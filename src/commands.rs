use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

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

const DEFAULT_FORGE_URL: &str = "https://git.fishmt.net/nakajima/psht";
const START_COMMAND_PATH: &str = "/etc/psht-start-command";

fn home_dir() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/home/psht".to_string()))
}

fn builds_dir() -> PathBuf {
    home_dir().join("builds")
}

fn repos_dir() -> PathBuf {
    home_dir().join("repos")
}

fn stacks_dir() -> PathBuf {
    home_dir().join("stacks")
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
    let forge_url = configured_forge_url();
    let latest_url = run_cmd_capture(
        "curl",
        &[
            "-fsSL",
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

fn checkout_code(app: &str) -> Result<PathBuf, String> {
    let build_dir = builds_dir().join(app);
    let repo_dir = repos_dir().join(format!("{app}.git"));

    if build_dir.exists() {
        fs::remove_dir_all(&build_dir).map_err(|e| format!("failed to clean build dir: {e}"))?;
    }
    fs::create_dir_all(&build_dir).map_err(|e| format!("failed to create build dir: {e}"))?;

    let status = Command::new("git")
        .args(["clone", "--depth", "1"])
        .arg(&repo_dir)
        .arg(&build_dir)
        .status()
        .map_err(|e| format!("failed to checkout code: {e}"))?;
    if !status.success() {
        return Err("git clone failed".to_string());
    }

    Ok(build_dir)
}

fn allocate_port(app: &str) -> u16 {
    // Simple deterministic port allocation based on app name hash
    let hash: u32 = app
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    3001 + (hash % 1000) as u16
}

fn start_cmd(port: u16, cmd: &str) -> String {
    let escaped = shell_quote(cmd);
    format!(
        "mkdir -p /var/psht && cd /app && export PORT={port} && {{ setsid sh -c {escaped} > /var/psht/app.log 2>&1 < /dev/null & echo $! > /var/psht/app.pid; }}"
    )
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

pub fn deploy(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    eprintln!("-----> Deploying {app}");

    eprintln!("-----> Checking out code");
    let build_dir = checkout_code(app)?;

    deploy_from(app, &build_dir)
}

pub fn push(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    eprintln!("-----> Deploying {app}");

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
        if container::exists(app) && read_binary_hash(app).as_deref() == Some(hash) {
            eprintln!("-----> Binary unchanged ({hash}), skipping deploy");
            return Ok(());
        }
    }

    deploy_from(app, &code_dir)
}

fn deploy_from(app: &str, code_dir: &Path) -> Result<(), String> {
    let current_uid = run_cmd_capture("id", &["-u"])?;
    let current_project = format!("user-{}", current_uid.trim());
    if command_succeeds("incus", &["project", "show", &current_project]) {
        ensure_project_default_profile(&current_project)?;
    }

    eprintln!("-----> Detecting app type");
    let config = detect::detect(code_dir)?;
    eprintln!("       Detected: {:?}", config.app_type);
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

    let mut tailnet_hostname = tailscale::dns_name_in_container(app);
    let needs_setup = if container::exists(app) {
        let remote_hash = container::exec_output(app, "cat /etc/psht-setup-hash 2>/dev/null")
            .unwrap_or_default()
            .trim()
            .to_string();
        if remote_hash == hash {
            eprintln!("-----> Reusing container");
            container::exec_cmd(
                app,
                "kill $(cat /var/psht/app.pid 2>/dev/null) 2>/dev/null || true",
            )?;
            false
        } else {
            eprintln!("-----> Rebuilding container");
            let _ = container::stop(app);
            let _ = container::delete(app);
            true
        }
    } else {
        true
    };

    if needs_setup {
        if container::image_exists(&stack, &hash) {
            eprintln!("-----> Creating container from cached image");
            container::create_from_image(app, &stack, &hash)?;

            eprintln!("-----> Installing tailscale");
            tailscale::install_in_container(app)?;
        } else {
            eprintln!("-----> Creating container");
            container::create(app)?;

            eprintln!("-----> Installing tailscale");
            tailscale::install_in_container(app)?;

            eprintln!("-----> Setting up runtime");
            container::push_file(app, &script_path.to_string_lossy(), "/tmp/setup.sh")?;
            container::exec_cmd_rolling(app, "chmod +x /tmp/setup.sh && /tmp/setup.sh", 5)?;

            eprintln!("-----> Caching stack image");
            if let Err(e) = container::publish_image(app, &stack, &hash) {
                eprintln!("       Warning: failed to cache stack image: {e}");
            }
        }

        container::exec_cmd(app, &format!("echo -n '{hash}' > /etc/psht-setup-hash"))?;

        eprintln!("-----> Connecting to tailnet");
        tailnet_hostname = tailscale::join_in_container(app)?;

        let port = allocate_port(app);
        eprintln!("-----> Setting up port forwarding on :{port}");
        container::add_proxy(app, port, port)?;
    }

    eprintln!("-----> Pushing code to container");
    container::push_code(app, &code_dir.to_string_lossy())?;
    persist_start_command(app, &config.start_command)?;

    if !config.install_command.is_empty() {
        eprintln!("-----> Installing dependencies");
        container::exec_cmd_rolling(app, &config.install_command, 5)?;
    }

    let port = allocate_port(app);
    eprintln!("-----> Starting app");
    let start_cmd = start_cmd(port, &config.start_command);
    container::exec_cmd(app, &start_cmd)?;

    tailnet_hostname = tailnet_hostname.or_else(|| tailscale::dns_name_in_container(app));
    if tailnet_hostname.is_some() {
        if let Err(e) = tailscale::expose_http_in_container(app, port) {
            eprintln!("       Warning: failed to expose tailnet HTTP on :80: {e}");
        }
    }

    caddy::add(app, port)?;

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

pub fn ps() -> Result<(), String> {
    let containers = container::list()?;
    if containers.is_empty() {
        println!("No apps running.");
        return Ok(());
    }
    println!("{:<20} {:<10}", "APP", "STATUS");
    for c in &containers {
        let app = c.name.strip_prefix("psht-").unwrap_or(&c.name);
        println!("{:<20} {:<10}", app, c.status);
    }
    Ok(())
}

pub fn logs(app: &str, follow: bool) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    container::logs(app, follow)
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

detect_target() {{
  arch=$(uname -m)
  case "$arch" in
    x86_64) echo "x86_64-unknown-linux-gnu" ;;
    aarch64|arm64) echo "aarch64-unknown-linux-gnu" ;;
    *) echo "unsupported architecture: $arch" >&2; exit 1 ;;
  esac
}}

install_cli() {{
  install_dir="$1"
  target=$(detect_target)
  asset_url="$FORGE_URL/releases/download/v$VERSION/psht-$VERSION-$target.tar.gz"
  tmpdir=$(mktemp -d)
  curl -fsSL "$asset_url" -o "$tmpdir/psht.tar.gz"
  tar xzf "$tmpdir/psht.tar.gz" -C "$tmpdir"
  install -m 755 "$tmpdir/psht" "$install_dir/psht"
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

detect_target() {{
  arch=$(uname -m)
  case "$arch" in
    x86_64) echo "x86_64-unknown-linux-gnu" ;;
    aarch64|arm64) echo "aarch64-unknown-linux-gnu" ;;
    *) echo "unsupported architecture: $arch" >&2; exit 1 ;;
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
curl -fsSL "$asset_url" -o "$tmpdir/psht.tar.gz"
tar xzf "$tmpdir/psht.tar.gz" -C "$tmpdir"
candidate="$tmpdir/psht"
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
    let version = env!("CARGO_PKG_VERSION");
    let forge_url = configured_forge_url();
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

PSHT_USER="psht"
PSHT_HOME="/home/$PSHT_USER"
FORGE_URL="${{PSHT_FORGE_URL:-{forge_url}}}"
FORGE_URL="${{FORGE_URL%/}}"

log() {{ echo "-----> $*"; }}
err() {{ echo "ERROR: $*" >&2; exit 1; }}

PSHT_BIN=$(getent passwd "$PSHT_USER" | cut -d: -f7 || true)
if [[ -z "$PSHT_BIN" ]]; then
    PSHT_BIN=$(command -v psht-server) || err "psht-server not found in PATH"
fi
PSHT_BIN=$(realpath "$PSHT_BIN")
[[ -x "$PSHT_BIN" ]] || err "psht-server binary is not executable: $PSHT_BIN"

[[ $EUID -eq 0 ]] || err "Run this script as root: sudo psht-server upgrade"

CURRENT_VERSION="{version}"

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

if [[ "$CURRENT_VERSION" == "$LATEST" ]]; then
    echo "psht $CURRENT_VERSION (up to date)"
    exit 0
fi

log "Upgrading psht $CURRENT_VERSION -> $LATEST"

# Set up temp directory with cleanup
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

# Download both tarballs
BASE_URL="$FORGE_URL/releases/download/v$LATEST"
log "Downloading psht $LATEST"
curl -fsSL "$BASE_URL/psht-server-${{LATEST}}-${{TARGET}}.tar.gz" -o "$TMPDIR/psht-server.tar.gz"
curl -fsSL "$BASE_URL/psht-${{LATEST}}-${{TARGET}}.tar.gz" -o "$TMPDIR/psht.tar.gz"

# Extract and install
tar xzf "$TMPDIR/psht-server.tar.gz" -C "$TMPDIR"
tar xzf "$TMPDIR/psht.tar.gz" -C "$TMPDIR"

log "Installing binaries"
install -m 755 "$TMPDIR/psht-server" "$PSHT_BIN"
mkdir -p "$PSHT_HOME/bin"
install -m 755 "$TMPDIR/psht" "$PSHT_HOME/bin/psht"
chown "$PSHT_USER:$PSHT_USER" "$PSHT_HOME/bin/psht"

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

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".to_string())
}

pub fn stop(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    if !container::exists(app) {
        return Err(format!("app '{app}' not found"));
    }
    eprintln!("-----> Stopping {app}");
    container::stop(app)?;
    eprintln!("=====> {app} stopped");
    Ok(())
}

pub fn start(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    if !container::exists(app) {
        return Err(format!("app '{app}' not found"));
    }
    eprintln!("-----> Starting {app}");
    container::start(app)?;
    let command = read_start_command(app)?;
    let port = allocate_port(app);
    container::exec_cmd(app, &start_cmd(port, &command))?;
    if tailscale::dns_name_in_container(app).is_some()
        && let Err(e) = tailscale::expose_http_in_container(app, port)
    {
        eprintln!("       Warning: failed to expose tailnet HTTP on :80: {e}");
    }
    eprintln!("=====> {app} started");
    Ok(())
}

pub fn destroy(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    if !container::exists(app) {
        return Err(format!("app '{app}' not found"));
    }
    eprintln!("-----> Destroying {app}");
    caddy::remove(app)?;
    container::stop(app)?;
    container::delete(app)?;
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
        assert!(
            script.contains("Install psht CLI"),
            "script should install the CLI"
        );
        assert!(
            script.contains("PSHT_FORGE_URL"),
            "script should support overriding forge URL via PSHT_FORGE_URL"
        );
        assert!(
            script.contains(
                "asset_url=\"$FORGE_URL/releases/download/v$VERSION/psht-$VERSION-$target.tar.gz\""
            ),
            "script should download CLI tarball from forge releases"
        );
        assert!(
            script.contains("curl -fsSL \"$asset_url\""),
            "script should fetch CLI with curl"
        );
        assert!(
            script.contains("tar xzf \"$tmpdir/psht.tar.gz\""),
            "script should extract downloaded CLI tarball"
        );
        assert!(
            script.contains("install -m 755 \"$tmpdir/psht\" \"$install_dir/psht\""),
            "script should install CLI binary"
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
        assert!(
            script.contains("PSHT_FORGE_URL"),
            "should support overriding forge URL via PSHT_FORGE_URL"
        );
        assert!(
            script.contains("asset_url=\"$FORGE_URL/releases/download/v"),
            "should build release asset URL from forge"
        );
        assert!(
            script.contains("curl -fsSL \"$asset_url\""),
            "should download CLI tarball from forge"
        );
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
    fn upgrade_script_checks_root() {
        let script = upgrade_script();
        assert!(script.contains("EUID -eq 0"), "should check for root");
    }

    #[test]
    fn upgrade_script_embeds_current_version() {
        let script = upgrade_script();
        assert!(
            script.contains(env!("CARGO_PKG_VERSION")),
            "should embed the current version"
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
            script.contains("install -m 755 \"$TMPDIR/psht-server\" \"$PSHT_BIN\""),
            "should install psht-server to active binary path"
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

    #[test]
    fn command_entrypoints_reject_invalid_app_name() {
        for result in [
            deploy("bad/name"),
            push("bad/name"),
            logs("bad/name", false),
            stop("bad/name"),
            start("bad/name"),
            destroy("bad/name"),
        ] {
            let err = result.expect_err("should reject invalid app name");
            assert!(err.contains("invalid app name"), "unexpected error: {err}");
        }
    }

    #[test]
    fn start_cmd_backgrounds_with_pid_file() {
        // The start command must use { } grouping so launch + PID capture are
        // synchronous, while the app process is detached in a separate session.
        // and echo writes the pid synchronously before the group exits.
        let cmd = start_cmd(3737, "bun run index.ts");
        assert!(cmd.starts_with("mkdir -p /var/psht && cd /app && export PORT=3737 && {"));
        assert!(cmd.contains("export PORT=3737 &&"));
        assert!(cmd.contains("setsid sh -c 'bun run index.ts'"));
        assert!(cmd.ends_with("& echo $! > /var/psht/app.pid; }"));
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
}
