#!/usr/bin/env bash
# One-shot build: turns the Tauri desktop app into a single portable AppImage
# that runs on any Linux machine with FUSE (no install step for the user -
# just `chmod +x` and run).
#
# Usage: ./desktop/scripts/build-appimage.sh
#
# Prerequisites (checked below, not auto-installed): Rust, pnpm, and the
# GTK/WebKitGTK/xcb dev headers Tauri's Linux bundler needs.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."   # desktop/

echo "==> Checking prerequisites"

missing=()
command -v cargo >/dev/null 2>&1 || missing+=("Rust (https://rustup.rs)")
command -v pnpm >/dev/null 2>&1 || missing+=("pnpm (https://pnpm.io/installation)")

if command -v pkg-config >/dev/null 2>&1; then
    pkg-config --exists gtk+-3.0 || missing+=("GTK 3 dev headers")
    pkg-config --exists webkit2gtk-4.1 || missing+=("WebKitGTK 4.1 dev headers")
    pkg-config --exists xcb-composite || missing+=("xcb-composite dev headers")
else
    missing+=("pkg-config")
fi

if [ "${#missing[@]}" -ne 0 ]; then
    echo "Missing prerequisites:" >&2
    printf '  - %s\n' "${missing[@]}" >&2
    cat >&2 <<'EOF'

Install the system packages for your distro, then re-run this script:
  Debian/Ubuntu: sudo apt install build-essential libgtk-3-dev libwebkit2gtk-4.1-dev libxcb-composite0-dev
  Fedora:        sudo dnf install gcc gtk3-devel webkit2gtk4.1-devel libxcb-devel
  Arch/CachyOS:  sudo pacman -S base-devel gtk3 webkit2gtk-4.1 libxcb
EOF
    exit 1
fi

echo "==> Installing frontend dependencies (pnpm install)"
pnpm install

echo "==> Building release AppImage (compiles the Rust backend - can take several minutes)"
# NO_STRIP=1: tauri's bundled linuxdeploy ships an old `strip` that chokes on the
# `.relr.dyn` ELF section newer binutils/glibc emit ("unknown type [0x13]") - e.g. on
# current Arch/CachyOS. Skipping strip just means a larger, unstripped AppImage, so
# it's a safe default everywhere, not just a workaround for that failure.
#
# APPIMAGE_EXTRACT_AND_RUN=1: linuxdeploy (and the appimagetool it calls) ship as
# AppImages themselves, which normally mount via FUSE to run - unavailable in most
# Docker containers (no /dev/fuse), which fails as an opaque "failed to run
# linuxdeploy". This makes them extract-and-run instead of mounting; harmless and a
# bit slower on a machine that does have FUSE, so on by default rather than only in CI.
NO_STRIP=1 APPIMAGE_EXTRACT_AND_RUN=1 pnpm tauri build --verbose

bundle_dir="src-tauri/target/release/bundle/appimage"
appimage=$(find "$bundle_dir" -maxdepth 1 -name '*.AppImage' -print -quit 2>/dev/null || true)

if [ -z "$appimage" ]; then
    echo "Build finished, but no .AppImage was found under $bundle_dir" >&2
    exit 1
fi

dest="$(cd .. && pwd)/Ludusavi-Sync.AppImage"
cp "$appimage" "$dest"
chmod +x "$dest"

echo
echo "Done: $dest"
echo "Run it on any Linux machine with:"
echo "  ./$(basename "$dest")"
