#!/bin/sh
set -e
apt-get update -qq
apt-get install -y -qq curl unzip
curl -fsSL https://bun.sh/install | bash
ln -sf $HOME/.bun/bin/bun /usr/local/bin/bun
