use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

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
fi
fi

# --- Install psht ---
INSTALLED_BIN="/usr/local/bin/psht"
if [[ "$(realpath "$PSHT_BIN")" != "$(realpath "$INSTALLED_BIN")" ]]; then
    cp "$PSHT_BIN" "$INSTALLED_BIN"
fi
chmod 755 "$INSTALLED_BIN"

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

# Apply secure ownership once user exists
if [[ -f "$PSHT_HOME/.config/tailscale-oauth" ]]; then
    chown "$PSHT_USER:$PSHT_USER" "$PSHT_HOME/.config/tailscale-oauth"
    chmod 600 "$PSHT_HOME/.config/tailscale-oauth"
fi

# Install client CLI binary for scp distribution
mkdir -p "$PSHT_HOME/bin"
if [[ -f "$PSHT_DIR/psht-cli" ]]; then
    cp "$PSHT_DIR/psht-cli" "$PSHT_HOME/bin/psht-cli"
    chmod 755 "$PSHT_HOME/bin/psht-cli"
    chown "$PSHT_USER:$PSHT_USER" "$PSHT_HOME/bin" "$PSHT_HOME/bin/psht-cli"
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

fn upgrade_script() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

PSHT_USER="psht"
PSHT_HOME="/home/$PSHT_USER"

log() {{ echo "-----> $*"; }}
err() {{ echo "ERROR: $*" >&2; exit 1; }}

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
install -m 755 "$TMPDIR/psht" /usr/local/bin/psht
install -m 755 "$TMPDIR/psht-cli" "$PSHT_HOME/bin/psht-cli"
chown "$PSHT_USER:$PSHT_USER" "$PSHT_HOME/bin/psht-cli"

# Update incus
log "Updating incus"
apt-get update -qq && apt-get install -y -qq incus

# Refresh stacks
log "Refreshing stacks"
sudo -u "$PSHT_USER" /usr/local/bin/psht init-stacks

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
check "psht binary at /usr/local/bin/psht" test -x /usr/local/bin/psht
check "psht-cli binary at \$PSHT_HOME/bin/psht-cli" test -x "$PSHT_HOME/bin/psht-cli"
INSTALLED_VERSION=$(/usr/local/bin/psht --version 2>/dev/null | awk '{{print $2}}') || INSTALLED_VERSION=""
if [[ "$INSTALLED_VERSION" == "{version}" ]]; then
    pass "psht version {version}"
else
    fail "psht version: expected {version}, got ${{INSTALLED_VERSION:-unknown}}"
fi

echo ""
echo "System:"
check "psht user exists" id psht
if getent passwd psht | grep -q ":/usr/local/bin/psht$"; then
    pass "psht user shell is /usr/local/bin/psht"
else
    fail "psht user shell is not /usr/local/bin/psht"
fi
if grep -qx "/usr/local/bin/psht" /etc/shells 2>/dev/null; then
    pass "/usr/local/bin/psht listed in /etc/shells"
else
    fail "/usr/local/bin/psht not listed in /etc/shells"
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
        let plain: String = text.replace("\x1b[2m", "").replace("\x1b[0m", "");
        assert!(
            plain.contains("ssh psht@example.com setup"),
            "missing setup"
        );
        assert!(
            plain.contains("ssh psht@example.com update"),
            "missing update"
        );
        assert!(plain.contains("ssh psht@example.com ps"), "missing ps");
        assert!(
            plain.contains("ssh psht@example.com logs [-f] <app>"),
            "missing logs"
        );
        assert!(
            plain.contains("ssh psht@example.com stop <app>"),
            "missing stop"
        );
        assert!(
            plain.contains("ssh psht@example.com start <app>"),
            "missing start"
        );
        assert!(
            plain.contains("ssh psht@example.com destroy <app>"),
            "missing destroy"
        );
    }

    #[test]
    fn help_text_mentions_deploy() {
        let text = help_text("example.com");
        let plain: String = text.replace("\x1b[2m", "").replace("\x1b[0m", "");
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
        let rm_pos = script
            .find("rm -f \"$PSHT_BIN\"")
            .expect("should rm the old binary to avoid ETXTBSY");
        let scp_pos = script.find("scp ").expect("should scp the new binary");
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
        assert!(
            !script.contains("cargo build"),
            "should not run cargo build"
        );
    }

    #[test]
    fn bootstrap_script_calls_init_stacks() {
        let script = bootstrap_script();
        assert!(
            script.contains("psht init-stacks"),
            "should call psht init-stacks"
        );
    }

    #[test]
    fn bootstrap_script_verifies_psht_in_path() {
        let script = bootstrap_script();
        assert!(
            script.contains("command -v psht"),
            "should check psht is in PATH"
        );
        assert!(
            script.contains("psht not found"),
            "should error if psht not found"
        );
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
            script.contains(
                r#"if [[ -z "${PSHT_SKIP_TAILSCALE:-}" ]]; then
if ! command -v tailscale"#
            ),
            "Tailscale SSH checks should be guarded"
        );
        // Tailscale OAuth block is guarded
        assert!(
            script.contains(
                r#"if [[ -z "${PSHT_SKIP_TAILSCALE:-}" ]]; then
OAUTH_CONFIG"#
            ),
            "Tailscale OAuth block should be guarded"
        );
        // Done banner falls back to hostname when skipped
        assert!(
            script.contains("TS_HOSTNAME=$(hostname)"),
            "should fall back to $(hostname) when Tailscale is skipped"
        );
    }

    #[test]
    fn bootstrap_script_creates_user_before_first_chown() {
        let script = bootstrap_script();
        let create_user_pos = script
            .find("# --- Create psht user ---")
            .expect("should have user creation block");
        let first_chown_pos = script
            .find("chown \"$PSHT_USER:$PSHT_USER\"")
            .expect("should have at least one chown");
        assert!(
            create_user_pos < first_chown_pos,
            "user creation must happen before any chown"
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
            script.contains("/usr/local/bin/psht"),
            "should install psht to /usr/local/bin/psht"
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
        assert!(script.contains("psht init-stacks"), "should refresh stacks");
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
            script.contains("/usr/local/bin/psht"),
            "should check psht binary"
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
            script.contains("getent passwd psht"),
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
