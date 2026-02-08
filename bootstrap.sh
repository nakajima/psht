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

# --- Build psht ---
log "Building psht"
cd "$SCRIPT_DIR"
cargo build --release
PSHT_BIN="$SCRIPT_DIR/target/release/psht"
[[ -f "$PSHT_BIN" ]] || err "Build failed: $PSHT_BIN not found"

INSTALLED_BIN="/usr/local/bin/psht"
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

# --- Grant Incus access ---
log "Adding $PSHT_USER to incus group"
usermod -aG incus "$PSHT_USER"

# --- Create directories ---
log "Setting up directories"
mkdir -p "$PSHT_HOME/repos" "$PSHT_HOME/builds"
chown -R "$PSHT_USER:$PSHT_USER" "$PSHT_HOME/repos" "$PSHT_HOME/builds"

# --- Done ---
TS_HOSTNAME=$(tailscale status --json | grep -o '"DNSName":"[^"]*"' | head -1 | cut -d'"' -f4 | sed 's/\.$//')

echo ""
echo "=====> psht is ready!"
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
