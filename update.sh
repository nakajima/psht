#!/usr/bin/env bash
set -euo pipefail

PSHT_USER="psht"
PSHT_HOME="/home/$PSHT_USER"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

log() { echo "-----> $*"; }
err() { echo "ERROR: $*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || err "Run this script as root: sudo $0"

log "Building psht-server and psht"
if [[ -n "${SUDO_USER:-}" ]]; then
    sudo -iu "$SUDO_USER" cargo build --manifest-path "$SCRIPT_DIR/Cargo.toml" --bin psht-server --bin psht
else
    cargo build --manifest-path "$SCRIPT_DIR/Cargo.toml" --bin psht-server --bin psht
fi

cp "$SCRIPT_DIR/target/debug/psht-server" /usr/local/bin/psht-server
chmod 755 /usr/local/bin/psht-server

mkdir -p "$PSHT_HOME/bin"
cp "$SCRIPT_DIR/target/debug/psht" "$PSHT_HOME/bin/psht"
chmod 755 "$PSHT_HOME/bin/psht"
chown "$PSHT_USER:$PSHT_USER" "$PSHT_HOME/bin" "$PSHT_HOME/bin/psht"

mkdir -p "$PSHT_HOME/stacks"
cp "$SCRIPT_DIR"/stacks/*.sh "$PSHT_HOME/stacks/"
chown -R "$PSHT_USER:$PSHT_USER" "$PSHT_HOME/stacks"

echo "=====> Updated"
