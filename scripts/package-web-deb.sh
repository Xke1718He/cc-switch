#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

require_cmd pnpm
require_cmd cargo
require_cmd dpkg
require_cmd dpkg-deb
require_cmd install
require_cmd node

VERSION="${VERSION:-$(node -p "require('./package.json').version")}"
PACKAGE_NAME="${PACKAGE_NAME:-cc-switch-web}"
ARCH="${DEB_ARCH:-$(dpkg --print-architecture)}"
MAINTAINER="${DEB_MAINTAINER:-CC Switch Maintainers <maintainers@ccswitch.local>}"
DESCRIPTION="${DEB_DESCRIPTION:-Headless browser UI for CC Switch}"

BUILD_DIR="$ROOT_DIR/src-tauri/target/package-web-deb"
OUT_DIR="$ROOT_DIR/src-tauri/target/packages"
STAGE_DIR="$BUILD_DIR/${PACKAGE_NAME}_${VERSION}_${ARCH}"
DEB_PATH="$OUT_DIR/${PACKAGE_NAME}_${VERSION}_${ARCH}.deb"
BINARY_PATH="$ROOT_DIR/src-tauri/target/release/cc-switchd"

detect_ssl_dep() {
  if ! command -v ldd >/dev/null 2>&1; then
    return 0
  fi

  local linked
  linked="$(ldd "$1" 2>/dev/null || true)"
  if printf '%s\n' "$linked" | grep -q 'libssl\.so\.3'; then
    printf ', libssl3'
  elif printf '%s\n' "$linked" | grep -q 'libssl\.so\.1\.1'; then
    printf ', libssl1.1'
  fi
}

echo "==> Building web UI"
pnpm build:web

echo "==> Building headless daemon"
cargo build \
  --manifest-path src-tauri/Cargo.toml \
  --release \
  --no-default-features \
  --features headless \
  --bin cc-switchd

if [ -n "${DEB_DEPENDS:-}" ]; then
  DEPENDS="$DEB_DEPENDS"
else
  DEPENDS="ca-certificates, xdg-utils, libc6 (>= 2.31)$(detect_ssl_dep "$BINARY_PATH")"
fi

echo "==> Staging package"
rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR/DEBIAN" "$OUT_DIR"

install -D -m 0755 \
  "$BINARY_PATH" \
  "$STAGE_DIR/usr/lib/cc-switch/cc-switchd"

mkdir -p "$STAGE_DIR/usr/share/cc-switch/web"
cp -a "$ROOT_DIR/dist/." "$STAGE_DIR/usr/share/cc-switch/web/"

install -D -m 0644 \
  "$ROOT_DIR/src-tauri/icons/128x128.png" \
  "$STAGE_DIR/usr/share/icons/hicolor/128x128/apps/cc-switch.png"

install -D -m 0644 /dev/stdin "$STAGE_DIR/usr/share/applications/cc-switch-web.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=CC Switch Web
Comment=All-in-One Assistant for Claude Code, Codex & Gemini CLI
Exec=cc-switch-web start
Icon=cc-switch
Terminal=false
Categories=Utility;Development;
StartupNotify=false
DESKTOP

install -D -m 0644 /dev/stdin "$STAGE_DIR/usr/lib/systemd/user/cc-switch-web.service" <<'SERVICE'
[Unit]
Description=CC Switch Web daemon
After=network-online.target

[Service]
Type=simple
ExecStart=/usr/bin/cc-switch-web server
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
SERVICE

install -D -m 0755 /dev/stdin "$STAGE_DIR/usr/bin/cc-switch-web" <<'WRAPPER'
#!/usr/bin/env sh
set -eu

BIN="/usr/lib/cc-switch/cc-switchd"
UI_DIR="/usr/share/cc-switch/web"
HOST="${CC_SWITCH_WEB_HOST:-127.0.0.1}"
PORT="${CC_SWITCH_WEB_PORT:-31235}"
URL="http://${HOST}:${PORT}"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/cc-switch"
PID_FILE="$CACHE_DIR/cc-switchd.pid"
LOG_FILE="$CACHE_DIR/cc-switchd.log"

is_running() {
  [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null
}

health() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsS "$URL/api/health" >/dev/null 2>&1
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- "$URL/api/health" >/dev/null 2>&1
  else
    return 1
  fi
}

