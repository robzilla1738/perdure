#!/bin/sh
# Perdure installer: fetches the latest (or pinned) release binary for this
# machine, verifies its checksum, and installs it.
#
#   curl -fsSL https://raw.githubusercontent.com/robzilla1738/perdure/main/install.sh | sh
#
# Environment:
#   PERDURE_VERSION      pin a version tag (e.g. v0.2.0-alpha.1); default: latest
#   PERDURE_INSTALL_DIR  where the binary goes; default: ~/.local/bin
set -eu

REPO="robzilla1738/perdure"
INSTALL_DIR="${PERDURE_INSTALL_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin)
    case "$arch" in
      arm64) target="aarch64-apple-darwin" ;;
      x86_64) target="x86_64-apple-darwin" ;;
      *) echo "error: unsupported macOS architecture: $arch" >&2; exit 1 ;;
    esac ;;
  Linux)
    case "$arch" in
      x86_64) target="x86_64-unknown-linux-musl" ;;
      *) echo "error: unsupported Linux architecture: $arch (build from source: cargo install perdure)" >&2; exit 1 ;;
    esac ;;
  *)
    echo "error: unsupported OS: $os (on Windows, download the .zip from GitHub releases)" >&2
    exit 1 ;;
esac

if [ -n "${PERDURE_VERSION:-}" ]; then
  tag="$PERDURE_VERSION"
else
  tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)"
  [ -n "$tag" ] || { echo "error: could not determine the latest release tag" >&2; exit 1; }
fi

version="${tag#v}"
name="perdure-$version-$target"
url="https://github.com/$REPO/releases/download/$tag/$name.tar.gz"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "downloading $name.tar.gz ($tag)"
curl -fsSL -o "$tmp/$name.tar.gz" "$url"
curl -fsSL -o "$tmp/SHA256SUMS" \
  "https://github.com/$REPO/releases/download/$tag/SHA256SUMS"

cd "$tmp"
expected="$(awk -v f="$name.tar.gz" '$2 == f {print $1}' SHA256SUMS)"
[ -n "$expected" ] || { echo "error: $name.tar.gz not found in SHA256SUMS" >&2; exit 1; }
if command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$name.tar.gz" | awk '{print $1}')"
else
  actual="$(sha256sum "$name.tar.gz" | awk '{print $1}')"
fi
[ "$expected" = "$actual" ] || { echo "error: checksum mismatch" >&2; exit 1; }

tar xzf "$name.tar.gz"
mkdir -p "$INSTALL_DIR"
install -m 0755 "$name/perdure" "$INSTALL_DIR/perdure"

echo "installed perdure $version to $INSTALL_DIR/perdure"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "note: add $INSTALL_DIR to your PATH" ;;
esac
"$INSTALL_DIR/perdure" version
