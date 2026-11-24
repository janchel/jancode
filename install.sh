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
# Minimum supported toolchain. Keep in sync with `rust-version` in Cargo.toml.
REQUIRED_RUST_MAJOR=1
REQUIRED_RUST_MINOR=88

rust_version_ok() {
  command -v cargo >/dev/null 2>&1 || return 1
  local v major minor
  v="$(cargo --version 2>/dev/null | awk '{print $2}' || true)"
  [ -n "$v" ] || return 1
  major="${v%%.*}"
  minor="$(printf '%s' "$v" | cut -d. -f2)"
  case "$major" in ''|*[!0-9]*) return 1 ;; esac
  case "$minor" in ''|*[!0-9]*) minor=0 ;; esac
  if [ "$major" -gt "$REQUIRED_RUST_MAJOR" ]; then return 0; fi
  if [ "$major" -eq "$REQUIRED_RUST_MAJOR" ] && [ "$minor" -ge "$REQUIRED_RUST_MINOR" ]; then
    return 0
  fi
  return 1
}

require_rust_version() {
  rust_version_ok && return 0
  local found=""
  if command -v cargo >/dev/null 2>&1; then
    found="$(cargo --version 2>/dev/null | awk '{print $2}' || true)"
  fi
  die "jancode requires Rust >= ${REQUIRED_RUST_MAJOR}.${REQUIRED_RUST_MINOR} (found: ${found:-none}).

Distro packages are usually too old. Install a current toolchain with rustup:
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  source \"\$HOME/.cargo/env\"    # or start a new shell
then re-run this script. To install an already-built binary instead:
  ./install.sh --skip-build"
}

# Install a current toolchain via the official rustup script. Supports curl or
# wget. Returns non-zero on failure.
install_rustup() {
  local script=""
  if command -v curl >/dev/null 2>&1; then
    script="$(curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs)"
  elif command -v wget >/dev/null 2>&1; then
    script="$(wget -qO- https://sh.rustup.rs)"
  else
    return 1
  fi
  [ -n "$script" ] || return 1
  printf '%s\n' "$script" | sh -s -- -y --profile minimal --default-toolchain stable
}

