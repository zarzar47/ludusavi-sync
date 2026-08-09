// Tauri backend for the fork's sync frontend.
//
// Commands bind straight to `api.rs::Ludusavi` (the same surface the CLI's
// `sync push|pull|status` subcommand uses) - no subprocess, no JSON-stdio
// hop. See AGENTS.md "Frontend Pivot" and CLAUDE.md for why.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ludusavi::{
    api::{CloudStatus, Ludusavi, parameters},
    path::StrictPath,
    prelude::{Cancel, Finality},
    report::ApiGame,
    resource::{SaveableResourceFile, sync_state::GameSyncEntry},
    scan::registry::RegistryItem,
    sync::SyncProgress,
};
use serde::Serialize;
use tauri::{Emitter, Manager};

/// `Ludusavi::load()` needs `manifest.yaml` to already exist (via `ludusavi manifest
/// update` or a prior GUI/CLI run), so on a fresh checkout it can fail. Hold that as
/// `None` rather than crashing app startup; commands surface a clear error instead.
struct AppState {
    ludusavi: Mutex<Option<Ludusavi>>,
    /// Cooperative cancel flag for the in-flight `scan_games`, flipped by `cancel_scan`.
    /// Kept outside the `Ludusavi` mutex so a cancel is delivered even while a scan
    /// is holding that lock.
    scan_cancel: Arc<AtomicBool>,
}

fn load_ludusavi() -> Option<Ludusavi> {
    match Ludusavi::load() {
        Ok(l) => Some(l),
        Err(e) => {
            eprintln!("Failed to load Ludusavi state (run `ludusavi manifest update` first?): {e:?}");
            None
        }
    }
}

fn with_ludusavi<T>(
    state: &tauri::State<AppState>,
    f: impl FnOnce(&Ludusavi) -> Result<T, String>,
) -> Result<T, String> {
    let guard = state.ludusavi.lock().map_err(|e| e.to_string())?;
    let ludusavi = guard
        .as_ref()
        .ok_or_else(|| "Config/manifest not loaded - run `ludusavi manifest update`, then restart".to_string())?;
    f(ludusavi)
}

fn with_ludusavi_mut<T>(
    state: &tauri::State<AppState>,
    f: impl FnOnce(&mut Ludusavi) -> Result<T, String>,
) -> Result<T, String> {
    let mut guard = state.ludusavi.lock().map_err(|e| e.to_string())?;
    let ludusavi = guard
        .as_mut()
        .ok_or_else(|| "Config/manifest not loaded - run `ludusavi manifest update`, then restart".to_string())?;
    f(ludusavi)
}

/// Live progress of a `sync_push`/`sync_pull`, streamed to the webview as
/// `"sync-progress"` events so the UI can show a per-game progress bar.
#[derive(Clone, Serialize)]
struct SyncProgressEvent {
    game: String,
    current: f32,
    total: f32,
}

/// Push a single game's local backup to the cloud (additive - never deletes other games).
#[tauri::command]
async fn sync_push(
    app: tauri::AppHandle,
    game: String,
    preview: bool,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    with_ludusavi(&state, |l| {
        let finality = if preview { Finality::Preview } else { Finality::Final };
        let mut on_progress = |p: SyncProgress| {
            let _ = app.emit(
                "sync-progress",
                SyncProgressEvent {
                    game: game.clone(),
                    current: p.current,
                    total: p.max,
                },
            );
        };
        l.sync_push(&game, finality, Some(&mut on_progress))
            .map(|r| r.changes.len())
            .map_err(|e| format!("{e:?}"))
    })
}

/// Pull a single game's backup from the cloud (additive - never deletes local data).
#[tauri::command]
async fn sync_pull(
    app: tauri::AppHandle,
    game: String,
    preview: bool,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    with_ludusavi(&state, |l| {
        let finality = if preview { Finality::Preview } else { Finality::Final };
        let mut on_progress = |p: SyncProgress| {
            let _ = app.emit(
                "sync-progress",
                SyncProgressEvent {
                    game: game.clone(),
                    current: p.current,
                    total: p.max,
                },
            );
        };
        l.sync_pull(&game, finality, Some(&mut on_progress))
            .map(|r| r.changes.len())
            .map_err(|e| format!("{e:?}"))
    })
}

