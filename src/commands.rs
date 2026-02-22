use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
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
    ("bun", include_str!("../stacks/bun.sh")),
    ("go", include_str!("../stacks/go.sh")),
    ("node", include_str!("../stacks/node.sh")),
    ("python", include_str!("../stacks/python.sh")),
    ("rust", include_str!("../stacks/rust.sh")),
    ("static", include_str!("../stacks/static.sh")),
];

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

fn command_exists(name: &str) -> bool {
    env::var_os("PATH")
        .map(|path| {
            env::split_paths(&path).any(|dir| {
                let candidate = dir.join(name);
                candidate.is_file()
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
    let value: serde_json::Value =
        serde_json::from_str(&json).map_err(|e| format!("failed to parse tailscale status: {e}"))?;

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

fn detect_release_target() -> Result<&'static str, String> {
    let arch = run_cmd_capture("uname", &["-m"])?;
    match arch.trim() {
        "x86_64" => Ok("x86_64-unknown-linux-gnu"),
        "aarch64" => Ok("aarch64-unknown-linux-gnu"),
        other => Err(format!("unsupported architecture: {other}")),
    }
}

fn install_cli_from_release(dst: &Path) -> Result<(), String> {
    let version = env!("CARGO_PKG_VERSION");
    let target = detect_release_target()?;
    let tmpdir = run_cmd_capture("mktemp", &["-d"])?;
    let tmpdir_path = PathBuf::from(tmpdir);
    let tmpdir_s = tmpdir_path.to_string_lossy().to_string();
    let tarball = tmpdir_path.join("psht-cli.tar.gz");
    let tarball_s = tarball.to_string_lossy().to_string();
    let url = format!(
        "https://github.com/nakajima/psht/releases/download/v{version}/psht-cli-{version}-{target}.tar.gz"
    );

    let result = (|| {
        run_cmd_quiet("curl", &["-fsSL", &url, "-o", &tarball_s])?;
        run_cmd_quiet("tar", &["xzf", &tarball_s, "-C", &tmpdir_s])?;

        let extracted = tmpdir_path.join("psht-cli");
        if !extracted.is_file() {
            return Err(format!(
                "release tarball did not contain {}",
                extracted.display()
            ));
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        fs::copy(&extracted, dst)
            .map_err(|e| format!("failed to copy {} to {}: {e}", extracted.display(), dst.display()))?;
        fs::set_permissions(dst, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("failed to chmod {}: {e}", dst.display()))?;
        Ok(())
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
                .args(["build", "--release", "--bin", "psht-cli"])
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

            let built = dir.join("target/release/psht-cli");
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
            fs::copy(&built, dst)
                .map_err(|e| format!("failed to copy {} to {}: {e}", built.display(), dst.display()))?;
            fs::set_permissions(dst, fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("failed to chmod {}: {e}", dst.display()))?;
            return Ok(true);
        }
        cursor = dir.parent().map(|p| p.to_path_buf());
    }
    Ok(false)
}

fn ensure_cli_binary() -> Result<PathBuf, String> {
    let home_cli = home_dir().join("bin/psht-cli");
    if home_cli.is_file() {
        return Ok(home_cli);
    }

    let current_bin = current_psht_binary()?;
    if let Some(parent) = current_bin.parent() {
        let sibling = parent.join("psht-cli");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }

    let build_err = match build_cli_from_source(&home_cli) {
        Ok(true) => return Ok(home_cli),
        Ok(false) => None,
        Err(e) => Some(e),
    };

    if let Err(download_err) = install_cli_from_release(&home_cli) {
        if let Some(build_err) = build_err {
            return Err(format!(
                "failed to provide psht-cli (build failed: {build_err}; release download failed: {download_err})"
            ));
        }
        return Err(format!("failed to provide psht-cli: {download_err}"));
    }
    Ok(home_cli)
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

    let file_meta =
        fs::metadata(&resolved).map_err(|e| format!("failed to stat {}: {e}", resolved.display()))?;
    Ok(file_meta.permissions().mode() & 0o001 != 0)
}

fn prepare_server_binary(current_bin: &Path) -> Result<PathBuf, String> {
    let resolved = fs::canonicalize(current_bin).unwrap_or_else(|_| current_bin.to_path_buf());
    if path_is_world_executable(&resolved)? {
        return Ok(resolved);
    }

    let fallback = PathBuf::from("/usr/local/bin/psht");
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

    deploy_from(app, &code_dir)
}

fn deploy_from(app: &str, code_dir: &Path) -> Result<(), String> {
    eprintln!("-----> Detecting app type");
    let config = detect::detect(code_dir)?;
    eprintln!("       Detected: {:?}", config.app_type);

    if code_dir.join("psht-stack.sh").exists() {
        eprintln!("       Using custom stack");
    }

    let (stack, script_path) = resolve_stack(app, code_dir, config.stack())?;
    let hash = stack_hash(&script_path)?;

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
        tailscale::join_in_container(app)?;

        let port = allocate_port(app);
        eprintln!("-----> Setting up port forwarding on :{port}");
        container::add_proxy(app, port, port)?;
    }

    eprintln!("-----> Pushing code to container");
    container::push_code(app, &code_dir.to_string_lossy())?;

    if !config.install_command.is_empty() {
        eprintln!("-----> Installing dependencies");
        container::exec_cmd_rolling(app, &config.install_command, 5)?;
    }

    let port = allocate_port(app);
    eprintln!("-----> Starting app");
    let start_cmd = format!(
        "mkdir -p /var/psht && cd /app && {{ PORT={port} nohup {cmd} > /var/psht/app.log 2>&1 & echo $! > /var/psht/app.pid; }}",
        cmd = config.start_command
    );
    container::exec_cmd(app, &start_cmd)?;

    caddy::add(app, port)?;

    eprintln!("=====> App {app} deployed on port {port}");
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
    format!(
        r#"#!/bin/sh
set -e

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
  ssh "psht@{hostname}" print-cli > "$install_dir/psht"
  chmod +x "$install_dir/psht"
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
    format!(
        r#"#!/bin/sh
set -e
PSHT_BIN=$(command -v psht) || {{ echo "psht not found. Run: ssh psht@{hostname} setup | sh" >&2; exit 1; }}
current=$("$PSHT_BIN" --version 2>/dev/null | awk '{{print $2}}') || current=""
if [ "$current" = "{version}" ]; then
  echo "psht {version} (up to date)" >&2
  exit 0
fi
rm -f "$PSHT_BIN"
ssh "psht@{hostname}" print-cli > "$PSHT_BIN"
chmod +x "$PSHT_BIN"
echo "psht {version} (updated)" >&2"#
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
    let content = format!(
        "TS_OAUTH_CLIENT_ID={client_id}\nTS_OAUTH_CLIENT_SECRET={client_secret}\n"
    );
    fs::write(path, content).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(())
}

pub fn bootstrap() -> Result<(), String> {
    if run_cmd_capture("id", &["-u"])? != "0" {
        return Err("Run this command as root: sudo psht bootstrap".to_string());
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
        fs::write("/etc/apt/sources.list.d/zabbly-incus-stable.sources", source).map_err(
            |e| {
                format!("failed to write /etc/apt/sources.list.d/zabbly-incus-stable.sources: {e}")
            },
        )?;

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
                return Err("Complete the steps above and re-run: sudo psht bootstrap".to_string());
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

    let psht_cli_src = psht_dir.join("psht-cli");
    let psht_bin_dir = psht_home.join("bin");
    fs::create_dir_all(&psht_bin_dir)
        .map_err(|e| format!("failed to create {}: {e}", psht_bin_dir.display()))?;
    if psht_cli_src.exists() {
        let psht_cli_dst = psht_bin_dir.join("psht-cli");
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
    run_cmd("incus", &["project", "set", &psht_project, "restricted=true"])?;
    run_cmd(
        "incus",
        &[
            "project",
            "set",
            &psht_project,
            "restricted.devices.proxy=allow",
        ],
    )?;

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
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

PSHT_USER="psht"
PSHT_HOME="/home/$PSHT_USER"

log() {{ echo "-----> $*"; }}
err() {{ echo "ERROR: $*" >&2; exit 1; }}

PSHT_BIN=$(getent passwd "$PSHT_USER" | cut -d: -f7 || true)
if [[ -z "$PSHT_BIN" ]]; then
    PSHT_BIN=$(command -v psht) || err "psht not found in PATH"
fi
PSHT_BIN=$(realpath "$PSHT_BIN")
[[ -x "$PSHT_BIN" ]] || err "psht binary is not executable: $PSHT_BIN"

[[ $EUID -eq 0 ]] || err "Run this script as root: sudo psht upgrade"

CURRENT_VERSION="{version}"

# Detect architecture
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
    aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
    *)       err "Unsupported architecture: $ARCH" ;;
esac

# Fetch latest version from GitHub
log "Checking for updates"
LATEST=$(curl -fsSL https://api.github.com/repos/nakajima/psht/releases/latest | grep -o '"tag_name":"[^"]*"' | cut -d'"' -f4 | sed 's/^v//')

if [[ "$CURRENT_VERSION" == "$LATEST" ]]; then
    echo "psht $CURRENT_VERSION (up to date)"
    exit 0
fi

log "Upgrading psht $CURRENT_VERSION -> $LATEST"

# Set up temp directory with cleanup
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

# Download both tarballs
BASE_URL="https://github.com/nakajima/psht/releases/download/v$LATEST"
log "Downloading psht $LATEST"
curl -fsSL "$BASE_URL/psht-${{LATEST}}-${{TARGET}}.tar.gz" -o "$TMPDIR/psht.tar.gz"
curl -fsSL "$BASE_URL/psht-cli-${{LATEST}}-${{TARGET}}.tar.gz" -o "$TMPDIR/psht-cli.tar.gz"

# Extract and install
tar xzf "$TMPDIR/psht.tar.gz" -C "$TMPDIR"
tar xzf "$TMPDIR/psht-cli.tar.gz" -C "$TMPDIR"

log "Installing binaries"
install -m 755 "$TMPDIR/psht" "$PSHT_BIN"
mkdir -p "$PSHT_HOME/bin"
install -m 755 "$TMPDIR/psht-cli" "$PSHT_HOME/bin/psht-cli"
chown "$PSHT_USER:$PSHT_USER" "$PSHT_HOME/bin/psht-cli"

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
    check "psht binary at $PSHT_USER_SHELL" test -x "$PSHT_USER_SHELL"
else
    fail "psht user shell path missing"
fi
check "psht-cli binary at \$PSHT_HOME/bin/psht-cli" test -x "$PSHT_HOME/bin/psht-cli"
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
            script.contains("ssh \"psht@example.com\" print-cli > \"$install_dir/psht\""),
            "script should download CLI via ssh print-cli"
        );
        assert!(
            script.contains("chmod +x"),
            "script should make CLI executable"
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
    fn update_script_fetches_binary_via_ssh() {
        let script = update_script("example.com");
        assert!(
            script.contains("ssh \"psht@example.com\" print-cli > \"$PSHT_BIN\""),
            "should fetch the binary over ssh"
        );
    }

    #[test]
    fn update_script_removes_before_fetch() {
        let script = update_script("example.com");
        let rm_pos = script
            .find("rm -f \"$PSHT_BIN\"")
            .expect("should rm the old binary to avoid ETXTBSY");
        let fetch_pos = script
            .find("ssh \"psht@example.com\" print-cli")
            .expect("should fetch the new binary");
        assert!(rm_pos < fetch_pos, "rm must come before fetch");
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
        assert_eq!(parse_version_codename(os_release), Some("noble".to_string()));
    }

    #[test]
    fn parse_version_codename_handles_quotes() {
        let os_release = "NAME=Ubuntu\nVERSION_CODENAME=\"jammy\"\n";
        assert_eq!(parse_version_codename(os_release), Some("jammy".to_string()));
    }

    #[test]
    fn ensure_line_in_file_appends_once() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("shells");
        fs::write(&path, "/bin/sh\n").unwrap();

        ensure_line_in_file(&path, "/opt/psht/bin/psht").unwrap();
        ensure_line_in_file(&path, "/opt/psht/bin/psht").unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let count = contents
            .lines()
            .filter(|line| *line == "/opt/psht/bin/psht")
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
            script.contains("api.github.com/repos/nakajima/psht/releases/latest"),
            "should fetch latest release from GitHub"
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
            script.contains("psht-cli-"),
            "should download psht-cli tarball"
        );
        // The psht tarball (not psht-cli) should also be downloaded
        let has_psht_download = script.contains("psht-$") || script.contains("psht-${");
        assert!(has_psht_download, "should download psht tarball");
    }

    #[test]
    fn upgrade_script_installs_to_correct_paths() {
        let script = upgrade_script();
        assert!(
            script.contains("PSHT_BIN=$(getent passwd \"$PSHT_USER\""),
            "should resolve psht binary path from user shell"
        );
        assert!(
            script.contains("PSHT_BIN=$(command -v psht)"),
            "should fall back to command -v psht"
        );
        assert!(
            script.contains("install -m 755 \"$TMPDIR/psht\" \"$PSHT_BIN\""),
            "should install psht to active binary path"
        );
        assert!(
            script.contains("$PSHT_HOME/bin/psht-cli"),
            "should install psht-cli to $PSHT_HOME/bin/psht-cli"
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
        assert!(script.contains("psht-cli"), "should check psht-cli binary");
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
        // The start command must use { } grouping so only nohup is backgrounded,
        // and echo writes the pid synchronously before the group exits.
        let cmd = format!(
            "mkdir -p /var/psht && cd /app && {{ PORT={port} nohup {cmd} > /var/psht/app.log 2>&1 & echo $! > /var/psht/app.pid; }}",
            port = 3737,
            cmd = "bun run index.ts"
        );
        assert!(cmd.starts_with("mkdir -p /var/psht && cd /app && {"));
        assert!(cmd.ends_with("& echo $! > /var/psht/app.pid; }"));
    }
}
