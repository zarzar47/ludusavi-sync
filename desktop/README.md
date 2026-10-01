# Ludusavi Sync -- Desktop App

Tauri desktop frontend for [Ludusavi Sync](../README.md)'s per-game cloud sync.
React + TypeScript frontend, Rust backend -- no subprocess hop, the Tauri
commands call straight into `api.rs::Ludusavi` in-process.

## Architecture

```
desktop/
  src/                    React frontend
    App.tsx               Sync screen + page switch (useState, no router)
    CloudSettings.tsx     Cloud config: connect/disconnect, path, auto-sync
    GameSettingsModal.tsx Per-game modal: cover art, save-file checklist, task log
    tasks.ts              Task pipeline types/helpers (mirrors src-tauri)
    App.css               Styles (dark mode via prefers-color-scheme)
  src-tauri/              Rust backend
    src/lib.rs            Tauri commands wrapping api.rs::Ludusavi, task runner, log bridge
    Cargo.toml            Depends on root crate (default-features = false)
    tauri.conf.json       Window title/size, bundle config
  test/                   Node test-runner tests (no framework dependency)
  scripts/
    dev.sh                WebKitGTK/Wayland workaround (see below)
```

The root crate (`ludusavi = { path = "../..", default-features = false }`) is
used as a path dependency -- the `app` feature (clap/rfd/dialoguer) is excluded
so the GUI backend has no CLI-only deps in its tree. `Ludusavi::load()` runs
once at startup into managed state; if `manifest.yaml` is missing (fresh
checkout), the window still opens and commands return a clear error instead of
panicking.

## Screens

### Sync screen (`App.tsx`)

Unified game list -- no separate "enabled" vs "search" sections.

- **Search box**: debounced `search_games` (capped at 50 results, only fires on
  non-empty input -- never dumps the ~19k-title manifest by default).
- **Scan button**: `scan_games`, a full-library `Finality::Preview` backup.
  Same cost as upstream's startup scan -- finds installed games on this machine.
- **Star toggle**: per-row, controls membership in `sync.enabled_games` and
  sorts starred games to the top. Only starred rows show action buttons.
- **Action buttons** (starred only): Backup, Restore, Push, Pull, Status --
  all in the per-game settings modal (right-click / long-press a card).
- **Status text**: shows last push time and device, or "no cloud sync record".
- **Task badge**: a spinner on the card while an operation runs, then how it
  ended (`✓` / `!` / `✕`). Kept after the modal is closed, so a failure isn't
  lost -- click it to reopen the log.

### Task log, progress, and cancellation

`backup_game`, `restore_game`, `sync_push`, `sync_pull` all run through one
`run_task` wrapper in `src-tauri/src/lib.rs`:

- **Background thread**: `spawn_blocking`, so the webview stays responsive.
- **One at a time**: a second task is refused with a message rather than
  blocking silently on the `Ludusavi` lock (which reads as a hung button).
- **Streams progress** back to the UI as it works:
  `task-phase` (which step -- a push is backup-then-upload, a pull is
  download-then-restore), `task-progress` (rclone's byte counts for the cloud
  transfer; backup/restore have no per-file hook, so their bar is
  indeterminate), and `task-log` (see below).
- **Answers with a `TaskOutcome`**: how many files/objects actually changed, bytes,
  warnings, and per-file errors. `api.rs` returns `Ok` even when individual
  files fail -- the failures live in the output -- so this is the only place
  they're visible. Previously the Tauri layer returned a file count and dropped
  all of it. A file that already matched is `Same`, not a change, so re-running
  a backup of an unchanged game correctly reports zero changes.
- **Cancellable**: `task_cancel` flips a `Cancel` token shared with the core
  (`Cancel` is threaded through `sync.rs`'s rclone wait, the backup copy loop,
  and the restore loop; rclone gets killed). A cancelled task resolves as a
  normal result with `cancelled: true`, not an error, and reports **no tally** --
  a mid-operation cancel aborts the `api.rs` call, so there's no output to
  summarize. Nothing completed is lost: a cloud copy is additive, and a cancelled
  backup keeps the contents it already had.

While a task runs, the per-game modal refuses to close -- `✕`, the overlay, and
Escape are all disabled. A task has no owner once its modal is gone, so there'd
be no way to see its progress, cancel it, or read what it reported.

#### Log bridge

The core logs through the `log` crate. The CLI installs a file logger
(`src/main.rs`); a library consumer installs nothing, so every `log::warn!` /
`error!` in the core ("rclone failed", "no Wine prefix found", ...) was
silently dropped in this app. `install_log_bridge` in `src-tauri/src/lib.rs`
installs a logger that forwards those records to the running task's log in the
webview as `task-log` events, at `Info` and above -- enough for a readable
transcript of what the core did, without the `Debug`/`Trace` chatter that would
bury it. The panel keeps the last 400 lines. Records outside a task have no game
to attach to, so warnings and errors go to stderr -- visible in a terminal
`tauri dev` run.

### Cloud settings (`CloudSettings.tsx`)

Accessed via the gear icon in the header. Plain `useState` page switch, no
router library.

