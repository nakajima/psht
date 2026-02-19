#!/bin/sh
set -e
mkdir -p /var/psht
apt-get update -qq
apt-get install -y -qq python3 python3-pip python3-venv
ln -sf /usr/bin/python3 /usr/local/bin/python
