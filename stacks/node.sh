#!/bin/sh
set -e
mkdir -p /var/psht
apt-get update -qq
apt-get install -y -qq nodejs npm
