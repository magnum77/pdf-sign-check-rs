#!/usr/bin/env bash
set -euo pipefail

ACTION="${1:-}"
PROJECT_DIR="${PROJECT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
IMAGE_NAME="${IMAGE_NAME:-pdf-sign-check-rs:local}"
CONTAINER_NAME="${CONTAINER_NAME:-pdf-sign-check-rs}"
DOCKERFILE="${DOCKERFILE:-$PROJECT_DIR/Dockerfile}"
ENV_FILE="${ENV_FILE:-$PROJECT_DIR/.env}"
DEBUG_DIR="${DEBUG_DIR:-$PROJECT_DIR/debug}"
LOG_DIR="${LOG_DIR:-$PROJECT_DIR/logs}"
TMP_DIR="${TMP_DIR:-$PROJECT_DIR/tmp}"
HOST_IP="${HOST_IP:-127.0.0.1}"
HOST_PORT="${HOST_PORT:-3000}"
CONTAINER_PORT="${CONTAINER_PORT:-3000}"

usage() {
  cat <<EOF
Usage: $0 {build|run|restart|stop|logs|status|health}

Environment overrides:
  PROJECT_DIR=$PROJECT_DIR
  IMAGE_NAME=$IMAGE_NAME
  CONTAINER_NAME=$CONTAINER_NAME
  DOCKERFILE=$DOCKERFILE
  ENV_FILE=$ENV_FILE
  DEBUG_DIR=$DEBUG_DIR
  LOG_DIR=$LOG_DIR
  TMP_DIR=$TMP_DIR
  HOST_IP=$HOST_IP
  HOST_PORT=$HOST_PORT
  CONTAINER_PORT=$CONTAINER_PORT
EOF
}

require_macos() {
  if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "This helper is intended for macOS with Apple's native container runtime." >&2
    exit 1
  fi

  if [[ "$(uname -m)" != "arm64" ]]; then
    echo "Apple's native container runtime currently requires Apple silicon." >&2
    exit 1
  fi
}

require_container_cli() {
  if ! command -v container >/dev/null 2>&1; then
    echo "The 'container' CLI is not installed. See https://opensource.apple.com/projects/container/." >&2
    exit 1
  fi
}

require_container_system() {
  if ! container system status >/dev/null 2>&1; then
    echo "The Apple container system is not running. Start it with: container system start" >&2
    exit 1
  fi
}

ensure_dirs() {
  mkdir -p "$DEBUG_DIR" "$LOG_DIR" "$TMP_DIR"
}

build_image() {
  require_container_system
  container build --tag "$IMAGE_NAME" --file "$DOCKERFILE" "$PROJECT_DIR"
}

stop_container() {
  require_container_system
  container stop "$CONTAINER_NAME" 2>/dev/null || true
}

run_container() {
  require_container_system
  ensure_dirs
  stop_container

  local args=(
    run
    --detach
    --name "$CONTAINER_NAME"
    --rm
    --publish "${HOST_IP}:${HOST_PORT}:${CONTAINER_PORT}"
    --volume "${DEBUG_DIR}:/app/debug"
    --volume "${LOG_DIR}:/app/logs"
    --volume "${TMP_DIR}:/tmp/pdf-sign-check-rs"
  )

  if [[ -f "$ENV_FILE" ]]; then
    args+=(--env-file "$ENV_FILE")
  fi

  args+=("$IMAGE_NAME")

  container "${args[@]}"
}

show_logs() {
  require_container_system
  container logs "$CONTAINER_NAME"
}

show_status() {
  require_container_system
  if ! container ls | awk -v name="$CONTAINER_NAME" 'NR == 1 || $1 == name { print; found = 1 } END { exit found ? 0 : 1 }'; then
    echo "Container '$CONTAINER_NAME' is not running." >&2
    exit 1
  fi
}

check_health() {
  curl -fsS "http://${HOST_IP}:${HOST_PORT}/healthz"
}

require_macos
require_container_cli

case "$ACTION" in
  build) build_image ;;
  run) run_container ;;
  restart)
    stop_container
    run_container
    ;;
  stop) stop_container ;;
  logs) show_logs ;;
  status) show_status ;;
  health) check_health ;;
  *) usage; exit 1 ;;
esac
