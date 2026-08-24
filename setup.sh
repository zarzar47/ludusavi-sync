#!/usr/bin/env bash
# Runtime setup for Ludusavi Sync: installs the one thing the app doesn't
# bundle - rclone (README "Requirements": "required for any cloud
# operation") - and checks for the one thing the AppImage needs from the
# host to even launch (FUSE).
#
# Deliberately NOT a system-package-manager script (apt/dnf/pacman/...): this
# app targets Steam Deck among other machines, where the root filesystem is
# read-only and there's no passwordless sudo, so "sudo apt/pacman install"
# is a bad default. Instead this installs rclone's official prebuilt binary
# straight from rclone.org into ~/.local/bin - no root, no distro-specific
# package name to get right, works the same on Steam Deck, Ubuntu, Fedora,
# Arch, whatever. If rclone is already on PATH, this is a no-op unless you
# pass --force.
#
# Usage: ./setup.sh [--force]

set -euo pipefail

FORCE=0
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        -h|--help)
            echo "Usage: $0 [--force]"
            echo "  --force   reinstall rclone into ~/.local/bin even if already on PATH"
            exit 0
            ;;
        *)
            echo "Unknown argument: $arg" >&2
            exit 1
            ;;
    esac
done

echo "==> Checking rclone"

if command -v rclone >/dev/null 2>&1 && [ "$FORCE" -eq 0 ]; then
    echo "    Found: $(command -v rclone) ($(rclone version | head -n1))"
    echo "    Skipping install (pass --force to reinstall into ~/.local/bin)."
else
    case "$(uname -s)" in
        Linux) os=linux ;;
        Darwin) os=osx ;;
        *)
            echo "Unsupported OS: $(uname -s). Install rclone manually: https://rclone.org/downloads/" >&2
            exit 1
            ;;
    esac

    case "$(uname -m)" in
        x86_64|amd64) arch=amd64 ;;
        aarch64|arm64) arch=arm64 ;;
        armv7l) arch=arm-v7 ;;
        *)
            echo "Unsupported architecture: $(uname -m). Install rclone manually: https://rclone.org/downloads/" >&2
            exit 1
            ;;
    esac

    bin_dir="$HOME/.local/bin"
    mkdir -p "$bin_dir"

    tmp_dir=$(mktemp -d)
    trap 'rm -rf "$tmp_dir"' EXIT

    zip_name="rclone-current-${os}-${arch}.zip"
    url="https://downloads.rclone.org/${zip_name}"
    echo "    Downloading $url"
    curl --fail --location --progress-bar -o "$tmp_dir/rclone.zip" "$url"

    echo "    Extracting"
    unzip -q -o "$tmp_dir/rclone.zip" -d "$tmp_dir"
    extracted_dir=$(find "$tmp_dir" -maxdepth 1 -type d -name 'rclone-*' -print -quit)
    install -m 755 "$extracted_dir/rclone" "$bin_dir/rclone"

    echo "    Installed: $bin_dir/rclone ($("$bin_dir/rclone" version | head -n1))"

    if ! command -v rclone >/dev/null 2>&1; then
        echo
        echo "    NOTE: $bin_dir isn't on your PATH in this shell."
        echo "    Add it (e.g. in ~/.bashrc): export PATH=\"\$HOME/.local/bin:\$PATH\""
        echo "    Or skip PATH entirely: the desktop app's Settings screen has an"
        echo "    rclone path field you can point straight at $bin_dir/rclone."
    fi
fi

echo
echo "==> Checking FUSE (needed to run the .AppImage)"

if command -v fusermount3 >/dev/null 2>&1 || command -v fusermount >/dev/null 2>&1; then
    echo "    Found: $(command -v fusermount3 2>/dev/null || command -v fusermount)"
else
    echo "    Not found. Install it via your distro's package manager, e.g.:"
    echo "      Debian/Ubuntu: sudo apt install fuse3"
    echo "      Fedora:        sudo dnf install fuse3"
    echo "      Arch/CachyOS:  sudo pacman -S fuse3"
    echo "    (Steam Deck ships this by default; if you're on one and see this,"
    echo "    something unusual is going on.)"
    echo "    Alternative if you can't install packages: extract and run directly -"
    echo "      ./Ludusavi-Sync.AppImage --appimage-extract"
    echo "      ./squashfs-root/AppRun"
fi

echo
echo "==> Done"
echo "Next: run the app and authorize a cloud remote from Settings (gear icon),"
echo "or from the CLI: ludusavi cloud set google-drive"
