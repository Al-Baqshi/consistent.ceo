#!/usr/bin/env sh
# One-line install:
#   curl -fsSL https://raw.githubusercontent.com/Al-Baqshi/consistent.ceo/main/install.sh | sh
#
# Installs the latest consistent-world gateway binary (dashboard embedded) for
# the current OS/arch into ~/.local/bin (or PREFIX/bin if PREFIX is set), then
# sets it to run in the background (LaunchAgent on macOS, a systemd user unit
# on Linux). Nothing here talks to a server other than GitHub.
#
# Binaries are the assets CI attaches to each tagged GitHub release
# (.github/workflows/ci.yml, `release` job). Overrides:
#   CONSISTENT_WORLD_REPO=owner/name        install from a fork
#   CONSISTENT_WORLD_DOWNLOAD_URL=<dir>     install from your own mirror
#   CONSISTENT_WORLD_NO_SERVICE=1           do not install the background service
#
# NOTE: sveltfront/static/install.sh is a byte-for-byte copy served by the
# gateway itself; keep both in sync.

set -e

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="${PREFIX}/bin"
REPO="${CONSISTENT_WORLD_REPO:-Al-Baqshi/consistent.ceo}"
DOWNLOAD_URL="${CONSISTENT_WORLD_DOWNLOAD_URL:-https://github.com/${REPO}/releases/latest/download}"

detect_target() {
  os=$(uname -s)
  arch=$(uname -m)
  case "$os" in
    Darwin)
      case "$arch" in
        x86_64) echo "x86_64-apple-darwin" ;;
        arm64|aarch64) echo "aarch64-apple-darwin" ;;
        *) echo "unsupported: $os $arch" ;;
      esac ;;
    Linux)
      case "$arch" in
        x86_64) echo "x86_64-unknown-linux-gnu" ;;
        *) echo "unsupported: $os $arch" ;;
      esac ;;
    *)
      echo "unsupported: $os $arch" ;;
  esac
}

from_source_hint() {
  echo "Build it from source instead (needs Rust and Node):" >&2
  echo "  git clone https://github.com/${REPO}.git && cd \$(basename ${REPO})" >&2
  echo "  cd sveltfront && npm ci && npm run build && cd .." >&2
  echo "  cargo install --path crates/gateway --features dashboard" >&2
}

target=$(detect_target)
if echo "$target" | grep -q unsupported; then
  echo "No prebuilt binary for $(uname -s) $(uname -m)." >&2
  from_source_hint
  exit 1
fi

tarball="consistent-world-${target}.tar.gz"
url="${DOWNLOAD_URL}/${tarball}"

echo "Installing consistent-world for ${target} into ${BIN_DIR}"
mkdir -p "$BIN_DIR"
tmp=$(mktemp -d 2>/dev/null || mktemp -d -t consistent-world)
trap 'rm -rf "$tmp"' EXIT

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  echo "Need curl or wget" >&2
  exit 1
fi

if ! fetch "$url" "$tmp/$tarball"; then
  echo "Could not download ${url}" >&2
  echo "Check https://github.com/${REPO}/releases for a published release." >&2
  from_source_hint
  exit 1
fi
tar -xzf "$tmp/$tarball" -C "$BIN_DIR"

chmod +x "${BIN_DIR}/consistent-world"
echo "Installed: ${BIN_DIR}/consistent-world"
if ! echo "$PATH" | grep -q "$BIN_DIR"; then
  echo "Add to PATH: export PATH=\"${BIN_DIR}:\$PATH\""
fi

# ── Always-on (macOS) ────────────────────────────────────────────────────────
#
# A phone that can reach your Mac is only useful while the gateway is running.
# Install a LaunchAgent — deliberately an *Agent*, not a daemon: the gateway
# needs the user's login keychain, the user's `claude` CLI session, and (for
# browser work) a real user session. A daemon has none of those.
#
# Opt out with CONSISTENT_WORLD_NO_SERVICE=1.
install_launchd_service() {
  [ "$(uname -s)" = "Darwin" ] || return 0
  [ -z "${CONSISTENT_WORLD_NO_SERVICE:-}" ] || return 0
  command -v launchctl >/dev/null 2>&1 || return 0

  label="world.consistent"
  plist_dir="${HOME}/Library/LaunchAgents"
  plist="${plist_dir}/${label}.plist"
  log_dir="${HOME}/Library/Logs/consistent-world"
  mkdir -p "$plist_dir" "$log_dir"

  # Rendered at install time: the hardcoded /usr/local paths in the repo
  # template never matched where this script actually installs the binary.
  cat > "$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>${BIN_DIR}/consistent-world</string>
    <string>start</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>${log_dir}/gateway.log</string>
  <key>StandardErrorPath</key>
  <string>${log_dir}/gateway.err</string>
  <key>WorkingDirectory</key>
  <string>${HOME}</string>
</dict>
</plist>
PLIST

  # bootout first so re-running the installer repairs an existing (possibly
  # stale-path) service instead of silently keeping the old one.
  launchctl bootout "gui/$(id -u)/${label}" >/dev/null 2>&1 || true
  if launchctl bootstrap "gui/$(id -u)" "$plist" >/dev/null 2>&1; then
    echo "Always-on: installed and started (launchctl ${label})"
    echo "  Logs: ${log_dir}/gateway.log"
    echo "  Stop: launchctl bootout gui/$(id -u)/${label}"
  else
    echo "Note: could not start the background service; run 'consistent-world start' manually."
  fi
}

# ── Always-on (Linux) ────────────────────────────────────────────────────────
#
# A systemd *user* unit, for the same reasons the macOS side uses an Agent: the
# gateway runs as you, with your CLI sign-ins and your files. `loginctl
# enable-linger` keeps it running after you log out (it asks for sudo, so it is
# printed rather than run).
install_systemd_service() {
  [ "$(uname -s)" = "Linux" ] || return 0
  [ -z "${CONSISTENT_WORLD_NO_SERVICE:-}" ] || return 0
  command -v systemctl >/dev/null 2>&1 || return 0
  systemctl --user show-environment >/dev/null 2>&1 || return 0

  unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  mkdir -p "$unit_dir"
  cat > "${unit_dir}/consistent-world.service" <<UNIT
[Unit]
Description=Consistent CEO gateway
After=network-online.target

[Service]
ExecStart=${BIN_DIR}/consistent-world start
Restart=on-failure
RestartSec=5
WorkingDirectory=${HOME}

[Install]
WantedBy=default.target
UNIT
  systemctl --user daemon-reload
  if systemctl --user enable --now consistent-world.service >/dev/null 2>&1; then
    echo "Always-on: installed and started (systemctl --user status consistent-world)"
    echo "  Logs: journalctl --user -u consistent-world -f"
    echo "  Keep it running after logout: sudo loginctl enable-linger $(id -un)"
  else
    echo "Note: could not start the background service; run 'consistent-world start' manually."
  fi
}

install_launchd_service
install_systemd_service

echo
echo "Open http://127.0.0.1:18800 to set up your account and pick a brain."
echo
echo "Next: reach it from your phone —"
echo "  consistent-world remote setup   # make it reachable from anywhere (Tailscale)"
echo "  consistent-world pair           # show the QR to scan in the mobile app"
