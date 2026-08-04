# Ludusavi Sync

Per-game cloud save sync for your PC and Steam Deck, built on the
[ludusavi](https://github.com/mtkennerly/ludusavi) backup engine.

Ludusavi Sync keeps individual games in sync across devices through a shared
cloud folder (Google Drive via [rclone](https://rclone.org/)), instead of
mirroring your entire backup. Underneath, it keeps ludusavi's full backup and
restore engine intact, so it still understands 19,000+ games, Windows registry
saves, Steam screenshots, and Proton/Wine prefixes.

## Features

* **Per-game cloud sync** -- push or pull one game at a time to a shared cloud folder.
* **Additive-only transfers** -- sync uses `rclone copy`, so syncing one game can
  never delete another game's cloud data.
* **Cross-device Wine/Proton remap** -- Proton saves backed up on one machine resolve
  to the correct local prefix on another, even when the Linux username and
  compatdata app ID differ between devices.
* **Tauri desktop app** -- a lightweight React + TypeScript GUI in `desktop/` with a
  star-to-sync game list, search, library scan, per-game backup/push/pull controls
  with live progress, and cloud settings.
* **CLI** -- `ludusavi sync push|pull|status` for scripting and headless use
  (SSH into your Deck, cron jobs, etc.).
* **Full backup engine** -- file and registry saves, multi-store support
  (Steam, GOG, Epic, Heroic, Lutris, ...), custom games, backup validation,
  retention, and more.

## How it works

Each game's local backup lives under your backup root (configured in
`config.yaml`). `sync push` uploads a game's backup folder to your cloud folder;
`sync pull` downloads it back. Per-game metadata (last push, source device, and the
Wine prefix mapping) is recorded in `settings.config` at the backup root and merged
across devices, so every machine can restore a game into its own local prefix.

## Getting started

### Desktop app

1. Build and launch the app (see [Building](#building)).
2. Open **Settings** (gear icon) and connect Google Drive; set the cloud folder.
3. Star the games you want to keep in sync. Hit **Scan** to discover installed
   games, then use **Backup** / **Push** / **Pull** per game.

### CLI

```bash
# One-time setup
ludusavi manifest update
ludusavi cloud set google-drive        # or: cloud set custom --id <rclone remote>
# set the cloud folder in config.yaml under `cloud.path`

# Back up a game locally, then sync it
ludusavi backup "Baldur's Gate 3"
ludusavi sync push "Baldur's Gate 3"
ludusavi sync pull "Baldur's Gate 3"
ludusavi sync status "Baldur's Gate 3"
```

> `cloud upload` and `cloud download` mirror the whole backup directory with
> `rclone sync` and can delete remote data. Prefer per-game
> `sync push` / `sync pull`, which are additive and safe to mix across devices.

## Requirements

* [rclone](https://rclone.org/) -- required for any cloud operation.
* Latest stable Rust, and on Linux the system packages listed under [Building](#building).

## Building

### CLI binary

Prerequisites: latest stable Rust, `gcc`, and on Linux also `libxcb-composite0-dev libgtk-3-dev`.

```bash
cargo build --release           # produces target/release/ludusavi
cargo build --release --no-default-features   # library-only (no CLI deps)
```

### Tauri desktop app

Prerequisites: Rust, [pnpm](https://pnpm.io/), [rclone](https://rclone.org/),
and on Linux the same system packages as above plus `libwebkit2gtk-4.1-dev`.

```bash
cd desktop
pnpm install
pnpm run tauri build            # produces AppImage in src-tauri/target/release/bundle/appimage/
```

For development with hot-reload:

```bash
cd desktop
pnpm install
pnpm run tauri:dev              # uses scripts/dev.sh (WebKitGTK/Wayland workaround)
```

`scripts/dev.sh` sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` and falls back to
`GDK_BACKEND=x11` if the Wayland path crashes on your compositor/GPU.

### Steam Deck

Cross-compile the CLI:

```bash
cargo build --release --target x86_64-unknown-linux-gnu
```

The Tauri desktop app also works on Deck in desktop mode. Use `tauri:dev`
via the dev.sh script for the same Wayland workaround.

### Cross-compiling

For other targets, install the appropriate target and linker:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## Documentation

Detailed help for the underlying backup tool (configuration file, roots,
redirects, custom games, backup exclusions/retention, troubleshooting, etc.)
is under [docs/help/](./docs/help/), inherited from upstream
[ludusavi](https://github.com/mtkennerly/ludusavi).

## Development

Please refer to [CONTRIBUTING.md](./CONTRIBUTING.md).
