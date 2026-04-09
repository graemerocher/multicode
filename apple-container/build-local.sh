#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

exec container build \
  -t multicode-java25:latest \
  -f "$SCRIPT_DIR/Containerfile" \
  --build-arg "HOST_UID=$(id -u)" \
  --build-arg "HOST_GID=$(id -g)" \
  "$SCRIPT_DIR"
