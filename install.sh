#!/bin/sh
set -eu

REPO="${SAMISHISA_REPO:-heyngra/samishisa}"
VERSION="${SAMISHISA_VERSION:-latest}"
DIR="${SAMISHISA_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  MINGW* | MSYS* | CYGWIN*) die "on Windows, run in PowerShell: irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex" ;;
  *) die "unsupported OS: $(uname -s)" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x64 ;;
  aarch64 | arm64) arch=arm64 ;;
  *) die "unsupported CPU: $(uname -m) (builds exist for x64 and arm64)" ;;
esac
asset="samishisa-$os-$arch"
if [ "$os" = linux ] && { ls /lib/ld-musl-* >/dev/null 2>&1 || (ldd --version 2>&1 | grep -qi musl); }; then
  asset="$asset-musl"
fi

if [ -n "${SAMISHISA_BASE_URL:-}" ]; then
  base="${SAMISHISA_BASE_URL%/}"
elif [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --retry 2 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
else
  die "need curl or wget"
fi
if command -v sha256sum >/dev/null 2>&1; then
  sha() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "need sha256sum or shasum to verify the download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "downloading $asset ($VERSION) from $base ..."
fetch "$base/$asset" "$tmp/samishisa" || die "download failed: $base/$asset"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "download failed: $base/SHA256SUMS"
expected="$(grep " \*\{0,1\}$asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)"
[ -n "$expected" ] || die "$asset is not listed in SHA256SUMS"
actual="$(sha "$tmp/samishisa")"
[ "$expected" = "$actual" ] || die "checksum mismatch for $asset (expected $expected, got $actual)"

mkdir -p "$DIR"
chmod +x "$tmp/samishisa"
mv "$tmp/samishisa" "$DIR/samishisa"
[ "$os" = macos ] && xattr -d com.apple.quarantine "$DIR/samishisa" 2>/dev/null || true

say "installed $("$DIR/samishisa" version) to $DIR/samishisa"
case ":$PATH:" in
  *":$DIR:"*) ;;
  *) say ""
     say "$DIR is not on your PATH. Add it, e.g.:"
     say "  echo 'export PATH=\"$DIR:\$PATH\"' >> ~/.profile && . ~/.profile" ;;
esac
say ""
say "Next:"
say "  samishisa init my-api && cd my-api && samishisa    # then open http://localhost:3000/docs"
