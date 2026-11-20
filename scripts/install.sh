#!/usr/bin/env bash
# TrackerTop One-Line macOS / Linux Installer
# Usage: curl -fsSL https://raw.githubusercontent.com/trgtefecngz/TrackerTop/main/scripts/install.sh | bash
#
# Picks the release asset for this OS + CPU, verifies its SHA256 checksum (published next to
# the binary as <asset>.sha256) and installs it to ~/.local/bin/trackertop.
set -euo pipefail

REPO="trgtefecngz/TrackerTop"
INSTALL_DIR="${HOME}/.local/bin"

OS="$(uname -s)"
ARCH="$(uname -m)"
case "${OS}/${ARCH}" in
  Darwin/arm64)          PATTERN="macos-arm64" ;;
  Darwin/x86_64)         PATTERN="macos-x64" ;;
  Linux/x86_64|Linux/amd64) PATTERN="linux-x64-headless" ;;
  *)
    echo "Error: no prebuilt TrackerTop binary for ${OS}/${ARCH}." >&2
    echo "Build from source instead: cargo install --git https://github.com/${REPO} --no-default-features" >&2
    exit 1
    ;;
esac

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

echo "Fetching latest TrackerTop release..."
RELEASE_JSON="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest")"
# Match the URL that ENDS with the pattern (the closing quote excludes the .sha256 asset)
URL="$(printf '%s' "$RELEASE_JSON" | grep -o "https://[^\"]*${PATTERN}\"" | head -n 1 | tr -d '"')"
if [ -z "$URL" ]; then
  echo "Error: could not find release asset matching ${PATTERN}" >&2
  exit 1
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT
curl -fsSL "$URL" -o "${TMP_DIR}/trackertop"

if printf '%s' "$RELEASE_JSON" | grep -q "\"${URL}.sha256\""; then
  EXPECTED="$(curl -fsSL "${URL}.sha256" | awk '{print tolower($1)}')"
  ACTUAL="$(sha256_of "${TMP_DIR}/trackertop" | tr '[:upper:]' '[:lower:]')"
  if [ "$EXPECTED" != "$ACTUAL" ]; then
    echo "Error: checksum mismatch (expected ${EXPECTED}, got ${ACTUAL}). Nothing was installed." >&2
    exit 1
  fi
  echo "SHA256 verified."
else
  echo "Warning: release has no checksum file; skipping SHA256 verification." >&2
fi

mkdir -p "$INSTALL_DIR"
chmod +x "${TMP_DIR}/trackertop"
mv -f "${TMP_DIR}/trackertop" "${INSTALL_DIR}/trackertop"
echo "Installed TrackerTop to ${INSTALL_DIR}/trackertop"
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *) echo "Note: add ${INSTALL_DIR} to your PATH to run 'trackertop' directly." ;;
esac
echo "Run 'trackertop install claude --global' to connect Claude Code."
