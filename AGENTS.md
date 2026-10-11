# Ludusavi - Cross-Device Save Sync

## Architecture Summary

Ludusavi is a Rust game save backup tool being modified into a **per-game cloud sync tool** for Steam Deck and PC.

### Current State (post-GUI-strip)
- **Framework**: Clap CLI only. The Phase 1-3 iced GUI (`src/gui/`) was deliberately deleted — see "Frontend Pivot" below.
- **Cloud**: `rclone copy` (additive only - never deletes from cloud) for per-game `sync push`/`sync pull`; `rclone sync` (mirroring, can delete) still backs the older bulk `cloud upload`/`cloud download` commands.
- **Backup**: Bulk backup/restore of all games via `BackupLayout` / `GameLayout`, plus per-game push/pull via `sync.rs`.
- **Config**: YAML-based (`config.yaml`) with roots, redirects, custom games, cloud settings, plus fork's `SyncConfig { enabled_games }`.
- **Sync metadata**: JSON `settings.config` at the backup root, per-game `{ last_push, device, mapping_path }` (`resource/sync_state.rs`).

### Frontend Pivot (current phase)
The original plan below (Phases 1-3) built a Rust-native iced GUI with game cards. That GUI has since been **removed**. Decision: no more Rust-native GUI toolkits — the next frontend will be **Tauri** (JS/HTML/React frontend, Rust backend). Rationale: Rust GUI ecosystem (iced included) was judged not worth fighting; Tauri gets a non-Rust frontend without Electron's Chromium+Node bundle weight, which matters on Steam Deck, and its commands call straight into `api.rs`'s `Ludusavi` struct in-process (no subprocess/JSON-stdio hop).