- Connect/disconnect Google Drive (opens browser for OAuth).
- Cloud folder path (the rclone remote subfolder).
- Auto-upload-after-backup toggle (upstream's bulk-sync feature, separate from
  the fork's manual per-game push/pull).

## Tauri Commands

All commands bind to `api.rs::Ludusavi` -- the same surface the CLI's
`sync push|pull|status` subcommand uses. Extend `api.rs` rather than reaching
into `sync.rs`/`cloud.rs` from here.

| Command | Description |
|---------|-------------|
| `sync_push` | Push one game's local backup to cloud (additive, no delete) |
| `sync_pull` | Pull one game's backup from cloud (additive, no delete) |
| `sync_status` | Last-known cloud sync info for a game (`settings.config`) |
| `enabled_games` | List games in `sync.enabled_games` |
| `search_games` | Fuzzy search the full manifest (capped at 50) |
| `set_game_enabled` | Add/remove a game from `sync.enabled_games` |
| `backup_game` | Take a local backup of one game (scan roots, copy saves) |
| `restore_game` | Restore one game's latest local backup over the live save |
| `task_cancel` | Cancel the running backup/restore/push/pull |
| `scan_games` | Full-library preview backup -- find all installed games |
| `cancel_scan` | Stop a running full-library scan |
| `wine_prefix_check` | Cross-device Wine/Proton remap diagnostics for a game |
| `game_scan_entries` | Save files/registry entries found for one game |
| `toggle_game_path` / `set_group_ignored` | Include/exclude entries from backup |
| `game_cover` / `set_custom_cover` / `clear_custom_cover` | Cover art |
| `sync_status_batch` | Cloud sync info for many games at once |
| `cloud_status` | Current cloud remote, path, rclone validity |
| `connect_google_drive` | Start rclone OAuth flow (opens browser) |
| `disconnect_cloud` | Tear down cloud remote config |
| `set_cloud_path` | Set the cloud-side folder name |
| `set_cloud_synchronize` | Toggle auto-upload after backup |

## Tests

```bash
cd desktop
pnpm test                                  # frontend (Node's runner, no framework dep)
cd src-tauri && cargo test                 # backend (needs the system deps below)
```

What they cover, and why each exists:

**`src-tauri/src/lib.rs`** (`mod tests`, 30 tests) -- the parts of the task pipeline
that are pure logic and would otherwise only be exercised by clicking through a GUI:

- `TaskSlot`: the one-at-a-time policy. That a second task is refused *and the message
  names the task in the way*, that the slot is released on the way out (including twice,
  since `run_task` releases it before propagating the `Err` a panicking body produces),
  and that a cancel only reaches the task that's actually running. This is the seam that
  lets the policy be tested at all -- it was inline in `run_task`, behind a Tauri
  `AppHandle`.
- `summarize_output`: the whole reason the backend returns a `TaskOutcome`. That a
  failed file is reported rather than swallowed (this is the bug: `api.rs` returns `Ok`
  with the failure buried in the output), that reported errors are capped at 8 with a
  count of the remainder, that ignored files and registry entries are counted correctly,
  that an already-matching file isn't counted as a change, and that a post-operation
  cloud-sync failure is a warning rather than a failed backup.
- The label and level contracts the frontend depends on: `TaskKind`'s serialized names,
  `phase_name`'s four strings, and the log bridge's `Info`-and-above policy. A rename on
  one side would otherwise just quietly break the UI.

**`test/tasks.test.ts`** (22 tests) -- how a `TaskOutcome` becomes log lines and a card
badge: level choice (a partial restore is a warning, not a failure; a cancelled task is
"stopped", never an error), that a cancel claims no tally it can't have, the wording of
the summary, `formatBytes`, the 400-line log cap keeping the newest lines, and a check
that the Rust source still emits the exact phase and task labels `tasks.ts` keys on.

Not covered by either: `run_task` itself and the Tauri command bodies, which need a
running app (and a real backup root) to exercise.

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (latest stable)
- [pnpm](https://pnpm.io/)
- [rclone](https://rclone.org/) on your PATH
- Linux: `gcc libxcb-composite0-dev libgtk-3-dev libwebkit2gtk-4.1-dev`

### Running in dev mode

```bash
cd desktop
pnpm install
pnpm run tauri:dev
```

This uses `scripts/dev.sh`, which sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` to
work around a WebKitGTK/Wayland crash on some compositor/GPU combos. If it
still fails, the script retries with `GDK_BACKEND=x11` (XWayland fallback).

You can also run `pnpm tauri dev` directly if Wayland works fine on your setup,
but the dev.sh wrapper is the safe default.

### Building a release binary

```bash
cd desktop
pnpm install
pnpm run tauri build
```

Produces a portable AppImage in `src-tauri/target/release/bundle/appimage/`.
Single file, no install required -- just `chmod +x` and run.

### CI builds (GitHub Actions)

The `desktop-build.yaml` workflow builds an AppImage automatically when you push
a version tag and uploads it to the GitHub release:

```bash
git tag v0.31.0
git push origin v0.31.0
```

The workflow runs in an Arch Linux container, installs all Tauri system
dependencies, derives the version from the tag via [dunamai](https://github.com/mtkennerly/dunamai),
injects it into `tauri.conf.json`, and uploads the resulting `.AppImage` to the
release. The version in `tauri.conf.json` is a placeholder (`0.1.0`) -- CI
overwrites it at build time.

## Key Files to Read

- `../src/api.rs` -- the `Ludusavi` struct and all its methods (the integration
  surface for this frontend).
- `src-tauri/src/lib.rs` -- Tauri command handlers and app setup.
- `src/App.tsx` -- the sync screen UI and game list logic.
- `src/CloudSettings.tsx` -- cloud configuration screen.
- `src/GameSettingsModal.tsx` -- per-game modal, including the task log panel.
- `src/tasks.ts` -- task/log types and helpers shared by the two components.
- `test/tasks.test.ts` -- tests for `tasks.ts` (`pnpm test`).
- `../AGENTS.md` -- fork design notes, phase checklist, and the Wine/Proton
  remap docs.
