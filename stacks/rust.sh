#!/bin/sh
set -e
mkdir -p /var/psht
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq ca-certificates curl build-essential

# Use rustup, but force curl backend because reqwest transport fails in some
# environments despite curl succeeding.
export RUSTUP_USE_CURL=1
export RUSTUP_HOME=/usr/local/rustup
export CARGO_HOME=/usr/local/cargo
curl --retry 5 --retry-all-errors --retry-delay 2 --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --profile minimal --default-toolchain stable

# Ensure toolchain selection survives non-login `sh -c` runs by using wrappers
# that pin rustup/cargo homes and invoke the stable toolchain explicitly.
cat >/usr/local/bin/cargo <<'EOF'
#!/bin/sh
export RUSTUP_HOME=/usr/local/rustup
export CARGO_HOME=/usr/local/cargo
exec /usr/local/cargo/bin/rustup run stable cargo "$@"
EOF
chmod 755 /usr/local/bin/cargo

cat >/usr/local/bin/rustc <<'EOF'
#!/bin/sh
export RUSTUP_HOME=/usr/local/rustup
export CARGO_HOME=/usr/local/cargo
exec /usr/local/cargo/bin/rustup run stable rustc "$@"
EOF
chmod 755 /usr/local/bin/rustc
