#!/usr/bin/env bash
#
# jancode — install & deploy script for Linux (Ubuntu/Debian, RHEL/Fedora,
# and any distro with a Rust toolchain).
#
# What it does:
#   1. Detects the OS and required toolchain (Rust/cargo).
#   2. Builds jancode in release mode (single static-ish binary).
#   3. Installs the binary to a configurable PREFIX (default /usr/local/bin).
#   4. Creates $JANCODE_HOME (default ~/.jancode) with a safe config.toml
#      template you fill with your provider key.
#   5. Optionally installs a systemd service for the daemon (system-level) or
#      a user-level one, so `jancode` stays awake as a deployable service.
#
# Usage:
#   ./install.sh                    # build + install to /usr/local/bin
#   ./install.sh --prefix ~/.local  # install under user prefix
#   ./install.sh --skip-build       # install an already-built ./target/release/jancode
#   ./install.sh --service system   # + install systemd system service
#   ./install.sh --service user     # + install systemd user service
#   ./install.sh --uninstall        # remove binary + config created by this script
#
set -euo pipefail

BIN_NAME="jancode"
PREFIX="/usr/local"
SKIP_BUILD=0
SERVICE_MODE="none"   # none | system | user
UNINSTALL=0

usage() {
  sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix=*) PREFIX="${1#*=}" ;;
    --prefix) PREFIX="$2"; shift ;;
    --skip-build) SKIP_BUILD=1 ;;
    --service=system) SERVICE_MODE="system" ;;
    --service=user) SERVICE_MODE="user" ;;
    --uninstall) UNINSTALL=1 ;;
    --help|-h) usage 0 ;;
    *) echo "unknown option: $1" >&2; usage 1 ;;
  esac
  shift
done

