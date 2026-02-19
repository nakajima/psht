use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

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

fn stack_hash(path: &Path) -> Result<String, String> {
    let contents = fs::read(path)
        .map_err(|e| format!("failed to read stack script {}: {e}", path.display()))?;
    let mut hasher = DefaultHasher::new();
    contents.hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
}

fn resolve_stack_in(app: &str, code_dir: &Path, detected_stack: &str, stacks: &Path) -> Result<(String, PathBuf), String> {
    let custom = code_dir.join("psht-stack.sh");
    if custom.exists() {
        let saved = stacks.join(format!("{app}.sh"));
        fs::copy(&custom, &saved)
            .map_err(|e| format!("failed to save custom stack: {e}"))?;
        Ok((app.to_string(), saved))
    } else {
        Ok((detected_stack.to_string(), stacks.join(format!("{detected_stack}.sh"))))
    }
}

fn resolve_stack(app: &str, code_dir: &Path, detected_stack: &str) -> Result<(String, PathBuf), String> {
    resolve_stack_in(app, code_dir, detected_stack, &stacks_dir())
}

fn checkout_code(app: &str) -> Result<PathBuf, String> {
    let build_dir = builds_dir().join(app);
    let repo_dir = repos_dir().join(format!("{app}.git"));

    if build_dir.exists() {
        fs::remove_dir_all(&build_dir)
            .map_err(|e| format!("failed to clean build dir: {e}"))?;
    }
    fs::create_dir_all(&build_dir)
        .map_err(|e| format!("failed to create build dir: {e}"))?;

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
    let hash: u32 = app.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    3001 + (hash % 1000) as u16
}

pub fn deploy(app: &str) -> Result<(), String> {
    eprintln!("-----> Deploying {app}");

    eprintln!("-----> Checking out code");
    let build_dir = checkout_code(app)?;

    deploy_from(app, &build_dir)
}

pub fn push(app: &str) -> Result<(), String> {
    eprintln!("-----> Deploying {app}");

    let code_dir = home_dir().join(app);

    if code_dir.exists() {
        fs::remove_dir_all(&code_dir)
            .map_err(|e| format!("failed to clean code dir: {e}"))?;
    }
    fs::create_dir_all(&code_dir)
        .map_err(|e| format!("failed to create code dir: {e}"))?;

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
            container::exec_cmd(app, "kill $(cat /var/psht/app.pid 2>/dev/null) 2>/dev/null || true")?;
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
        "cd /app && {{ PORT={port} nohup {cmd} > /var/psht/app.log 2>&1 & echo $! > /var/psht/app.pid; }}",
        cmd = config.start_command
    );
    container::exec_cmd(app, &start_cmd)?;

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
    container::logs(app, follow)
}

fn help_text(hostname: &str) -> String {
    let dim = "\x1b[2m";
    let reset = "\x1b[0m";
    let prefix = format!("ssh psht@{hostname} ");
    let commands: &[(&str, &str, &str)] = &[
        ("setup", " | sh", "Set up project and install CLI"),
        ("update", " | sh", "Update CLI to latest version"),
        ("ps", "", "List running apps"),
        ("logs", " [-f] <app>", "Show app logs"),
        ("stop", " <app>", "Stop an app"),
        ("start", " <app>", "Start a stopped app"),
        ("destroy", " <app>", "Stop and remove an app"),
    ];

    let mut lines = vec![
        "psht - deploy apps with psht deploy".to_string(),
        String::new(),
        "Commands:".to_string(),
    ];
    for (name, suffix, desc) in commands {
        let visible_len = name.len() + suffix.len();
        let pad = 14_usize.saturating_sub(visible_len);
        lines.push(format!(
            "  {dim}{prefix}{reset}{name}{dim}{suffix}{reset}{:pad$}  {desc}",
            "",
        ));
    }
    lines.push(String::new());
    lines.push("Deploy:".to_string());
    lines.push(format!("  psht deploy{:>21}  Deploy current directory", ""));
    lines.join("\n")
}

pub fn help() -> Result<(), String> {
    eprintln!("{}", help_text(&hostname()));
    Ok(())
}

fn setup_script(hostname: &str) -> String {
    format!(
        r#"#!/bin/sh
set -e

# Find or install psht CLI
if command -v psht >/dev/null 2>&1; then
  PSHT_BIN=$(command -v psht)
else
  printf "Install psht CLI to (default: ~/.local/bin): " >&2
  read -r install_dir < /dev/tty
  install_dir="${{install_dir:-$HOME/.local/bin}}"
  mkdir -p "$install_dir"
  scp "psht@{hostname}:bin/psht-cli" "$install_dir/psht"
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
scp "psht@{hostname}:bin/psht-cli" "$PSHT_BIN"
chmod +x "$PSHT_BIN"
echo "psht {version} (updated)" >&2"#
    )
}

