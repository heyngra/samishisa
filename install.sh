#!/bin/sh
# Installs samishisa on Linux, macOS and WSL:
#
#   curl -fsSL https://raw.githubusercontent.com/heyngra/samishisa/main/install.sh | sh
#
# Environment variables:
#   SAMISHISA_VERSION=v0.1.0              release to install (default: latest)
#   SAMISHISA_INSTALL_DIR=/path           install folder (default: ~/.local/bin)
#   SAMISHISA_REPO=owner/repo             install from a fork
#   SAMISHISA_BASE_URL=https://...        download from a mirror (a folder with the release files)
#   SAMISHISA_URL=https://.../samishisa   direct link to the binary (CDN, file server)
#   SAMISHISA_SHA256=<hex>                checksum the download must match
#   SAMISHISA_FILE=/path/samishisa        install a binary you already have, no network
#   SAMISHISA_EXTENSION=https://.../mio.wasm   also add this extension (any source `samishisa add` accepts)
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

verify=""
if [ -n "${SAMISHISA_FILE:-}" ]; then
  say "using $SAMISHISA_FILE ..."
  cp "$SAMISHISA_FILE" "$tmp/samishisa"
  verify="${SAMISHISA_SHA256:-}"
elif [ -n "${SAMISHISA_URL:-}" ]; then
  say "downloading $SAMISHISA_URL ..."
  fetch "$SAMISHISA_URL" "$tmp/samishisa" || die "download failed: $SAMISHISA_URL"
  verify="${SAMISHISA_SHA256:-}"
  [ -n "$verify" ] || say "note: no SAMISHISA_SHA256 given, so the download is not verified"
else
  say "downloading $asset ($VERSION) from $base ..."
  fetch "$base/$asset" "$tmp/samishisa" || die "download failed: $base/$asset"
  fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "download failed: $base/SHA256SUMS"
  verify="$(grep " \*\{0,1\}$asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)"
  [ -n "$verify" ] || die "$asset is not listed in SHA256SUMS"
fi
if [ -n "$verify" ]; then
  verify="$(printf '%s' "$verify" | tr 'A-F' 'a-f')"
  actual="$(sha "$tmp/samishisa")"
  [ "$verify" = "$actual" ] || die "checksum mismatch (expected $verify, got $actual)"
fi

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
if [ -n "${SAMISHISA_EXTENSION:-}" ]; then
  if "$DIR/samishisa" list | grep -qF -- " $SAMISHISA_EXTENSION"; then
    say "$SAMISHISA_EXTENSION is already in your config"
  else
    say "adding $SAMISHISA_EXTENSION ..."
    "$DIR/samishisa" add "$SAMISHISA_EXTENSION" || die "samishisa add failed"
  fi
fi
say "          MIGGIKKGEGGGGGGGGGGGKQ           
         KGGGGGGGGGGGGGGIGGGIMOQOY         
   YWY  WGEGGGIGGGGGGIGGGGGGGGKMQQW        
   WY Y  OEEGGGGGIIMQSSUUQQKIGGQQQQ        
   W   W  WIEGIKMQUUWUUUUUUUQOSSSQOW       
   W   W     OMOUUUUUWUUUOMMUQQOSQS        
   Y   Y   WSQSUUUUUWWWWUSOMOWSUUW   YYY   
   Y   YY  WWUSUUUWUSWWWWWWWWWUSUUYY     Y 
   Y   YYWOMSUSUSWYWSSSUWUWYUWWSUSSUY    W 
   Y    WOMMSUUSU    WYYY   WUWQQUYUUW YY  
  W     SOMMSUUQOUWSY   WUWSOSWSOQY UU     
  Y     YOMMOUUUW          YUWUWSUUUUY     
 WYY    YSUUUUSSUY  WYYYY YQUUU            
Y         YWWUQOQOW YYYY  WWUWW            
                SOS SWYWW WWW Y  S         
Y    Y           YUUQWWYW SUUU  UOY        
               YYUUYWW      WUSSW          
                 YW  Y Y      GIQ          
    Y            YU YUSSWUY  MGGI          
Y              YSIO UOQSMKW QGGGGO         
                YMS MIUWKIW SGGGGGS        
UY   Y   YYYWWYWMGY UQYYWMS  YOGGEEQ       
             WWSGG  SSYY UQ   WEGGEEU      "
say "Samisha has been installed! Next steps:"
say "  samishisa add <source>      # an extension: github:owner/repo, or the URL of a .wasm"
say "  samishisa config            # edit your config, manage extensions etc."
say "  samishisa add builtin:docs  # optional: interactive API docs at /docs"
say "  samishisa run               # run it on http://localhost:[port] (default 3000)"