/// Last-known cloud sync info for a game, from `settings.config`.
#[tauri::command]
async fn sync_status(game: String, state: tauri::State<'_, AppState>) -> Result<Option<GameSyncEntry>, String> {
    with_ludusavi(&state, |l| Ok(l.sync_status(&game)))
}

/// Batched `sync_status`: one `settings.config` read for every requested game instead
/// of one per game, for an always-visible per-card sync badge (see `sync_status_batch`
/// on `Ludusavi`).
#[tauri::command]
async fn sync_status_batch(
    games: Vec<String>,
    state: tauri::State<'_, AppState>,
) -> Result<HashMap<String, GameSyncEntry>, String> {
    with_ludusavi(&state, |l| Ok(l.sync_status_batch(&games)))
}

#[derive(Serialize)]
struct WinePrefixCheck {
    /// Wine/Proton prefix(es) the game's latest backup recorded as its source.
    backup_prefixes: Vec<String>,
    /// Wine/Proton prefix(es) actually found on this machine right now.
    local_prefixes: Vec<String>,
    /// Every device's recorded prefix for this game, from `settings.config`.
    registered: BTreeMap<String, String>,
}

/// Detects a restore hazard: the latest backup recorded a Wine/Proton prefix, but none
/// was found locally. A CLI `ludusavi restore` would fail with `WinePrefixNotFound` (or
/// silently misdirect if `scan.redirect_wine` is off) - there's no in-app restore yet,
/// so this only warns rather than gating anything.
#[tauri::command]
async fn wine_prefix_check(game: String, state: tauri::State<'_, AppState>) -> Result<WinePrefixCheck, String> {
    with_ludusavi(&state, |l| {
        Ok(WinePrefixCheck {
            backup_prefixes: l.backup_wine_prefixes(&game),
            local_prefixes: l.wine_prefixes_for(&game),
            registered: l.registered_prefixes(&game),
        })
    })
}

/// Games currently enabled for sync (`config.yaml`'s `sync.enabled_games`).
#[tauri::command]
async fn enabled_games(state: tauri::State<'_, AppState>) -> Result<Vec<String>, String> {
    with_ludusavi(&state, |l| Ok(l.config.sync.enabled_games.iter().cloned().collect()))
}

/// Search every game Ludusavi recognizes, for the "add a game" search box.
#[tauri::command]
async fn search_games(query: String, state: tauri::State<'_, AppState>) -> Result<Vec<String>, String> {
    with_ludusavi(&state, |l| Ok(l.search_games(&query, 50)))
}

/// Low-res Steam-style cover art for a game, as a `data:` URI the webview can drop
/// straight into an `<img src>` - `None` when the game has no known Steam ID or Steam
/// has no art for it, in which case the card just renders without one.
///
/// The Steam ID lookup only needs a quick read of the loaded manifest, but the fetch
/// itself is a blocking HTTP call (`reqwest::blocking`, cached to disk after the first
/// hit), so it runs on the async runtime's blocking pool rather than holding `state`'s
/// lock across an await.
#[tauri::command]
async fn game_cover(game: String, state: tauri::State<'_, AppState>) -> Result<Option<String>, String> {
    let steam_id = with_ludusavi(&state, |l| Ok(l.steam_id_for(&game)))?;
    tauri::async_runtime::spawn_blocking(move || ludusavi::api::cover_art_for_game(&game, steam_id))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:?}"))
}

/// Replace `game`'s cover art with a user-picked image (`source_path`, e.g. from the
/// native file dialog the frontend opens via `@tauri-apps/plugin-dialog`). Returns the
/// new cover as a `data:` URI so the card can update immediately.
#[tauri::command]
async fn set_custom_cover(game: String, source_path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        ludusavi::api::set_custom_cover(&game, &StrictPath::from(source_path))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:?}"))
}

