#!/usr/bin/env sh
# gamechat installer
# Downloads the latest prebuilt gamechat binary from GitHub releases
# and places it in $INSTALL_DIR (default: $HOME/.local/bin).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/<owner>/gamechat/main/install.sh | sh
#
# Env overrides:
#   GAMECHAT_REPO       owner/repo to fetch from (default: gamechat/gamechat)
#   GAMECHAT_VERSION    tag to install (default: latest)
#   INSTALL_DIR         where to place the binary (default: $HOME/.local/bin)

set -eu

REPO="${GAMECHAT_REPO:-zdql/gamechat}"
VERSION="${GAMECHAT_VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

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

mkdir -p "$INSTALL_DIR"
install -m 0755 "$tmp/gamechat" "$INSTALL_DIR/gamechat"

echo "gamechat: installed to $INSTALL_DIR/gamechat"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo
    echo "gamechat: $INSTALL_DIR is not on your PATH."
    echo "  Add this to your shell profile:"
    echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac

echo
echo "next:"
echo "  gamechat --help"
