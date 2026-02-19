#!/bin/sh
set -e
mkdir -p /var/psht
apt-get update -qq
apt-get install -y -qq curl unzip
curl -fsSL https://bun.sh/install | bash
ln -sf $HOME/.bun/bin/bun /usr/local/bin/bun