pub fn update() -> Result<(), String> {
    println!("{}", update_script(&hostname()));
    Ok(())
}

fn init_stacks_in(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir)
        .map_err(|e| format!("failed to create stacks dir: {e}"))?;
    for (name, content) in STACKS {
        fs::write(dir.join(format!("{name}.sh")), content)
            .map_err(|e| format!("failed to write {name}.sh: {e}"))?;
    }
    Ok(())
}

pub fn init_stacks() -> Result<(), String> {
    init_stacks_in(&stacks_dir())
}

fn bootstrap_script() -> String {
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

PSHT_USER="psht"
PSHT_HOME="/home/$PSHT_USER"

log() {{ echo "-----> $*"; }}
err() {{ echo "ERROR: $*" >&2; exit 1; }}

[[ $EUID -eq 0 ]] || err "Run this script as root: sudo psht bootstrap"

# --- Verify psht is in PATH ---
PSHT_BIN=$(command -v psht) || err "psht not found in PATH. Install the binary first."
PSHT_DIR=$(dirname "$PSHT_BIN")

# --- Install Incus if missing ---
if ! command -v incus &>/dev/null; then
    log "Installing Incus"
    if ! command -v curl &>/dev/null; then
        apt-get update && apt-get install -y curl
    fi
    mkdir -p /etc/apt/keyrings
    curl -fsSL https://pkgs.zabbly.com/key.asc -o /etc/apt/keyrings/zabbly.asc
    cat > /etc/apt/sources.list.d/zabbly-incus-stable.sources <<EOF
Enabled: yes
Types: deb
URIs: https://pkgs.zabbly.com/incus/stable
Suites: $(. /etc/os-release && echo "$VERSION_CODENAME")
Components: main
Architectures: $(dpkg --print-architecture)
Signed-By: /etc/apt/keyrings/zabbly.asc
EOF
    apt-get update
    apt-get install -y incus
fi

# --- Initialize Incus if needed ---
# Ensure sockets are active after fresh install
systemctl start incus.socket incus-user.socket 2>/dev/null || true
if ! incus profile show default &>/dev/null 2>&1; then
    log "Initializing Incus"
    incus admin init --minimal
fi

# --- Tailscale SSH ---
if [[ -z "${{PSHT_SKIP_TAILSCALE:-}}" ]]; then
if ! command -v tailscale &>/dev/null; then
    err "Tailscale is not installed. Install it first: https://tailscale.com/download/linux"
fi

if ! tailscale status &>/dev/null; then
    err "Tailscale is not connected. Run: sudo tailscale up --ssh"
fi

if ! tailscale status --json | grep -q '"SSH":true'; then
    log "Enabling Tailscale SSH"
    tailscale up --ssh
fi

log "Tailscale SSH is active"
fi

# --- Tailscale OAuth for container networking ---
if [[ -z "${{PSHT_SKIP_TAILSCALE:-}}" ]]; then
OAUTH_CONFIG="$PSHT_HOME/.config/tailscale-oauth"
if [[ -f "$OAUTH_CONFIG" ]]; then
    log "Tailscale OAuth already configured"
elif [[ -n "${{TS_OAUTH_CLIENT_ID:-}}" && -n "${{TS_OAUTH_CLIENT_SECRET:-}}" ]]; then
    log "Setting up Tailscale OAuth from environment"
    mkdir -p "$PSHT_HOME/.config"
    cat > "$OAUTH_CONFIG" <<EOF
TS_OAUTH_CLIENT_ID=$TS_OAUTH_CLIENT_ID
TS_OAUTH_CLIENT_SECRET=$TS_OAUTH_CLIENT_SECRET
EOF
    chown "$PSHT_USER:$PSHT_USER" "$OAUTH_CONFIG"
    chmod 600 "$OAUTH_CONFIG"
else
    echo ""
    log "Setting up Tailscale OAuth for container networking"
    echo ""
    echo "       1. Ensure tag:psht exists in your ACL:"
    echo "          https://login.tailscale.com/admin/acls/visual/tags/add"
    echo ""
    echo "       2. Create a credential at:"
    echo "          https://login.tailscale.com/admin/settings/oauth"
    echo "          Under Scopes > Keys, check Write and select tag:psht."
    echo ""
    printf "       Have you completed the steps above? (y/n) "
    read -r CONFIRM < /dev/tty
    if [[ "$CONFIRM" != "y" && "$CONFIRM" != "Y" ]]; then
        err "Complete the steps above and re-run: sudo psht bootstrap"
    fi
    echo ""
    printf "OAuth client ID: "
    read -r TS_OAUTH_CLIENT_ID < /dev/tty
    printf "OAuth client secret: "
    read -r TS_OAUTH_CLIENT_SECRET < /dev/tty
    if [[ -z "$TS_OAUTH_CLIENT_ID" || -z "$TS_OAUTH_CLIENT_SECRET" ]]; then
        err "OAuth client ID and secret are required"
    fi
    mkdir -p "$PSHT_HOME/.config"
    cat > "$OAUTH_CONFIG" <<EOF
TS_OAUTH_CLIENT_ID=$TS_OAUTH_CLIENT_ID
TS_OAUTH_CLIENT_SECRET=$TS_OAUTH_CLIENT_SECRET
EOF
    chown "$PSHT_USER:$PSHT_USER" "$OAUTH_CONFIG"
    chmod 600 "$OAUTH_CONFIG"
fi
fi

# --- Install psht ---
INSTALLED_BIN="/usr/local/bin/psht"
if [[ "$(realpath "$PSHT_BIN")" != "$(realpath "$INSTALLED_BIN")" ]]; then
    cp "$PSHT_BIN" "$INSTALLED_BIN"
fi
chmod 755 "$INSTALLED_BIN"

# Install client CLI binary for scp distribution
mkdir -p "$PSHT_HOME/bin"
if [[ -f "$PSHT_DIR/psht-cli" ]]; then
    cp "$PSHT_DIR/psht-cli" "$PSHT_HOME/bin/psht-cli"
    chmod 755 "$PSHT_HOME/bin/psht-cli"
    chown "$PSHT_USER:$PSHT_USER" "$PSHT_HOME/bin" "$PSHT_HOME/bin/psht-cli"
fi

if ! grep -qx "$INSTALLED_BIN" /etc/shells; then
    log "Adding $INSTALLED_BIN to /etc/shells"
    echo "$INSTALLED_BIN" >> /etc/shells
fi

# --- Create psht user ---
if ! id "$PSHT_USER" &>/dev/null; then
    log "Creating user $PSHT_USER"
    useradd -m -s "$INSTALLED_BIN" "$PSHT_USER"
else
    log "User $PSHT_USER exists, updating shell"
    chsh -s "$INSTALLED_BIN" "$PSHT_USER"
fi

# --- Grant Incus access ---
log "Adding $PSHT_USER to incus group"
usermod -aG incus "$PSHT_USER"

# Wait for incus socket to be ready after fresh install
for i in $(seq 1 30); do
    incus info &>/dev/null && break
    sleep 1
done

# Create user project and allow proxy devices
PSHT_UID=$(id -u "$PSHT_USER")
PSHT_PROJECT="user-${{PSHT_UID}}"
incus project create "$PSHT_PROJECT" 2>/dev/null || true
incus project set "$PSHT_PROJECT" restricted=true 2>/dev/null || true
incus project set "$PSHT_PROJECT" restricted.devices.proxy=allow

# --- Create directories and write stacks ---
log "Setting up directories"
mkdir -p "$PSHT_HOME/repos" "$PSHT_HOME/builds" "$PSHT_HOME/stacks"
chown -R "$PSHT_USER:$PSHT_USER" "$PSHT_HOME/repos" "$PSHT_HOME/builds" "$PSHT_HOME/stacks"
sudo -u "$PSHT_USER" psht init-stacks

# --- Done ---
if [[ -z "${{PSHT_SKIP_TAILSCALE:-}}" ]]; then
TS_HOSTNAME=$(tailscale status --json | grep -o '"DNSName":"[^"]*"' | head -1 | cut -d'"' -f4 | sed 's/\.$//')
else
TS_HOSTNAME=$(hostname)
fi

echo ""
echo "=====> psht is ready!"
echo "       Containers will join your tailnet as psht-<app>"
echo ""
echo "Usage:"
echo ""
echo "  cd your-app/"
echo "  psht deploy"
echo ""
echo "Commands:"
echo "  ssh $PSHT_USER@$TS_HOSTNAME ps"
echo "  ssh $PSHT_USER@$TS_HOSTNAME logs <app>"
echo "  ssh $PSHT_USER@$TS_HOSTNAME stop <app>"
"#
    )
}