open_url() {
  if command -v xdg-open >/dev/null 2>&1 && { [ -n "${DISPLAY:-}" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; }; then
    xdg-open "$URL" >/dev/null 2>&1 || true
  fi
  printf '%s\n' "$URL"
}

start_background() {
  mkdir -p "$CACHE_DIR"
  if health; then
    open_url
    return 0
  fi

  if is_running; then
    printf 'cc-switchd appears to be running, but health check failed. Log: %s\n' "$LOG_FILE" >&2
    open_url
    return 0
  fi

  CC_SWITCH_WEB_UI_DIR="$UI_DIR" \
    CC_SWITCH_WEB_HOST="$HOST" \
    CC_SWITCH_WEB_PORT="$PORT" \
    nohup "$BIN" >"$LOG_FILE" 2>&1 &
  echo "$!" >"$PID_FILE"

  i=0
  while [ "$i" -lt 50 ]; do
    if health; then
      open_url
      return 0
    fi
    i=$((i + 1))
    sleep 0.1
  done

  printf 'cc-switchd started but did not become healthy. Log: %s\n' "$LOG_FILE" >&2
  return 1
}

case "${1:-start}" in
  start)
    start_background
    ;;
  server)
    CC_SWITCH_WEB_UI_DIR="$UI_DIR" exec "$BIN"
    ;;
  open)
    open_url
    ;;
  stop)
    if is_running; then
      kill "$(cat "$PID_FILE")"
      rm -f "$PID_FILE"
      printf 'stopped cc-switchd\n'
    else
      printf 'cc-switchd is not running\n'
    fi
    ;;
  status)
    if health; then
      printf 'running: %s\n' "$URL"
    elif is_running; then
      printf 'process exists, health check failed. Log: %s\n' "$LOG_FILE"
    else
      printf 'stopped\n'
    fi
    ;;
  logs)
    if [ -f "$LOG_FILE" ]; then
      tail -n "${2:-80}" "$LOG_FILE"
    else
      printf 'no log file: %s\n' "$LOG_FILE"
    fi
    ;;
  *)
    cat <<USAGE
Usage: cc-switch-web [start|server|open|stop|status|logs]

Commands:
  start   Start daemon in the background and open the browser.
  server  Run daemon in the foreground.
  open    Print/open the browser URL.
  stop    Stop the background daemon started by this wrapper.
  status  Show daemon status.
  logs    Print daemon logs.

Environment:
  CC_SWITCH_WEB_HOST  Bind host, default 127.0.0.1.
  CC_SWITCH_WEB_PORT  Bind port, default 31235.
USAGE
    exit 2
    ;;
esac
WRAPPER

install -D -m 0644 /dev/stdin "$STAGE_DIR/usr/share/doc/cc-switch-web/README.Debian" <<'README'
CC Switch Web package
=====================

This package installs the headless CC Switch daemon and the browser UI.

Quick start:

  cc-switch-web start

Open the printed URL in a browser. By default it is:

  http://127.0.0.1:31235

Foreground server:

  cc-switch-web server

Optional systemd user service:

  systemctl --user enable --now cc-switch-web.service
  cc-switch-web open

The daemon intentionally runs as the current user so it can access the user's
own Claude Code, Codex, Gemini, OpenCode, OpenClaw, Hermes, and CC Switch
configuration files.
README

cat >"$STAGE_DIR/DEBIAN/control" <<CONTROL
Package: ${PACKAGE_NAME}
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: ${ARCH}
Maintainer: ${MAINTAINER}
Depends: ${DEPENDS}
Description: ${DESCRIPTION}
 CC Switch Web provides the headless browser UI for CC Switch.
 It avoids the Tauri/WebKitGTK desktop runtime and is suitable for
 systems where the native WebView stack is unavailable or unreliable.
CONTROL

cat >"$STAGE_DIR/DEBIAN/postinst" <<'POSTINST'
#!/usr/bin/env sh
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database -q /usr/share/applications || true
fi
exit 0
POSTINST
chmod 0755 "$STAGE_DIR/DEBIAN/postinst"

cat >"$STAGE_DIR/DEBIAN/postrm" <<'POSTRM'
#!/usr/bin/env sh
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database -q /usr/share/applications || true
fi
exit 0
POSTRM
chmod 0755 "$STAGE_DIR/DEBIAN/postrm"

du -sh "$STAGE_DIR" | awk '{ print "==> Package payload size: " $1 }'

echo "==> Building deb"
dpkg-deb --build --root-owner-group "$STAGE_DIR" "$DEB_PATH"

echo "==> Wrote $DEB_PATH"
echo "Install with:"
echo "  sudo apt install \"$DEB_PATH\""