Sequencing:
1. **Done** — strip iced GUI, go CLI-only, give `sync.rs` real callers (`cli.rs`'s `sync push|pull|status` subcommand, `api.rs`'s `Ludusavi::sync_push`/`sync_pull`/`sync_status`).
2. **Done** — Tauri app scaffolded at `desktop/` (React+TS frontend, `pnpm`). `desktop/src-tauri` depends on the root crate as a path dependency (`ludusavi = { path = "../..", default-features = false }` — no `app` feature, so no clap/rfd/dialoguer in the Tauri backend's dep tree). `Ludusavi::load()` happens once at startup into managed state (`Option<Mutex<Ludusavi>>`, `None` if `manifest.yaml` isn't present yet, so a fresh checkout doesn't crash the window on launch).
3. **Done** — real sync screen, replacing the placeholder list. `App.tsx`: one unified, deduplicated game list (no separate "enabled" vs "search" sections) — a debounced search box (`search_games`, capped at 50, only queries non-empty input) and a "Scan" button (`scan_games`, full-library preview backup, same cost as upstream's startup scan) both feed candidates into it; a star toggle per row (`set_game_enabled`) controls both `sync.enabled_games` membership and sort-to-top, and gates whether Backup/Restore/Push/Pull/Status buttons show. `CloudSettings.tsx` is a separate page (gear icon, no router lib — plain `useState` page switch in `App.tsx`): connect/disconnect a cloud provider — Google Drive / Dropbox / OneDrive via the `connect_cloud(provider)` Tauri command (`cloud::CloudProvider`), one active remote at a time (connecting tears the old one down; switch = disconnect then connect), cloud folder path, auto-sync-after-backup toggle.
4. **Done** — `sync push`/`sync pull` now compose with local backup/restore instead of being bare cloud transfers: `api.rs::Ludusavi::sync_push` takes a fresh local backup of the game before uploading (so the cloud gets the current save, not a stale one); `sync_pull` restores the download over the current local save immediately after downloading it — destructive locally, by design (previously pull only landed files in backup storage and never touched the live save, which was the actual bug behind the "pulled on PC, save didn't change" report). Both suppress `config.cloud.synchronize` (upstream's bulk mirror-sync) for the duration of their inner backup/restore call, so they stay purely this fork's additive per-game mechanism. `cli.rs`'s `sync push|pull` now goes through `api.rs::Ludusavi` too (previously called `sync.rs::push_game`/`pull_game` directly, bypassing `api.rs`). `desktop/`'s Pull button confirms before firing, since it's now destructive.
5. **Done** — per-game task pipeline: `backup_game`/`restore_game`/`sync_push`/`sync_pull` all run through one `run_task` wrapper in `desktop/src-tauri/src/lib.rs` (see "Task pipeline" below). Live phase/progress events, a per-game log, real cancellation, and a card badge replace the previous "button hangs, then a file count or nothing" behaviour.
6. **Done** — Wine/Proton remap diagnostics surfaced in the UI: the per-game modal warns when a pulled backup's semantics won't resolve locally *before* the user clicks Pull and hits the `WinePrefixNotFound` refusal for real (no longer hypothetical, since Pull actually restores).
7. **Next** — batch "sync all starred": the task pipeline deliberately runs one task at a time (`run_task` refuses a second with a message rather than blocking silently on the `Ludusavi` lock), so this needs a queue on top of it, plus a "sync all" entry point that doesn't force opening each game's modal.

When picking up GUI work again: build the new Tauri frontend against `api.rs`, not against `sync.rs`/`cloud.rs` directly, and don't recreate `src/gui/`. Run it with `cd desktop && pnpm install && pnpm run tauri:dev` (works around a WebKitGTK/Wayland crash some compositors hit with plain `pnpm tauri dev` — see `desktop/scripts/dev.sh`). Building a real binary for daily use (CLI and/or the Tauri app, for this machine or a Steam Deck): see root `README.md`'s "Building" section.

## Task pipeline (backup/restore/push/pull)

All four per-game operations in `desktop/` go through `run_task` in `desktop/src-tauri/src/lib.rs`, so they behave the same way instead of each being a button that hangs:

- **Off the async runtime, one at a time.** The body runs on `spawn_blocking` (long, synchronous, IO-bound). A second concurrent task is refused with a message instead of blocking silently on the `Ludusavi` lock, which reads as a hung button.
- **Structured result, not a file count.** `api.rs` returns `Ok` even when individual files fail — the failures live in the `ApiOutput`. `summarize_output` turns that into a `TaskOutcome` (changes, bytes, warnings, per-file errors, capped at 8). Previously the Tauri layer returned just a count and dropped everything else.
- **Live events.** `task-phase` (a push is backup-then-upload, a pull download-then-restore), `task-progress` (rclone's bytes; backup/restore have no per-file hook, so their bar is indeterminate), `task-log`. All tagged with the game name, so `App.tsx` registers the listeners once on mount rather than per invocation.
- **Cancellation.** `Cancel` is threaded from the core outward: `sync.rs`'s rclone wait kills the child process, `scan/layout.rs`'s copy and restore loops check it per file (`parameters::Restore::cancel`), and `Error::Cancelled` is a distinct case from failure — a cancelled task resolves as `Ok` with `cancelled: true`. `task_cancel` only flips the shared token, which needs no lock, so it works mid-transfer. A cancelled task reports **no tally**: a mid-operation cancel aborts the `api.rs` call, so there's no `ApiOutput` to summarize and the log must not imply a count exists (though nothing completed is lost — a cloud copy is additive, and a cancelled backup keeps its previous contents).
- **A cancel must never destroy an existing backup.** A `SOLO` backup (the default: simple format, one retained full backup) is written *in place* over the previous one, so its directory already holds files the new run may never reach. Cancelling such a run would otherwise make `remove_irrelevant_backup_files` **delete** every file the run didn't get to, then `prune_failures` drop them from the mapping — leaving a saved mapping pointing at deleted files. So `BackupInfo::cancelled()` gates both: a cancelled in-place run skips the cleanup *and* the prune, keeping the still-valid unreached files. Genuinely fresh (timestamped/zip) backups still prune as usual, matching ordinary per-file-failure behaviour.
- **Log bridge.** `install_log_bridge` installs a `log::Log` that forwards the core's records (at `Info` and above) to the running task's log in the webview. Without it the core's `log::warn!`/`error!` calls — "rclone failed", "no Wine prefix found" — went nowhere, since a library consumer installs no logger (only the CLI's `main.rs` does).
- **The modal owns a running task.** `GameSettingsModal` refuses to close (`✕`, overlay, Escape) while a task runs: without it there'd be no way to see progress, cancel, or read the result. The card keeps a badge (spinner → `✓`/`!`/`✕`) so the outcome survives the modal being closed afterwards.
- **Tested where the logic is pure.** `TaskSlot` exists as its own type precisely so the one-at-a-time/cancel/release policy can be tested without a Tauri `AppHandle` (29 tests in `src-tauri/src/lib.rs`); `tasks.ts`'s outcome-to-log/badge rendering is tested by `pnpm test` (21 tests, Node's runner, no framework dep), including a check that the Rust source still emits the exact phase/task label strings the frontend keys on. `run_task` and the command bodies need a running app, so they're the untested remainder.

## Cloud quota constraint (this fork's Google Drive)

As of 2026-10-12 the backing Google Drive is **over quota**: every upload is rejected with `googleapi: Error 403: ... storageQuotaExceeded`. `rclone about` still reports free space — it lies; assume cloud push/pull is broken until space is freed. Consequences baked into the code:

- Transfers **fail fast** instead of grinding through rclone's ~12-minute retry loop: `cloud.rs::is_quota_exceeded` recognizes the rejection in rclone's error/notice log lines (and its `stats.lastError`), `RcloneProcessEvent::Notice` carries it to the wait loops (`sync.rs::wait_for_rclone`, `api.rs`, `cli.rs`), which kill the child and return `Error::CloudStorageQuotaExceeded` — a direct "Cloud storage is full..." message rather than a bare exit code.
- Failures always carry a reason: `RcloneProcess` keeps a small tail of rclone's classified diagnostic lines (`push_diagnostic`), since the JSON-log runs drain their own stderr for progress and would otherwise leave `CommandError::Exited` with empty stderr. Note rclone emits **no stats lines at all** when every upload is rejected before any bytes move, so the log-line path is the primary detector, not the stats path.

## Cross-device Wine/Proton remap

Separate feature from the frontend pivot above, landed the same session. The bug: Baldur's Gate 3 (or any Steam Proton game) backed up on two machines produces two divergent absolute-path trees in the same cloud game folder — `/home/deck/.../compatdata/4110821628/pfx/...` vs `/home/kookoo/.../compatdata/2811670038/pfx/...` — because both the Linux username and the Proton compatdata app ID differ per machine (the app ID even for the *same* game, since a non-Steam-shortcut ID is generated per device). Pulling one onto the other and restoring wrote to the literal foreign path. Full design doc: `/home/kookoo/.claude/plans/delegated-wobbling-mochi.md`.

Fixed via two mechanisms kept deliberately separate — **decompose** (which absolute prefix in a given backup was a Wine prefix — `layout.rs::BackupSemantics`, per backup) and **recompose** (which prefix *this* device should write into — `GameSyncEntry.prefixes: BTreeMap<device, path>` in `settings.config`, per device, cross-device). See CLAUDE.md's "Cross-device Wine/Proton remap" section for the file-by-file mechanics; summary:

- `scan.rs::wine_prefixes_with_included_files` now includes Steam compatdata (`steamapps/compatdata/<id>/pfx`) as a candidate, not just custom-game/launcher prefixes — this is why a Proton backup's `mapping.yaml` now gets a `semantics` block at all.
- `sync.rs::push_game` records this device's resolved prefix into `settings.config`'s registry at push time; `scan/semantic/discovery.rs::WineEnvironment::prefixes_for_game` reads it back first at restore, falling back to re-discovery (Steam shortcut ID → manifest Steam ID → launcher prefix) for a device that's never pushed this game.
- `SyncStateFile::merge_from` (`resource/sync_state.rs`) replaced a straight overwrite on pull — each device only knows its own registry key, so a naive overwrite would erase every other device's entry on the next sync.
- `config.scan.redirect_wine` defaults `true` now (was `false`); restore refuses (`Error::WinePrefixNotFound`) rather than silently writing a foreign path when no local prefix can be found.

Verified end-to-end against the real bug report's data (`~/ludusavi-backup/Baldur's Gate 3/mapping.yaml` gained the expected `semantics` block; `settings.config` gained the expected `prefixes` entry after a real `sync push`).

### Original Target State (superseded by the pivot above, kept for history)
- **UI**: Single "Sync" screen with game cards (Steam Deck optimized: 48px+ buttons, larger fonts) — this was the iced GUI's design; the same card-based UX is still the goal, just rebuilt in the Tauri frontend instead.
- **Cloud**: `rclone copy` (additive only - never deletes from cloud) — done, still current.
- **Backup**: Per-game push/pull with merge on pull — done via `sync.rs`, now wired to the CLI/API.
- **Config**: JSON `settings.config` at backup root with per-game sync metadata — done, still current.

### Key Changes (Phases 1-3, historical)
1. Replace `rclone sync` with `rclone copy` for additive-only cloud operations
2. Replace Backup/Restore screens with game card UI *(superseded: screens were removed, not replaced; card UI is now a Tauri-frontend goal)*
3. Add `SyncConfig` to config for enabled games tracking
4. Add `SyncStateFile` for cloud metadata (settings.config)
5. New `sync.rs` module for push/pull/merge logic

---

## Development Todo

### Phase 1: Foundation (No UI)
- [x] Create `AGENTS.md` (this file)
- [x] Create `src/resource/sync_state.rs` - settings.config JSON read/write
- [x] Modify `src/resource/config.rs` - add `SyncConfig` with `enabled_games`
- [x] Modify `src/cloud.rs` - add `Rclone::copy()` method (additive, no delete)
- [x] Create `src/sync.rs` - `push_game()`, `pull_game()`, merge logic
- [x] Update `src/lib.rs` - add `pub mod sync`
- [x] **Commit Phase 1**

### Phase 2: GUI Redesign
- [x] Create `src/gui/game_card.rs` - card data model + Steam Deck rendering
- [x] Modify `src/gui/button.rs` - nav button for Sync screen
- [x] Modify `src/gui/screen.rs` - Sync screen with view()
- [x] Modify `src/gui/common.rs` - Screen::Sync, sync Message variants
- [x] Modify `src/gui/app.rs` - game_cards population, sync handlers
- [x] Modify `src/gui/style.rs` - Container::Card, Container::Badge
- [x] Modify `src/gui/widget.rs` - SYNC_SCROLL id
- [x] Modify `src/main.rs` - add sync to use ludusavi import
- [x] **Commit Phase 2**

### Phase 3: Integration & Polish
- [x] Wire up async progress for rclone operations (via rclone_monitor subscription)
- [x] Batch sync ("Sync All Enabled") with progress bar
- [x] Compute SyncState from local vs cloud comparison (NotSynced, LocalOnly, Synced, CloudNewer)
- [x] **Commit Phase 3**

---

## Steam Deck UI Requirements
- Card height: minimum 80px
- Buttons: minimum 48x48px touch targets
- Icons: 24px+ (larger than default 20px)
- Font sizes: +2-4px over defaults
- Padding: extra breathing room between elements

---

## File Change Summary

| File | Change | Description |
|------|--------|-------------|
| `AGENTS.md` | NEW | Architecture docs and todo |
| `src/resource/sync_state.rs` | NEW | settings.config JSON read/write |
| `src/resource/config.rs` | MODIFY | Add SyncConfig |
| `src/cloud.rs` | MODIFY | Add Rclone::copy() method |
| `src/sync.rs` | NEW | Push/pull/merge logic |
| `src/lib.rs` | MODIFY | Add pub mod sync |
| `src/gui/game_card.rs` | NEW | Game card model + Steam Deck rendering |
| `src/gui/icon.rs` | MODIFY | Add larger arrow icons |
| `src/prelude.rs` | MODIFY | `Cancel` token, `Error::Cancelled` |
| `src/lang.rs` | MODIFY | Cancellation error message |
| `src/scan/layout.rs` | MODIFY | Per-file cancellation in backup/restore copy loops; a cancelled in-place (`SOLO`) backup skips its destructive cleanup so a cancel can't delete a previous backup |
| `src/scan/backup.rs` | MODIFY | `BackupInfo::cancelled()` — distinguishes a cancel from an ordinary file failure |
| `desktop/src-tauri/src/lib.rs` | MODIFY | `run_task` pipeline, `TaskOutcome`, phase/progress/log events, `task_cancel`, log bridge |
| `desktop/src/tasks.ts` | NEW | Task/log types and helpers shared by the frontend components |
| `desktop/test/tasks.test.ts` | NEW | Node-runner tests for `tasks.ts` (`pnpm test`) |
| `desktop/src/GameSettingsModal.tsx` | MODIFY | Task log panel, progress/phase status, cancel button, close-lock while running |
| `desktop/src/App.tsx` | MODIFY | Task state + event listeners, card badge, one `runTask` entry point |
| `src/gui/button.rs` | MODIFY | Add push/pull buttons |
| `src/gui/screen.rs` | MODIFY | Replace with Sync screen |
| `src/gui/common.rs` | MODIFY | New types |
| `src/gui/app.rs` | MODIFY | New state machine |
| `src/gui/modal.rs` | MODIFY | Sync confirmation modals |