check_rust() {
  # rustup installs to ~/.cargo/bin which is often not on a non-login PATH.
  for cand in "$HOME/.cargo/bin" "/usr/local/cargo/bin" "/usr/bin" "/opt/cargo/bin"; do
    if [ -x "$cand/cargo" ]; then
      export PATH="$cand:$PATH"
      break
    fi
  done

  if command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1 && rust_version_ok; then
    log "Rust toolchain found (cargo $(cargo --version | awk '{print $2}'))"
    return 0
  fi

  local found=""
  if command -v cargo >/dev/null 2>&1; then
    found="$(cargo --version 2>/dev/null | awk '{print $2}' || true)"
  fi

  # Root: bootstrap a current toolchain with rustup (distro packages are too
  # old for the dependency set).
  if [ "$(id -u)" -eq 0 ] && { command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1; }; then
    log "Rust >= ${REQUIRED_RUST_MAJOR}.${REQUIRED_RUST_MINOR} not found (found: ${found:-none}); installing rustup stable..."
    if install_rustup; then
      export PATH="$HOME/.cargo/bin:$PATH"
      for cand in "$HOME/.cargo/bin" "/usr/local/cargo/bin"; do
        [ -x "$cand/cargo" ] && export PATH="$cand:$PATH"
      done
      if command -v cargo >/dev/null 2>&1 && rust_version_ok; then
        log "Rust toolchain installed (cargo $(cargo --version | awk '{print $2}'))"
        return 0
      fi
    fi
    die "rustup install did not provide a usable cargo. Install Rust manually and re-run."
  fi

  # Non-root fallback: distro packages (only possible as root, handled above).
  aid=$(command -v apt-get || true)
  dnf_=$(command -v dnf || true)
  if [ -n "$aid" ] && [ "$(id -u)" -eq 0 ]; then
    log "Installing distro cargo/rustc via apt-get (may be too old)..."
    apt-get update -y && apt-get install -y cargo rustc
  elif [ -n "$dnf_" ] && [ "$(id -u)" -eq 0 ]; then
    log "Installing distro cargo/rust via dnf (may be too old)..."
    dnf install -y cargo rust
  fi
  command -v cargo >/dev/null 2>&1 || die "cargo still not available after install."
  require_rust_version
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
  local home="${JANCODE_HOME:-${HOME:-/root}/.jancode}"
  local here
  here="$(cd "$(dirname "$0")" && pwd)"
  mkdir -p "$home"
  if [ ! -f "$home/config.toml" ]; then
    # The official template lives in the repo so it's versioned and reviewable;
    # install.sh only copies it (never overwrites an existing config).
    if [ -f "$here/config.example.toml" ]; then
      cp "$here/config.example.toml" "$home/config.toml"
    else
      die "config template not found: $here/config.example.toml (run install.sh from the repository)"
    fi
    log "Created config: $home/config.toml (from config.example.toml)"
    log " -> set your API key in \$OPENAI_API_KEY or edit api_key in that file."
  else
    warn "config.toml already exists in $home, leaving it untouched."
  fi
  chmod 700 "$home" 2>/dev/null || true
  # config.toml may hold inline provider keys / MCP bearer tokens.
  chmod 600 "$home/config.toml" 2>/dev/null || true
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
  runtime_home="${JANCODE_HOME:-${HOME:-/root}/.jancode}"

  if [ "$SERVICE_MODE" = "system" ]; then
    unit="/etc/systemd/system/$BIN_NAME.service"
    # Run as the invoking user when installed via sudo, else root.
    if [ "$(id -u)" -eq 0 ]; then
      user="${SUDO_USER:-${USER:-root}}"
    else
      user="${USER:-$(id -un)}"
    fi
    # Secrets for the daemon (provider keys, MCP bearer tokens). systemd
    # services do NOT inherit your shell env, so anything the daemon reads via
    # env (e.g. OPENAI_API_KEY, or an MCP `bearer_env`) must live here.
    env_file="$runtime_home/service.env"
    if [ ! -f "$env_file" ]; then
        mkdir -p "$runtime_home"
        cat > "$env_file" << 'EOF'
# Environment variables for the jancode daemon (read by systemd).
# systemd services don't inherit your shell, so set secrets here.
# One KEY=VALUE per line, no quotes, no `export`.
#
# Examples:
# OPENAI_API_KEY=sk-...
# GROQ_API_KEY=gsk-...
# OPS_MCP_KEY=your-mcp-bearer-token
EOF
        chmod 600 "$env_file"
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
# Secrets (provider keys, MCP bearer_env tokens). The leading '-' in the
# EnvironmentFile line means the unit still starts if the file is absent.
# Edit: $env_file
EnvironmentFile=-$env_file
# systemd creates /run/jancode with the correct ownership on every start
# (survives reboots, unlike a manual mkdir in /run which is tmpfs).
RuntimeDirectory=jancode
RuntimeDirectoryMode=0755
Restart=on-failure
RestartSec=3
User=$user
Group=$(id -gn "$user" 2>/dev/null || echo "$user")
# You can also set one-off keys inline here:
# Environment=OPENAI_API_KEY=sk-...

[Install]
WantedBy=multi-user.target
EOF
    mkdir -p "$runtime_home"
    if command -v systemctl >/dev/null 2>&1; then
      systemctl daemon-reload || warn "systemctl daemon-reload failed; run it manually"
      if systemctl enable --now "$BIN_NAME"; then
        log "Service enabled and started."
      else
        warn "could not enable/start the service; run: sudo systemctl enable --now $BIN_NAME"
      fi
      echo
      systemctl status "$BIN_NAME" --no-pager || true
    else
      warn "systemctl not found; unit written to $unit but not loaded"
    fi
    log "Installed system service: $unit"
    log "Daemon secrets file: $env_file (chmod 600) — add MCP bearer keys here"
    log "Manage it with: sudo systemctl {start|stop|restart|status} $BIN_NAME"
  else
    unit="$HOME/.config/systemd/user/$BIN_NAME.service"
    mkdir -p "$(dirname "$unit")"
    env_file="$runtime_home/service.env"
    if [ ! -f "$env_file" ]; then
        mkdir -p "$runtime_home"
        cat > "$env_file" << 'EOF'
# Environment variables for the jancode daemon (read by systemd --user).
# One KEY=VALUE per line, no quotes, no `export`.
# OPENAI_API_KEY=sk-...
# OPS_MCP_KEY=your-mcp-bearer-token
EOF
        chmod 600 "$env_file"
    fi
    cat > "$unit" << EOF
[Unit]
Description=jancode AI coding agent daemon (user)
After=graphical-session.target

[Service]
ExecStart=$exec_path serve
Environment=JANCODE_HOME=$runtime_home
EnvironmentFile=-$env_file
Restart=on-failure
RestartSec=3
# You can also set one-off keys inline here:
# Environment=OPENAI_API_KEY=sk-...

[Install]
WantedBy=default.target
EOF
    if command -v systemctl >/dev/null 2>&1; then
      systemctl --user daemon-reload || warn "systemctl --user daemon-reload failed; run it manually"
      if systemctl --user enable --now "$BIN_NAME"; then
        log "User service enabled and started."
      else
        warn "could not enable/start the user service; run: systemctl --user enable --now $BIN_NAME"
      fi
      echo
      systemctl --user status "$BIN_NAME" --no-pager || true
    else
      warn "systemctl not found; unit written to $unit but not loaded"
    fi
    log "Installed user service: $unit"
    log "Daemon secrets file: $env_file (chmod 600) — add MCP bearer keys here"
    log "Manage it with: systemctl --user {start|stop|restart|status} $BIN_NAME"
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