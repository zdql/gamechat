#!/usr/bin/env sh
# gamechat installer
# Downloads the latest prebuilt gamechat binary from GitHub releases
# and places it in $INSTALL_DIR (default: $HOME/.local/bin).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/zdql/gamechat/main/install.sh | sh
#
# Env overrides:
#   GAMECHAT_REPO           owner/repo to fetch from (default: zdql/gamechat)
#   GAMECHAT_VERSION        tag to install (default: latest)
#   INSTALL_DIR             where to place the binary (default: $HOME/.local/bin)
#   GAMECHAT_NO_PROMPT      if set, skip the interactive OPENAI_API_KEY prompt
#   GAMECHAT_SKIP_CHECKSUM  if set, skip SHA-256 verification (not recommended)

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

# ── pick a SHA-256 checker ────────────────────────────────────────────────
# shasum is on macOS by default; sha256sum is on Linux. Either works.
if command -v shasum >/dev/null 2>&1; then
  sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }
elif command -v sha256sum >/dev/null 2>&1; then
  sha256_of() { sha256sum "$1" | awk '{print $1}'; }
else
  sha256_of() { echo ""; }
fi

# ── resolve version ───────────────────────────────────────────────────────
if [ "$VERSION" = "latest" ]; then
  base_url="https://github.com/${REPO}/releases/latest/download"
else
  base_url="https://github.com/${REPO}/releases/download/${VERSION}"
fi
url="$base_url/$asset"
sha_url="$url.sha256"

# ── download + verify + install ───────────────────────────────────────────
tmp=$(mktemp -d 2>/dev/null || mktemp -d -t gamechat)
trap 'rm -rf "$tmp"' EXIT

echo "gamechat: downloading $url"
fetch "$url" "$tmp/$asset"

if [ -n "${GAMECHAT_SKIP_CHECKSUM:-}" ]; then
  echo "gamechat: GAMECHAT_SKIP_CHECKSUM set, skipping checksum verification" >&2
else
  echo "gamechat: verifying checksum"
  if ! fetch "$sha_url" "$tmp/$asset.sha256" 2>/dev/null; then
    echo "gamechat: could not download checksum from $sha_url" >&2
    echo "  re-run with GAMECHAT_SKIP_CHECKSUM=1 to bypass (not recommended)" >&2
    exit 1
  fi

  expected=$(awk '{print $1}' "$tmp/$asset.sha256")
  if [ -z "$expected" ]; then
    echo "gamechat: empty checksum file at $sha_url" >&2
    exit 1
  fi

  actual=$(sha256_of "$tmp/$asset")
  if [ -z "$actual" ]; then
    echo "gamechat: need shasum or sha256sum to verify the download" >&2
    echo "  install one, or re-run with GAMECHAT_SKIP_CHECKSUM=1 to bypass" >&2
    exit 1
  fi

  if [ "$expected" != "$actual" ]; then
    echo "gamechat: checksum mismatch for $asset" >&2
    echo "  expected: $expected" >&2
    echo "  actual:   $actual" >&2
    exit 1
  fi
fi

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
#     In that case we print a loud warning so the user knows the key is
#     still missing and how to set it post-install.

have_tty=0
if [ -r /dev/tty ] && [ -w /dev/tty ]; then
  have_tty=1
fi

needs_key=1
if [ -n "${OPENAI_API_KEY:-}" ] || [ -f "$CONFIG_FILE" ]; then
  needs_key=0
fi

if [ "$needs_key" = "1" ] && [ -z "${GAMECHAT_NO_PROMPT:-}" ] && [ "$have_tty" = "1" ]; then
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
    # Defensive: reject keys with embedded newlines or NULs. The Rust
    # parser is line-oriented so a multi-line value would silently break.
    case "$api_key" in
      *"$(printf '\n')"*)
        echo "gamechat: refusing to write key containing newlines" >&2
        exit 1
        ;;
    esac

    mkdir -p "$CONFIG_DIR"
    # POSIX shell-safe single-quote escaping: replace every `'` with
    # `'\''`. The resulting `'<escaped>'` is safe both for `source` from
    # any POSIX shell and for the gamechat env parser, which strips the
    # outer single quotes and decodes the `'\''` sequence.
    escaped=$(printf '%s' "$api_key" | sed "s/'/'\\\\''/g")

    umask_old=$(umask)
    umask 077
    printf "OPENAI_API_KEY='%s'\n" "$escaped" > "$CONFIG_FILE"
    umask "$umask_old"
    chmod 600 "$CONFIG_FILE" 2>/dev/null || true
    echo "gamechat: wrote OPENAI_API_KEY to $CONFIG_FILE"
  else
    echo "gamechat: skipped key setup."
    echo "  set OPENAI_API_KEY in your shell, or run:"
    echo "    mkdir -p $CONFIG_DIR && printf \"OPENAI_API_KEY='sk-...'\\n\" > $CONFIG_FILE"
  fi
elif [ "$needs_key" = "1" ] && [ -n "${GAMECHAT_NO_PROMPT:-}" ]; then
  echo
  echo "gamechat: GAMECHAT_NO_PROMPT set — skipping OPENAI_API_KEY setup."
  echo "  --realtime will fail until you set OPENAI_API_KEY. To configure later:"
  echo "    mkdir -p $CONFIG_DIR && printf \"OPENAI_API_KEY='sk-...'\\n\" > $CONFIG_FILE"
elif [ "$needs_key" = "1" ] && [ "$have_tty" = "0" ]; then
  echo
  echo "gamechat: WARNING — no controlling terminal, cannot prompt for OPENAI_API_KEY." >&2
  echo "  This happens with noninteractive installs (e.g. some CI shells)." >&2
  echo "  --realtime will fail until you set the key. To configure now:" >&2
  echo "    export OPENAI_API_KEY=sk-..." >&2
  echo "  Or persist it:" >&2
  echo "    mkdir -p $CONFIG_DIR && printf \"OPENAI_API_KEY='sk-...'\\n\" > $CONFIG_FILE" >&2
elif [ -f "$CONFIG_FILE" ]; then
  echo "gamechat: $CONFIG_FILE already exists, leaving it as-is."
elif [ -n "${OPENAI_API_KEY:-}" ]; then
  echo "gamechat: OPENAI_API_KEY is already set in your environment."
fi

# ── coding-agent prerequisites ────────────────────────────────────────────
# gamechat shells out to `claude` (default) or `codex` (--provider codex).
# Both are user-installed CLIs — warn (don't fail) if neither is on PATH.

have_claude=0
have_codex=0
command -v claude >/dev/null 2>&1 && have_claude=1
command -v codex  >/dev/null 2>&1 && have_codex=1

if [ "$have_claude" = "0" ] && [ "$have_codex" = "0" ]; then
  echo
  echo "gamechat: WARNING — neither 'claude' nor 'codex' was found on PATH." >&2
  echo "  Install at least one coding agent before running --realtime:" >&2
  echo "    Claude Code: https://github.com/anthropics/claude-code" >&2
  echo "    Codex CLI:   https://github.com/openai/codex" >&2
elif [ "$have_claude" = "0" ]; then
  echo
  echo "gamechat: note — 'claude' not on PATH; default backend will fail."
  echo "  Install Claude Code, or run with: gamechat --realtime --provider codex"
elif [ "$have_codex" = "0" ]; then
  echo
  echo "gamechat: note — 'codex' not on PATH; --provider codex will fail."
fi

echo
echo "next:"
if [ "$on_path" = "1" ]; then
  echo "  gamechat --realtime"
else
  echo "  $INSTALL_DIR/gamechat --realtime"
fi
