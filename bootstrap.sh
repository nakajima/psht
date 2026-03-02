#!/usr/bin/env bash
set -euo pipefail

PSHT_USER="psht"
PSHT_HOME="/home/$PSHT_USER"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

log() { echo "-----> $*"; }
err() { echo "ERROR: $*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || err "Run this script as root: sudo $0"

# --- Install Rust if missing ---
if ! command -v cargo &>/dev/null; then
    log "Installing Rust"
    export RUSTUP_HOME="/usr/local/rustup"
    export CARGO_HOME="/usr/local/cargo"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
    export PATH="/usr/local/cargo/bin:$PATH"
fi
export PATH="/usr/local/cargo/bin:$PATH"

# --- Install Incus if missing ---
if ! command -v incus &>/dev/null; then
    log "Installing Incus"
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
if ! incus profile show default &>/dev/null 2>&1; then
    log "Initializing Incus"
    incus admin init --minimal
fi

# --- Tailscale SSH ---
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

# --- Tailscale OAuth for container networking ---
OAUTH_CONFIG="$PSHT_HOME/.config/tailscale-oauth"
if [[ -f "$OAUTH_CONFIG" ]]; then
    log "Tailscale OAuth already configured"
elif [[ -n "${TS_OAUTH_CLIENT_ID:-}" && -n "${TS_OAUTH_CLIENT_SECRET:-}" ]]; then
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
        err "Complete the steps above and re-run bootstrap.sh"
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

# --- Build binaries ---
log "Building psht-server and psht"
cd "$SCRIPT_DIR"
cargo build --release --bin psht-server --bin psht
PSHT_BIN="$SCRIPT_DIR/target/release/psht-server"
[[ -f "$PSHT_BIN" ]] || err "Build failed: $PSHT_BIN not found"

INSTALLED_BIN="/usr/local/bin/psht-server"
cp "$PSHT_BIN" "$INSTALLED_BIN"
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

if [[ -f "$PSHT_HOME/.config/tailscale-oauth" ]]; then
    chown "$PSHT_USER:$PSHT_USER" "$PSHT_HOME/.config/tailscale-oauth"
    chmod 600 "$PSHT_HOME/.config/tailscale-oauth"
fi

# Install client CLI binary for scp distribution
mkdir -p "$PSHT_HOME/bin"
cp "$SCRIPT_DIR/target/release/psht" "$PSHT_HOME/bin/psht"
chmod 755 "$PSHT_HOME/bin/psht"
chown "$PSHT_USER:$PSHT_USER" "$PSHT_HOME/bin" "$PSHT_HOME/bin/psht"

# --- Grant Incus access ---
log "Adding $PSHT_USER to incus group"
usermod -aG incus "$PSHT_USER"

# Trigger creation of the user project, then allow proxy devices
sudo -u "$PSHT_USER" incus list &>/dev/null || true
PSHT_UID=$(id -u "$PSHT_USER")
incus project set "user-${PSHT_UID}" restricted.devices.proxy=allow

# --- Create directories ---
log "Setting up directories"
mkdir -p "$PSHT_HOME/repos" "$PSHT_HOME/builds" "$PSHT_HOME/stacks"
cp "$SCRIPT_DIR"/stacks/*.sh "$PSHT_HOME/stacks/"
chown -R "$PSHT_USER:$PSHT_USER" "$PSHT_HOME/repos" "$PSHT_HOME/builds" "$PSHT_HOME/stacks"

# --- Done ---
TS_HOSTNAME=$(tailscale status --json | grep -o '"DNSName":"[^"]*"' | head -1 | cut -d'"' -f4 | sed 's/\.$//')

echo ""
echo "=====> psht is ready!"
echo "       Containers will join your tailnet as <app>"
echo ""
echo "Usage:"
echo ""
echo "  cd your-app/"
echo "  git remote add psht $PSHT_USER@$TS_HOSTNAME:myapp"
echo "  git push psht main"
echo ""
echo "Commands:"
echo "  ssh $PSHT_USER@$TS_HOSTNAME ps"
echo "  ssh $PSHT_USER@$TS_HOSTNAME logs <app>"
echo "  ssh $PSHT_USER@$TS_HOSTNAME stop <app>"
