#!/usr/bin/env bash
set -euo pipefail

ACTION="${1:-}"
APP_NAME="${APP_NAME:-pdf-sign-check-rs}"
PROJECT_DIR="${PROJECT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
BIN_PATH="${BIN_PATH:-$PROJECT_DIR/target/release/$APP_NAME}"
ENV_FILE="${ENV_FILE:-$PROJECT_DIR/.env}"
SYSTEM_SERVICE="${SYSTEM_SERVICE:-0}"

usage() {
  echo "Usage: $0 {install|uninstall|start|stop|restart|status}"
  echo
  echo "Environment overrides:"
  echo "  APP_NAME=$APP_NAME"
  echo "  PROJECT_DIR=$PROJECT_DIR"
  echo "  BIN_PATH=$BIN_PATH"
  echo "  ENV_FILE=$ENV_FILE"
  echo "  SYSTEM_SERVICE=1  # Linux only, install as root systemd service"
}

ensure_binary() {
  if [[ ! -x "$BIN_PATH" ]]; then
    (cd "$PROJECT_DIR" && cargo build --release)
  fi
}

os_name() {
  uname -s
}

linux_service_name() {
  echo "$APP_NAME.service"
}

linux_systemctl() {
  if [[ "$SYSTEM_SERVICE" == "1" ]]; then
    systemctl "$@"
  else
    systemctl --user "$@"
  fi
}

linux_service_dir() {
  if [[ "$SYSTEM_SERVICE" == "1" ]]; then
    echo "/etc/systemd/system"
  else
    echo "$HOME/.config/systemd/user"
  fi
}

linux_install() {
  ensure_binary
  local service_name
  local service_dir
  service_name="$(linux_service_name)"
  service_dir="$(linux_service_dir)"
  mkdir -p "$service_dir"

  cat > "$service_dir/$service_name" <<EOF
[Unit]
Description=$APP_NAME
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=$PROJECT_DIR
ExecStart=$BIN_PATH
Restart=always
RestartSec=5
EnvironmentFile=-$ENV_FILE

[Install]
WantedBy=default.target
EOF

  linux_systemctl daemon-reload
  linux_systemctl enable "$service_name"
  linux_systemctl restart "$service_name"
}

linux_uninstall() {
  local service_name
  local service_dir
  service_name="$(linux_service_name)"
  service_dir="$(linux_service_dir)"

  linux_systemctl stop "$service_name" 2>/dev/null || true
  linux_systemctl disable "$service_name" 2>/dev/null || true
  rm -f "$service_dir/$service_name"
  linux_systemctl daemon-reload
}

linux_action() {
  local service_name
  service_name="$(linux_service_name)"
  case "$ACTION" in
    install) linux_install ;;
    uninstall) linux_uninstall ;;
    start|stop|restart|status) linux_systemctl "$ACTION" "$service_name" ;;
    *) usage; exit 1 ;;
  esac
}

mac_label() {
  echo "local.$APP_NAME"
}

mac_plist_path() {
  echo "$HOME/Library/LaunchAgents/$(mac_label).plist"
}

mac_bootstrap_target() {
  echo "gui/$(id -u)"
}

mac_install() {
  ensure_binary
  mkdir -p "$HOME/Library/LaunchAgents" "$PROJECT_DIR/logs"

  local plist
  local label
  plist="$(mac_plist_path)"
  label="$(mac_label)"

  cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$label</string>
  <key>ProgramArguments</key>
  <array>
    <string>$BIN_PATH</string>
  </array>
  <key>WorkingDirectory</key>
  <string>$PROJECT_DIR</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>$PROJECT_DIR/logs/service.stdout.log</string>
  <key>StandardErrorPath</key>
  <string>$PROJECT_DIR/logs/service.stderr.log</string>
</dict>
</plist>
EOF

  launchctl bootout "$(mac_bootstrap_target)" "$plist" 2>/dev/null || true
  launchctl bootstrap "$(mac_bootstrap_target)" "$plist"
  launchctl enable "$(mac_bootstrap_target)/$label"
}

mac_uninstall() {
  local plist
  plist="$(mac_plist_path)"
  launchctl bootout "$(mac_bootstrap_target)" "$plist" 2>/dev/null || true
  rm -f "$plist"
}

mac_action() {
  local plist
  local label
  plist="$(mac_plist_path)"
  label="$(mac_label)"
  case "$ACTION" in
    install) mac_install ;;
    uninstall) mac_uninstall ;;
    start) launchctl bootstrap "$(mac_bootstrap_target)" "$plist" ;;
    stop) launchctl bootout "$(mac_bootstrap_target)" "$plist" ;;
    restart)
      launchctl bootout "$(mac_bootstrap_target)" "$plist" 2>/dev/null || true
      launchctl bootstrap "$(mac_bootstrap_target)" "$plist"
      ;;
    status) launchctl print "$(mac_bootstrap_target)/$label" ;;
    *) usage; exit 1 ;;
  esac
}

case "$(os_name)" in
  Linux) linux_action ;;
  Darwin) mac_action ;;
  *) echo "Unsupported OS: $(os_name)" >&2; exit 1 ;;
esac
