#!/bin/sh
set -e

REPO="ArtemisMucaj/codesearch"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"

# Detect OS
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
case "$OS" in
  darwin) OS="macos" ;;
  mingw*|msys*|cygwin*) OS="windows" ;;
esac

# Detect architecture
ARCH=$(uname -m)
case "$ARCH" in
  x86_64|amd64) ARCH="x86_64" ;;
  arm64|aarch64) ARCH="aarch64" ;;
  *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

# Get latest version
VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name"' | sed -E 's/.*"([^"]+)".*/\1/')
if [ -z "$VERSION" ]; then
  echo "Failed to get latest version"
  exit 1
fi

echo "Installing codesearch $VERSION for $OS/$ARCH..."

# Determine asset name
EXT=""
if [ "$OS" = "windows" ]; then
  EXT=".exe"
fi

ASSET_NAME="codesearch-${OS}-${ARCH}${EXT}"
URL="https://github.com/$REPO/releases/download/$VERSION/$ASSET_NAME"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

echo "Downloading $URL..."
if ! curl -fsSL -o "$TMPDIR/codesearch${EXT}" "$URL"; then
  echo "Download failed. Check that a release exists for your platform ($OS/$ARCH)."
  exit 1
fi

chmod +x "$TMPDIR/codesearch${EXT}"

# Install
if [ -w "$INSTALL_DIR" ]; then
  mv "$TMPDIR/codesearch${EXT}" "$INSTALL_DIR/"
else
  echo "Installing to $INSTALL_DIR (requires sudo)..."
  sudo mv "$TMPDIR/codesearch${EXT}" "$INSTALL_DIR/"
fi

echo "codesearch $VERSION installed successfully to $INSTALL_DIR/codesearch${EXT}"

# Install optional SCIP indexers
echo ""
echo "Installing optional SCIP indexers..."

# scip-typescript (JavaScript / TypeScript support)
if command -v npm >/dev/null 2>&1; then
  echo "Installing scip-typescript via npm..."
  npm install -g @sourcegraph/scip-typescript && echo "  scip-typescript installed." || echo "  Warning: scip-typescript installation failed (JS/TS indexing will be unavailable)."
else
  echo "  Skipping scip-typescript (npm not found). Install Node.js + npm to enable JS/TS support."
fi

# scip-php (PHP support)
if command -v go >/dev/null 2>&1; then
  echo "Installing scip-php via go install..."
  go install github.com/davidrjenni/scip-php/cmd/scip-php@latest && echo "  scip-php installed." || echo "  Warning: scip-php installation failed (PHP indexing will be unavailable)."
else
  echo "  Skipping scip-php (go not found). Install Go to enable PHP support."
  echo "  See: https://github.com/davidrjenni/scip-php"
fi

echo ""
echo "Get started:"
echo "  codesearch index /path/to/your/project"
echo "  codesearch search \"your semantic query\""
