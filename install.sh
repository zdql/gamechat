#!/usr/bin/env sh
# gamechat installer
# Downloads the latest prebuilt gamechat binary from GitHub releases
# and places it in $INSTALL_DIR (default: $HOME/.local/bin).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/zdql/gamechat/main/install.sh | sh
#
# Env overrides:
#   GAMECHAT_REPO       owner/repo to fetch from (default: zdql/gamechat)
#   GAMECHAT_VERSION    tag to install (default: latest)
#   INSTALL_DIR         where to place the binary (default: $HOME/.local/bin)
#   GAMECHAT_NO_PROMPT  if set, skip the interactive OPENAI_API_KEY prompt

set -eu

REPO="${GAMECHAT_REPO:-zdql/gamechat}"
VERSION="${GAMECHAT_VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/gamechat"
CONFIG_FILE="$CONFIG_DIR/env"

# ── platform detection ────────────────────────────────────────────────────
uname_os=$(uname -s)
uname_arch=$(uname -m)

case "$uname_os" in
  Darwin) os="darwin" ;;
  Linux)  os="linux" ;;
  *) echo "gamechat: unsupported OS: $uname_os" >&2; exit 1 ;;
esac

case "$uname_arch" in
  arm64|aarch64) arch="arm64" ;;
  x86_64|amd64)  arch="x86_64" ;;
  *) echo "gamechat: unsupported architecture: $uname_arch" >&2; exit 1 ;;
esac

target="${os}-${arch}"
asset="gamechat-${target}.tar.gz"

# ── pick a downloader ─────────────────────────────────────────────────────
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q "$1" -O "$2"; }
else
  echo "gamechat: need curl or wget to download" >&2
  exit 1
fi

# ── resolve version ───────────────────────────────────────────────────────
if [ "$VERSION" = "latest" ]; then
  url="https://github.com/${REPO}/releases/latest/download/${asset}"
else
  url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
fi

# ── download + install ────────────────────────────────────────────────────
tmp=$(mktemp -d 2>/dev/null || mktemp -d -t gamechat)
trap 'rm -rf "$tmp"' EXIT

echo "gamechat: downloading $url"
fetch "$url" "$tmp/$asset"

tar -xzf "$tmp/$asset" -C "$tmp"

bin_path=$(find "$tmp" -type f -name gamechat -perm -u+x | head -n 1)
if [ -z "$bin_path" ]; then
  echo "gamechat: extracted archive did not contain a gamechat binary" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
install -m 0755 "$bin_path" "$INSTALL_DIR/gamechat"

echo "gamechat: installed to $INSTALL_DIR/gamechat"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) on_path=1 ;;
  *) on_path=0 ;;
esac

if [ "$on_path" = "0" ]; then
  echo
  echo "gamechat: $INSTALL_DIR is not on your PATH."
  echo "  Add this to your shell profile:"
  echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
fi

# ── OPENAI_API_KEY setup ──────────────────────────────────────────────────
# gamechat needs OPENAI_API_KEY for the Realtime websocket. Offer to write
# it to $CONFIG_FILE (which gamechat loads automatically from any cwd).
#
# Skip when:
#   - GAMECHAT_NO_PROMPT is set,
#   - the key is already in the environment,
#   - a config file already exists,
#   - we have no controlling terminal (e.g. piped non-interactive install).

if [ -z "${GAMECHAT_NO_PROMPT:-}" ] \
   && [ -z "${OPENAI_API_KEY:-}" ] \
   && [ ! -f "$CONFIG_FILE" ] \
   && [ -r /dev/tty ] && [ -w /dev/tty ]
then
  printf '\n' >/dev/tty
  printf 'gamechat needs an OPENAI_API_KEY to drive the Realtime voice loop.\n' >/dev/tty
  printf 'Paste your key now (input is hidden), or press Enter to skip.\n' >/dev/tty
  printf '> ' >/dev/tty

  # Hide echo while reading; restore on any exit path.
  saved_stty=$(stty -g </dev/tty 2>/dev/null || echo "")
  if [ -n "$saved_stty" ]; then
    trap 'stty "$saved_stty" </dev/tty 2>/dev/null; rm -rf "$tmp"' EXIT INT TERM HUP
    stty -echo </dev/tty
  fi

  api_key=""
  IFS= read -r api_key </dev/tty || api_key=""

  if [ -n "$saved_stty" ]; then
    stty "$saved_stty" </dev/tty 2>/dev/null || true
    trap 'rm -rf "$tmp"' EXIT
  fi
  printf '\n' >/dev/tty

  # Trim leading/trailing whitespace (mostly to drop a trailing newline
  # when the key was bracketed-pasted).
  api_key=$(printf '%s' "$api_key" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')

  if [ -n "$api_key" ]; then
    mkdir -p "$CONFIG_DIR"
    # Write with mode 0600 — never world-readable.
    umask_old=$(umask)
    umask 077
    printf 'OPENAI_API_KEY=%s\n' "$api_key" > "$CONFIG_FILE"
    umask "$umask_old"
    chmod 600 "$CONFIG_FILE" 2>/dev/null || true
    echo "gamechat: wrote OPENAI_API_KEY to $CONFIG_FILE"
  else
    echo "gamechat: skipped key setup."
    echo "  set OPENAI_API_KEY in your shell, or run:"
    echo "    mkdir -p $CONFIG_DIR && echo 'OPENAI_API_KEY=sk-...' > $CONFIG_FILE"
  fi
elif [ -f "$CONFIG_FILE" ]; then
  echo "gamechat: $CONFIG_FILE already exists, leaving it as-is."
elif [ -n "${OPENAI_API_KEY:-}" ]; then
  echo "gamechat: OPENAI_API_KEY is already set in your environment."
fi

echo
echo "next:"
if [ "$on_path" = "1" ]; then
  echo "  gamechat --realtime"
else
  echo "  $INSTALL_DIR/gamechat --realtime"
fi
