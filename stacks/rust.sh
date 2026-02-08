#!/bin/sh
set -e
apt-get update -qq
apt-get install -y -qq curl build-essential
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ln -sf $HOME/.cargo/bin/cargo /usr/local/bin/cargo
ln -sf $HOME/.cargo/bin/rustc /usr/local/bin/rustc