log()  { printf '\033[1;32m[jancode]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[jancode:WARN]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[jancode:ERR]\033[0m %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# OS detection
# ---------------------------------------------------------------------------
detect_os() {
  if [ "$(uname -s)" = "Linux" ]; then
    if [ -f /etc/os-release ]; then
      . /etc/os-release
      log "OS detected: ${PRETTY_NAME:-Linux}"
    else
      log "OS detected: Linux"
    fi
  elif [ "$(uname -s)" = "Darwin" ]; then
    warn "macOS detected — jancode is built for Linux, but the cargo build should still work."
  else
    warn "Unrecognized OS: $(uname -s); attempting build anyway."
  fi
}

# ---------------------------------------------------------------------------
# Toolchain check
# ---------------------------------------------------------------------------
check_rust() {
  # rustup installs to ~/.cargo/bin which is often not on a non-login PATH.
  for cand in "$HOME/.cargo/bin" "/usr/local/cargo/bin" "/usr/bin" "/opt/cargo/bin"; do
    if [ -x "$cand/cargo" ]; then
      export PATH="$cand:$PATH"
      break
    fi
  done
  if command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1; then
    log "Rust toolchain found (cargo $(cargo --version | awk '{print $2}'))"
    return 0
  fi
  aid=$(command -v apt-get || true)
  dnf_=$(command -v dnf || true)
  if [ -n "$aid" ] && [ "$(id -u)" -eq 0 ]; then
    log "Rust not found. Installing via apt-get..."
    apt-get update -y && apt-get install -y cargo rustc
  elif [ -n "$dnf_" ] && [ "$(id -u)" -eq 0 ]; then
    log "Rust not found. Installing via dnf..."
    dnf install -y cargo rust
  else
    die "Rust/cargo not found. Install it: 'curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh' then re-run, or run this script as root so it can install the distro Rust package."
  fi
  command -v cargo >/dev/null 2>&1 || die "cargo still not available after install."
}

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
build() {
  local here
  here="$(cd "$(dirname "$0")" && pwd)"
  log "Building release binary in $here ..."
  (cd "$here" && cargo build --release)
  local bin="$here/target/release/$BIN_NAME"
  [ -x "$bin" ] || die "build did not produce $bin"
  log "Build OK: $bin"
}

# ---------------------------------------------------------------------------
# Install binary
# ---------------------------------------------------------------------------
install_binary() {
  local src dest dir
  if [ "$SKIP_BUILD" -eq 1 ]; then
    here="$(cd "$(dirname "$0")" && pwd)"
    src="$here/target/release/$BIN_NAME"
    [ -x "$src" ] || die "--skip-build given but $src does not exist; run 'cargo build --release' first."
    log "Using existing build: $src"
  else
    build
    here="$(cd "$(dirname "$0")" && pwd)"
    src="$here/target/release/$BIN_NAME"
  fi
  dest="$PREFIX/bin/$BIN_NAME"
  dir="$(dirname "$dest")"
  if mkdir -p "$dir" 2>/dev/null && [ -w "$dir" ]; then
    install -m 0755 "$src" "$dest"
  else
    log "Installing to $dest (need sudo):"
    sudo mkdir -p "$dir"
    sudo install -m 0755 "$src" "$dest"
  fi
  log "Installed: $dest"
  "$dest" --help >/dev/null 2>&1 || die "installed binary at $dest failed to run."
  log "Binary verified."
}

# ---------------------------------------------------------------------------
# Config + data dirs
# ---------------------------------------------------------------------------
setup_config() {
  local home="${JANCODE_HOME:-$HOME/.jancode}"
  mkdir -p "$home"
  if [ ! -f "$home/config.toml" ]; then
    cat > "$home/config.toml" << EOF
# jancode configuration — edit this file.
[server]
idle_timeout_secs = 300
# Approval policy for risky tool calls (file writes/edits/patches and reads
# outside the working directory). "prompt" is interactive-only; "auto" never
# asks; "deny" always blocks.
# approve_mode = "prompt"

[provider]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
default_model = "gpt-4o-mini"

# [[mcp.servers]]
# name = "ops"
# url = "https://your-mcp-server.example.com/mcp"
# transport = "http-streamable"
# bearer_env = "OPS_MCP_KEY"
EOF
    log "Created config template: $home/config.toml"
    log " -> set your API key in \$OPENAI_API_KEY (or put api_key in config.toml)."
  else
    warn "config.toml already exists in $home, leaving it untouched."
  fi
  chmod 700 "$home" 2>/dev/null || true
  log "Config dir: $home ('JANCODE_HOME' override supported)"
}

# ---------------------------------------------------------------------------
# Optional: runtime dir (for the unix socket)
# ---------------------------------------------------------------------------
setup_runtime_dir() {
  local rt="${JANCODE_RUNTIME_DIR:-$(get_runtime_dir_default)}"
  if [ "$SERVICE_MODE" = "system" ]; then
    # For the systemd service we pin a fixed runtime dir under /run.
    printf '%s' '/run/jancode'
    return
  fi
  printf '%s' "$rt"
}

get_runtime_dir_default() {
  # XDG runtime dir when present, else fall back to /tmp (default in config.rs
  # is a per-user temp dir).
  if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
    printf '%s' "$XDG_RUNTIME_DIR"
  else
    printf '%s' "/tmp"
  fi
}

# ---------------------------------------------------------------------------
# systemd service (deployment mode)
# ---------------------------------------------------------------------------
install_service() {
  local unit user runtime_home exec_path
  exec_path="$PREFIX/bin/$BIN_NAME"
  runtime_home="${JANCODE_HOME:-$HOME/.jancode}"

  if [ "$SERVICE_MODE" = "system" ]; then
    unit="/etc/systemd/system/$BIN_NAME.service"
    user="root"
    [ "$(id -u)" -eq 0 ] && user="$USER" || user="root"
    # For a system service the daemon should run as a dedicated user when
    # possible; fall back to root if was run with sudo already.
    if [ "$(id -u)" -eq 0 ]; then
      user="${SUDO_USER:-root}"
    else
      user="$USER"
    fi
    cat > "$unit" << EOF
[Unit]
Description=jancode AI coding agent daemon
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=$exec_path serve
Environment=JANCODE_HOME=$runtime_home
Environment=JANCODE_RUNTIME_DIR=/run/jancode
Restart=on-failure
RestartSec=3
User=$user
Group=$(id -gn "$user" 2>/dev/null || echo "$user")
# uncomment and set your key:
# Environment=OPENAI_API_KEY=sk-...
EOF
    mkdir -p /run/jancode
    chown "$user" /run/jancode 2>/dev/null || true
    chmod 755 /run/jancode
    mkdir -p "$runtime_home"
    systemctl daemon-reload
    log "Installed system service: $unit"
    log "Start it with: sudo systemctl enable --now $BIN_NAME"
  else
    unit="$HOME/.config/systemd/user/$BIN_NAME.service"
    mkdir -p "$(dirname "$unit")"
    cat > "$unit" << EOF
[Unit]
Description=jancode AI coding agent daemon (user)
After=graphical-session.target

[Service]
ExecStart=$exec_path serve
Environment=JANCODE_HOME=$runtime_home
Restart=on-failure
RestartSec=3
# uncomment and set your key:
# Environment=OPENAI_API_KEY=sk-...
EOF
    systemctl --user daemon-reload
    log "Installed user service: $unit"
    log "Start it with: systemctl --user enable --now $BIN_NAME"
  fi
}

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------
uninstall() {
  local dest="$PREFIX/bin/$BIN_NAME"
  if [ -f "$dest" ]; then
    if [ ! -w "$(dirname "$dest")" ]; then
      sudo rm -f "$dest"
    else
      rm -f "$dest"
    fi
    log "Removed binary: $dest"
  else
    warn "No binary at $dest"
  fi
  rm -f "$HOME/.config/systemd/user/$BIN_NAME.service"
  if [ "$(id -u)" -eq 0 ]; then
    rm -f "/etc/systemd/system/$BIN_NAME.service"
  fi
  log "Note: your config/state in \${JANCODE_HOME:-$HOME/.jancode} was kept."
}

# ---------------------------------------------------------------------------
main() {
  if [ "$UNINSTALL" -eq 1 ]; then
    uninstall
    exit 0
  fi
  detect_os
  check_rust
  install_binary
  setup_config
  if [ "$SERVICE_MODE" != "none" ]; then
    install_service
  else
    log "Done. Run: $PREFIX/bin/$BIN_NAME connect   (or 'run \"your prompt\"')"
  fi
}

main "$@"