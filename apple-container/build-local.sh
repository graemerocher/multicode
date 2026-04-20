#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
HOST_UID=$(id -u)
HOST_GID=$(id -g)
CODEX_VERSION=${CODEX_VERSION:-0.120}

container build \
  -t multicode-java25:latest \
  -t multicode-opencode-java25:latest \
  -f "$SCRIPT_DIR/Containerfile" \
  --build-arg "HOST_UID=$HOST_UID" \
  --build-arg "HOST_GID=$HOST_GID" \
  --build-arg "INSTALL_OPENCODE=1" \
  --build-arg "INSTALL_CODEX=0" \
  "$SCRIPT_DIR"

exec container build \
  -t multicode-codex-java25:latest \
  -f "$SCRIPT_DIR/Containerfile" \
  --build-arg "HOST_UID=$HOST_UID" \
  --build-arg "HOST_GID=$HOST_GID" \
  --build-arg "CODEX_VERSION=$CODEX_VERSION" \
  --build-arg "INSTALL_OPENCODE=0" \
  --build-arg "INSTALL_CODEX=1" \
  "$SCRIPT_DIR"