pub fn bootstrap() -> Result<(), String> {
    let script = bootstrap_script();
    let status = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| format!("failed to run bootstrap: {e}"))?;
    if !status.success() {
        return Err("bootstrap failed".to_string());
    }
    Ok(())
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".to_string())
}

pub fn stop(app: &str) -> Result<(), String> {
    if !container::exists(app) {
        return Err(format!("app '{app}' not found"));
    }
    eprintln!("-----> Stopping {app}");
    container::stop(app)?;
    eprintln!("=====> {app} stopped");
    Ok(())
}

pub fn start(app: &str) -> Result<(), String> {
    if !container::exists(app) {
        return Err(format!("app '{app}' not found"));
    }
    eprintln!("-----> Starting {app}");
    container::start(app)?;
    eprintln!("=====> {app} started");
    Ok(())
}

pub fn destroy(app: &str) -> Result<(), String> {
    if !container::exists(app) {
        return Err(format!("app '{app}' not found"));
    }
    eprintln!("-----> Destroying {app}");
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
            assert!(port >= 3001 && port < 4001, "port {port} out of range for {name}");
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
            script.contains("scp \"psht@example.com:bin/psht-cli\""),
            "script should download CLI via scp"
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
    fn help_text_contains_all_commands() {
        let text = help_text("example.com");
        let plain: String = text
            .replace("\x1b[2m", "")
            .replace("\x1b[0m", "");
        assert!(plain.contains("ssh psht@example.com setup"), "missing setup");
        assert!(plain.contains("ssh psht@example.com update"), "missing update");
        assert!(plain.contains("ssh psht@example.com ps"), "missing ps");
        assert!(plain.contains("ssh psht@example.com logs [-f] <app>"), "missing logs");
        assert!(plain.contains("ssh psht@example.com stop <app>"), "missing stop");
        assert!(plain.contains("ssh psht@example.com start <app>"), "missing start");
        assert!(plain.contains("ssh psht@example.com destroy <app>"), "missing destroy");
    }

    #[test]
    fn help_text_mentions_deploy() {
        let text = help_text("example.com");
        let plain: String = text
            .replace("\x1b[2m", "")
            .replace("\x1b[0m", "");
        assert!(plain.contains("psht deploy"), "missing deploy method");
        assert!(!plain.contains("git push"), "should not mention git push");
    }

    #[test]
    fn update_script_scps_binary() {
        let script = update_script("example.com");
        assert!(
            script.contains("scp \"psht@example.com:bin/psht-cli\" \"$PSHT_BIN\""),
            "should scp the binary to the existing install path"
        );
    }

    #[test]
    fn update_script_removes_before_scp() {
        let script = update_script("example.com");
        let rm_pos = script.find("rm -f \"$PSHT_BIN\"")
            .expect("should rm the old binary to avoid ETXTBSY");
        let scp_pos = script.find("scp ")
            .expect("should scp the new binary");
        assert!(rm_pos < scp_pos, "rm must come before scp");
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
    fn bootstrap_script_checks_root() {
        let script = bootstrap_script();
        assert!(script.contains("EUID -eq 0"), "should check for root");
    }

    #[test]
    fn bootstrap_script_does_not_install_rust() {
        let script = bootstrap_script();
        assert!(!script.contains("rustup"), "should not install rustup");
        assert!(!script.contains("cargo build"), "should not run cargo build");
    }

    #[test]
    fn bootstrap_script_calls_init_stacks() {
        let script = bootstrap_script();
        assert!(script.contains("psht init-stacks"), "should call psht init-stacks");
    }

    #[test]
    fn bootstrap_script_verifies_psht_in_path() {
        let script = bootstrap_script();
        assert!(script.contains("command -v psht"), "should check psht is in PATH");
        assert!(script.contains("psht not found"), "should error if psht not found");
    }

    #[test]
    fn bootstrap_script_supports_skip_tailscale() {
        let script = bootstrap_script();
        assert!(
            script.contains("PSHT_SKIP_TAILSCALE"),
            "should reference PSHT_SKIP_TAILSCALE env var"
        );
        // Tailscale SSH block is guarded
        assert!(
            script.contains(r#"if [[ -z "${PSHT_SKIP_TAILSCALE:-}" ]]; then
if ! command -v tailscale"#),
            "Tailscale SSH checks should be guarded"
        );
        // Tailscale OAuth block is guarded
        assert!(
            script.contains(r#"if [[ -z "${PSHT_SKIP_TAILSCALE:-}" ]]; then
OAUTH_CONFIG"#),
            "Tailscale OAuth block should be guarded"
        );
        // Done banner falls back to hostname when skipped
        assert!(
            script.contains("TS_HOSTNAME=$(hostname)"),
            "should fall back to $(hostname) when Tailscale is skipped"
        );
    }

    #[test]
    fn start_cmd_backgrounds_with_pid_file() {
        // The start command must use { } grouping so only nohup is backgrounded,
        // and echo writes the pid synchronously before the group exits.
        let cmd = format!(
            "cd /app && {{ PORT={port} nohup {cmd} > /var/psht/app.log 2>&1 & echo $! > /var/psht/app.pid; }}",
            port = 3737,
            cmd = "bun run index.ts"
        );
        assert!(cmd.starts_with("cd /app && {"));
        assert!(cmd.ends_with("& echo $! > /var/psht/app.pid; }"));
    }
}