/// Revert `game` to its default cover art (Steam's, or none), undoing [`set_custom_cover`].
#[tauri::command]
async fn clear_custom_cover(game: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || ludusavi::api::clear_custom_cover(&game))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:?}"))
}

/// One save file or registry entry found for a single game, for the per-game settings
/// screen's include/exclude checklist.
#[derive(Serialize)]
struct ScanEntry {
    path: String,
    /// Whether it's currently excluded from backup/sync (`config.yaml`'s
    /// `backup.toggledPaths`/`toggledRegistry`) - the checkbox's unchecked state.
    ignored: bool,
    kind: &'static str,
}

/// Save files (and, on Windows, registry entries) found for a single game - a
/// single-game version of [`scan_games`]'s full-library preview, for the per-game
/// settings screen opened by right-click/long-press on a card.
#[tauri::command]
async fn game_scan_entries(game: String, state: tauri::State<'_, AppState>) -> Result<Vec<ScanEntry>, String> {
    with_ludusavi_mut(&state, |l| {
        let output = l
            .back_up(parameters::BackUp {
                games: vec![game.clone()],
                finality: Finality::Preview,
                ..Default::default()
            })
            .map_err(|e| format!("{e:?}"))?;

        let mut entries = Vec::new();
        if let Some(ApiGame::Operative { files, registry, .. }) = output.games.get(&game) {
            entries.extend(files.iter().map(|(path, file)| ScanEntry {
                path: path.clone(),
                ignored: file.ignored,
                kind: "file",
            }));
            entries.extend(registry.iter().map(|(path, entry)| ScanEntry {
                path: path.clone(),
                ignored: entry.ignored,
                kind: "registry",
            }));
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    })
}

/// Include or exclude one save file/registry entry from `game`'s backup/sync
/// (`config.yaml`'s `backup.toggledPaths`/`toggledRegistry`), mirroring upstream
/// ludusavi's per-file checkboxes. Returns the entry's new `ignored` state.
#[tauri::command]
async fn toggle_game_path(
    game: String,
    path: String,
    kind: String,
    state: tauri::State<'_, AppState>,
) -> Result<bool, String> {
    with_ludusavi_mut(&state, |l| {
        let ignored = if kind == "registry" {
            let item = RegistryItem::new(path);
            l.config.backup.toggled_registry.toggle(&game, &item, None);
            l.config.backup.toggled_registry.is_ignored(&game, &item, None)
        } else {
            let item = StrictPath::new(path);
            l.config.backup.toggled_paths.toggle(&game, &item);
            l.config.backup.toggled_paths.is_ignored(&game, &item)
        };
        l.config.save();
        Ok(ignored)
    })
}

/// Set several save files/registry entries' included state at once (the settings
/// modal's per-folder "select all"/"select none", for games with far too many
/// individual files to click one at a time - e.g. Baldur's Gate 3). One
/// `config.yaml` write for the whole batch rather than one per entry.
#[tauri::command]
async fn set_group_ignored(
    game: String,
    paths: Vec<String>,
    kind: String,
    ignored: bool,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    with_ludusavi_mut(&state, |l| {
        for path in paths {
            if kind == "registry" {
                let item = RegistryItem::new(path);
                if l.config.backup.toggled_registry.is_ignored(&game, &item, None) != ignored {
                    l.config.backup.toggled_registry.toggle(&game, &item, None);
                }
            } else {
                let item = StrictPath::new(path);
                if l.config.backup.toggled_paths.is_ignored(&game, &item) != ignored {
                    l.config.backup.toggled_paths.toggle(&game, &item);
                }
            }
        }
        l.config.save();
        Ok(())
    })
}

/// Enable or disable a game for cloud sync.
#[tauri::command]
async fn set_game_enabled(game: String, enabled: bool, state: tauri::State<'_, AppState>) -> Result<(), String> {
    with_ludusavi_mut(&state, |l| {
        l.set_game_enabled(&game, enabled);
        Ok(())
    })
}

/// Take a real local backup of one game (scans its roots, copies changed saves into the
/// backup dir) - the step that has to happen before there's anything to push.
#[tauri::command]
async fn backup_game(game: String, state: tauri::State<'_, AppState>) -> Result<usize, String> {
    with_ludusavi_mut(&state, |l| {
        let output = l
            .back_up(parameters::BackUp {
                games: vec![game.clone()],
                finality: Finality::Final,
                ..Default::default()
            })
            .map_err(|e| format!("{e:?}"))?;

        Ok(match output.games.get(&game) {
            Some(ApiGame::Operative { files, registry, .. }) => files.len() + registry.len(),
            _ => 0,
        })
    })
}

/// One game found by [`scan_games`]: it has actual local save data on this machine,
/// whether or not it's currently enabled for sync.
#[derive(Serialize)]
struct ScanResult {
    name: String,
    file_count: usize,
    registry_count: usize,
    /// Debug-formatted `ScanChange` (e.g. "New", "Different", "Same").
    change: String,
}

/// Scan every game Ludusavi knows about against this machine's configured roots - the
/// same full-library preview upstream ludusavi does on startup. Slow-ish (walks
/// thousands of possible games), which is inherent to the operation, not this UI.
///
/// Runs as an async command so the webview stays responsive throughout.
#[tauri::command]
async fn scan_games(state: tauri::State<'_, AppState>) -> Result<Vec<ScanResult>, String> {
    state.scan_cancel.store(false, Ordering::Relaxed);
    let cancel = Cancel::from_flag(state.scan_cancel.clone());

    let output = with_ludusavi_mut(&state, |l| {
        l.back_up(parameters::BackUp {
            finality: Finality::Preview,
            cancel: Some(cancel.clone()),
            ..Default::default()
        })
        .map_err(|e| format!("{e:?}"))
    })?;

    // Cancelled: the partial preview is meaningless, and we must not persist a
    // half-scanned library as the discovered set. Return nothing; the UI has
    // already torn down the spinner.
    if cancel.is_cancelled() {
        return Ok(vec![]);
    }

    let mut results: Vec<ScanResult> = output
        .games
        .into_iter()
        .filter_map(|(name, game)| match game {
            ApiGame::Operative {
                change,
                files,
                registry,
                ..
            } => Some(ScanResult {
                name,
                file_count: files.len(),
                registry_count: registry.len(),
                change: format!("{change:?}"),
            }),
            _ => None,
        })
        .collect();
    results.sort_by(|a, b| a.name.cmp(&b.name));

    // Persist the discovered names so a restart doesn't require re-scanning.
    // A successful full scan replaces the set, pruning games no longer installed.
    with_ludusavi_mut(&state, |l| {
        l.set_discovered_games(results.iter().map(|r| r.name.clone()));
        Ok(())
    })?;

    Ok(results)
}

/// Cancel the in-flight [`scan_games`], if any. Cheap and immediate: it only flips
/// the shared flag; the scan's per-game steps notice and unwind.
#[tauri::command]
async fn cancel_scan(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.scan_cancel.store(true, Ordering::Relaxed);
    Ok(())
}

/// Games found by a previous [`scan_games`], persisted so a restart doesn't
/// require re-scanning (`config.yaml`'s `sync.discovered_games`).
#[tauri::command]
async fn discovered_games(state: tauri::State<'_, AppState>) -> Result<Vec<String>, String> {
    with_ludusavi(&state, |l| Ok(l.discovered_games()))
}

/// Current cloud remote/path/rclone status, for the settings screen.
#[tauri::command]
async fn cloud_status(state: tauri::State<'_, AppState>) -> Result<CloudStatus, String> {
    with_ludusavi(&state, |l| Ok(l.cloud_status()))
}

/// Configure Google Drive as the cloud remote.
///
/// This drives rclone's own OAuth flow (opens a browser, waits for approval) and can
/// take a while. Takes an `AppHandle` (not `State`) so the whole call can run inside
/// `spawn_blocking` - `State`'s lifetime is tied to this invocation and can't move into
/// a `'static` closure, but `AppHandle` can, and re-derives `AppState` via `.state()`
/// once inside. Without this the OAuth wait would tie up an async runtime thread for as
/// long as the user takes to approve in their browser.
#[tauri::command]
async fn connect_google_drive(app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        with_ludusavi_mut(&state, |l| {
            l.set_cloud_remote_google_drive().map_err(|e| format!("{e:?}"))
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Tear down the configured cloud remote (both rclone's own config and ours).
#[tauri::command]
async fn disconnect_cloud(state: tauri::State<'_, AppState>) -> Result<(), String> {
    with_ludusavi_mut(&state, |l| l.disconnect_cloud_remote().map_err(|e| format!("{e:?}")))
}

/// Cloud-side folder name to sync into.
#[tauri::command]
async fn set_cloud_path(path: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    with_ludusavi_mut(&state, |l| {
        l.set_cloud_path(path);
        Ok(())
    })
}

/// Toggle upstream's auto-upload-after-backup. Separate from this fork's manual
/// `sync_push`/`sync_pull`.
#[tauri::command]
async fn set_cloud_synchronize(enabled: bool, state: tauri::State<'_, AppState>) -> Result<(), String> {
    with_ludusavi_mut(&state, |l| {
        l.set_cloud_synchronize(enabled);
        Ok(())
    })
}

/// Manually point at the `rclone` binary, for when it's installed but not on `PATH`.
#[tauri::command]
async fn set_rclone_path(path: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    with_ludusavi_mut(&state, |l| {
        l.set_rclone_path(path);
        Ok(())
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // WebKitGTK/GTK's default GPU-accelerated rendering fails on several real
    // setups this app targets - gamescope on Steam Deck ("Could not create
    // default EGL display: EGL_BAD_PARAMETER", blank white window persisting
    // even with just WEBKIT_DISABLE_DMABUF_RENDERER) and some Wayland/GPU
    // driver combos on desktop Linux (desktop/scripts/dev.sh sets the DMA-BUF
    // var for `tauri dev`, for the same underlying class of failure). The
    // EGL_BAD_PARAMETER case happens at raw EGL display creation - below
    // WebKit's own compositor, so disabling just its DMA-BUF renderer isn't
    // enough; it's GTK's GL context init itself failing under gamescope's
    // nested compositor. This app has no need for GPU-accelerated rendering
    // (it's a settings/file-list UI, not a game), so force software
    // rendering outright rather than chasing every GPU/EGL-platform mismatch
    // individually. Bakes into the binary so an AppImage works out of the box
    // - don't overwrite a var the user already set themselves.
    for (key, value) in [
        ("WEBKIT_DISABLE_DMABUF_RENDERER", "1"),
        ("WEBKIT_DISABLE_COMPOSITING_MODE", "1"),
        ("LIBGL_ALWAYS_SOFTWARE", "1"),
    ] {
        if std::env::var(key).is_err() {
            unsafe {
                std::env::set_var(key, value);
            }
        }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            ludusavi: Mutex::new(load_ludusavi()),
            scan_cancel: Arc::new(AtomicBool::new(false)),
        })
        .invoke_handler(tauri::generate_handler![
            sync_push,
            sync_pull,
            sync_status,
            sync_status_batch,
            wine_prefix_check,
            enabled_games,
            discovered_games,
            search_games,
            game_cover,
            set_custom_cover,
            clear_custom_cover,
            game_scan_entries,
            toggle_game_path,
            set_group_ignored,
            set_game_enabled,
            backup_game,
            scan_games,
            cancel_scan,
            cloud_status,
            connect_google_drive,
            disconnect_cloud,
            set_cloud_path,
            set_cloud_synchronize,
            set_rclone_path
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
