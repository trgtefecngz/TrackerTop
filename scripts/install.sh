#!/usr/bin/env bash
# TrackerTop One-Line macOS / Linux Installer
# Usage: curl -fsSL https://raw.githubusercontent.com/trgtefecngz/TrackerTop/main/scripts/install.sh | bash
set -euo pipefail

REPO="trgtefecngz/TrackerTop"
INSTALL_DIR="${HOME}/.local/bin"
mkdir -p "$INSTALL_DIR"

OS="$(uname -s)"
if [ "$OS" = "Darwin" ]; then
  PATTERN="macos-arm64"
else
  PATTERN="linux-x64-headless"
fi

echo "Fetching latest TrackerTop release..."
URL="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep -o "https://[^\"]*${PATTERN}" | head -n 1)"
if [ -z "$URL" ]; then
  echo "Error: Could not find release asset matching ${PATTERN}" >&2
  exit 1
fi

curl -fsSL "$URL" -o "${INSTALL_DIR}/trackertop"
chmod +x "${INSTALL_DIR}/trackertop"
echo "Installed TrackerTop to ${INSTALL_DIR}/trackertop"
echo "Run 'trackertop install claude --global' to connect Claude Code."
