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

ln -sf /usr/local/cargo/bin/cargo /usr/local/bin/cargo
ln -sf /usr/local/cargo/bin/rustc /usr/local/bin/rustc
