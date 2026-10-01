// Tauri backend for the fork's sync frontend.
//
// Commands bind straight to `api.rs::Ludusavi` (the same surface the CLI's
// `sync push|pull|status` subcommand uses) - no subprocess, no JSON-stdio
// hop. See AGENTS.md "Frontend Pivot" and CLAUDE.md for why.
//
// Long operations (backup/restore/push/pull) are wrapped in a single task slot
// ([`run_task`]): one at a time, on the blocking pool, cancellable, and
// reporting progress, phases, and a structured outcome to the webview. That's
// what the frontend's per-game task log renders.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ludusavi::{
    api::{CloudStatus, Ludusavi, parameters},
    lang::TRANSLATOR,
    path::StrictPath,
    prelude::{Cancel, Error, Finality},
    report::{ApiGame, ApiOutput, SaveError},
    resource::{SaveableResourceFile, sync_state::GameSyncEntry},
    scan::{change::ScanChange, OperationStepDecision, registry::RegistryItem},
    sync::{SyncHooks, SyncPhase, SyncProgress},
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
    /// The in-flight backup/restore/push/pull, if any. Doubles as the mutual exclusion
    /// that keeps two tasks from queuing invisibly behind each other on the shared
    /// `Ludusavi` lock, and as the "which game is this core log line about?" lookup for
    /// the log bridge. Outside the `Ludusavi` mutex for the same reason as `scan_cancel`.
    task: TaskSlot,
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

/// Which long operation a task is running. Serialized as kebab-case for the frontend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum TaskKind {
    Backup,
    Restore,
    Push,
    Pull,
}

impl TaskKind {
    /// User-facing name, used in the "another task is running" message.
    fn label(&self) -> &'static str {
        match self {
            Self::Backup => "Backup",
            Self::Restore => "Restore",
            Self::Push => "Push",
            Self::Pull => "Pull",
        }
    }
}

/// The single in-flight long operation, if any.
struct ActiveTask {
    kind: TaskKind,
    game: String,
    cancel: Cancel,
}

/// Who is running, and the token to stop them.
///
/// All of the one-at-a-time policy lives here rather than inline in [`run_task`], so it
/// can be tested without a Tauri app handle: the `Ludusavi` mutex is held for the whole
/// of a task's body, so a second task would otherwise block on it invisibly.
struct TaskSlot(Mutex<Option<ActiveTask>>);

impl TaskSlot {
    fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// Claim the slot, or refuse with a message naming the task that's in the way.
    /// Returns the new task's cancel token, which the caller keeps for the duration.
    fn begin(&self, kind: TaskKind, game: String) -> Result<Cancel, String> {
        let mut slot = self.0.lock().map_err(|e| e.to_string())?;
        if let Some(active) = slot.as_ref() {
            return Err(format!(
                "{} of \"{}\" is still running - wait for it to finish, or cancel it",
                active.kind.label(),
                active.game
            ));
        }
        *slot = Some(ActiveTask {
            kind,
            game: game.clone(),
            cancel: Cancel::new(),
        });
        Ok(slot.as_ref().expect("just set").cancel.clone())
    }

    /// Release the slot. Safe to call more than once, and from any thread, including when
    /// the task body panicked (which surfaces as an `Err` from the join, not a normal
    /// return). `run_task` calls this before propagating that error.
    fn finish(&self) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = None;
        }
    }

    /// The running task's kind and game, for the log bridge's "whose line is this?".
    fn current(&self) -> Option<(TaskKind, String)> {
        self.0
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|t| (t.kind, t.game.clone())))
    }

    /// Ask the running task to stop. Only flips the shared token, which is why it works
    /// mid-transfer and never needs the `Ludusavi` lock.
    fn cancel(&self) -> bool {
        match self.0.lock() {
            Ok(slot) => match slot.as_ref() {
                Some(active) => {
                    active.cancel.cancel();
                    true
                }
                None => false,
            },
            Err(_) => false,
        }
    }
}

/// What a task did, in the shape the frontend's log renders.
///
/// Errors and warnings are separate on purpose: a backup where 2 of 400 files failed
/// succeeded overall, and the user should be able to see that without the whole task
/// being painted as a failure.
#[derive(Default, Serialize)]
struct TaskOutcome {
    /// The user cancelled, so the operation stopped early. Whatever it managed to do
    /// first is kept (a push is additive; a cancelled restore simply leaves the rest of
    /// the files still differing from the backup).
    cancelled: bool,
    /// Files written/restored (backup/restore) or objects transferred (push/pull).
    changes: usize,
    /// Bytes written/restored or transferred, when the operation reports a size.
    bytes: u64,
    /// How the game compares to its previous backup, or its backup to the live data,
    /// e.g. `"New"` or `"Different"`. Debug-formatted `ScanChange`.
    change: Option<String>,
    /// The operation completed, but with something the user should know about.
    warnings: Vec<String>,
    /// Files the operation couldn't handle. Non-fatal: the task still reports success.
    errors: Vec<String>,
}

/// Per-file errors are capped when reported - a game with 400 unreadable files should
/// show a handful, not a wall of text.
const MAX_REPORTED_ERRORS: usize = 8;

/// Why a task stopped. Cancellation is deliberately not an error: the frontend reports
/// it as "stopped", and the outcome it gets back is a normal (partial) result.
enum TaskError {
    Cancelled,
    Failed(String),
}

impl From<Error> for TaskError {
    fn from(error: Error) -> Self {
        match error {
            Error::Cancelled => Self::Cancelled,
            // The core's own wording ("Unable to synchronize with cloud", plus rclone's
            // stderr) is far more useful to a user than a Debug dump of the enum.
            other => Self::Failed(TRANSLATOR.handle_error(&other)),
        }
    }
}

impl From<String> for TaskError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<TaskError> for String {
    fn from(error: TaskError) -> Self {
        match error {
            TaskError::Cancelled => "cancelled".to_string(),
            TaskError::Failed(message) => message,
        }
    }
}

/// What a task body is handed: the `Ludusavi` handle (already locked) plus the tools to
/// report on itself while it runs.
struct TaskContext<'a> {
    app: &'a tauri::AppHandle,
    game: &'a str,
    cancel: &'a Cancel,
}

impl TaskContext<'_> {
    /// Announce which step of a push/pull is running, so the UI can say more than
    /// "working…" while the transfer bar sits still.
    fn phase(&self, phase: SyncPhase) {
        let _ = self.app.emit(
            "task-phase",
            TaskPhaseEvent {
                game: self.game.to_string(),
                phase: phase_name(phase).to_string(),
            },
        );
    }

    /// Stream rclone's byte progress. `SyncPhase::BackingUp`/`Restoring` have no
    /// equivalent: ludusavi's reporter doesn't emit per-file counts while copying, so
    /// the UI shows an indeterminate bar for those phases instead.
    fn progress(&self) -> impl FnMut(SyncProgress) + '_ {
        let app = self.app;
        let game = self.game.to_string();
        move |p: SyncProgress| {
            let _ = app.emit(
                "task-progress",
                TaskProgressEvent {
                    game: game.clone(),
                    current: p.current,
                    total: p.max,
                },
            );
        }
    }
}

fn phase_name(phase: SyncPhase) -> &'static str {
    match phase {
        SyncPhase::BackingUp => "backing-up",
        SyncPhase::Uploading => "uploading",
        SyncPhase::Downloading => "downloading",
        SyncPhase::Restoring => "restoring",
    }
}

#[derive(Clone, Serialize)]
struct TaskPhaseEvent {
    game: String,
    phase: String,
}

/// Live byte progress of a push/pull's rclone transfer, streamed to the webview so the
/// UI can show a real progress bar instead of a spinner.
#[derive(Clone, Serialize)]
struct TaskProgressEvent {
    game: String,
    current: f32,
    total: f32,
}

/// Run one long operation: claim the task slot, do the work on the blocking pool, hand
/// back a structured outcome.
///
/// Three things this buys over calling `api.rs` inline from an `async fn` command:
///
/// 1. It runs on `spawn_blocking`, not the async runtime. These operations are long,
///    synchronous, and IO-bound; inline, they occupied a runtime worker (and the
///    `Ludusavi` lock) for the whole task.
/// 2. Only one at a time. The `Ludusavi` lock already serializes them, but a second
///    task would then block silently, which reads as a hung button. Rejecting it with
///    a message is honest.
/// 3. Cancellation is delivered through a token that doesn't need that lock, so
///    `task_cancel` still works while the body is mid-transfer.
async fn run_task<F>(
    app: tauri::AppHandle,
    kind: TaskKind,
    game: String,
    body: F,
) -> Result<TaskOutcome, String>
where
    F: FnOnce(&mut Ludusavi, &TaskContext<'_>) -> Result<TaskOutcome, TaskError> + Send + 'static,
{
    let state = app.state::<AppState>();
    let cancel = state.task.begin(kind, game.clone())?;

    // A clone for the worker: the original is still needed afterwards, to free the slot.
    let worker = app.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let state = worker.state::<AppState>();
        // Recover from poisoning rather than refusing every future command. A panic in a
        // task body leaves the guard held-and-poisoned, but the `Ludusavi` behind it is
        // still structurally sound - and failing closed here would wedge the app
        // permanently after one crash, for a problem the user can only fix by restarting.
        let mut guard = state
            .ludusavi
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let ludusavi = guard.as_mut().ok_or_else(|| {
            "Config/manifest not loaded - run `ludusavi manifest update`, then restart".to_string()
        })?;
        let context = TaskContext {
            app: &worker,
            game: &game,
            cancel: &cancel,
        };
        body(ludusavi, &context)
    })
    .await;

    // Release the slot on every exit path - including a panic in the body, where the join
    // itself yields `Err`. This has to happen before that error can propagate, or a broken
    // task would leave the slot occupied and the UI refusing every future one forever.
    state.task.finish();

    let result = joined.map_err(|e| format!("task crashed: {e}"))?;

    match result {
        Ok(outcome) => Ok(outcome),
        // Cancellation resolves as a normal result with `cancelled: true`: nothing went
        // wrong, the user asked for it. There's no tally to report: a mid-operation cancel
        // aborts the `api.rs` call outright, so no `ApiOutput` exists to summarize. The
        // work that *was* completed isn't lost - a cancelled cloud copy is additive, and a
        // cancelled backup leaves its previous contents intact - it just isn't counted
        // here, so the frontend must not claim otherwise.
        Err(TaskError::Cancelled) => Ok(TaskOutcome {
            cancelled: true,
            ..Default::default()
        }),
        Err(TaskError::Failed(message)) => Err(message),
    }
}

/// Last 3 path segments, matching the frontend's own shortening: a game's file list can
/// run to hundreds of paths, and an error quoting all of them in full helps nobody.
fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|x| !x.is_empty()).collect();
    let tail = parts[parts.len().saturating_sub(3)..].join("/");
    match parts.len() > 3 {
        true => format!(".../{tail}"),
        false => tail,
    }
}

/// Flatten a save file or registry entry into the fields [`summarize_output`] counts.
fn classify_entry<'a>(
    path: &'a str,
    change: ScanChange,
    failed: bool,
    ignored: bool,
    error: Option<&SaveError>,
) -> (&'a str, bool, bool, bool, Option<String>) {
    (
        path,
        // Only count an entry as "changed" if something was actually written. A file that
        // already matched is `Same`, and reporting it as a change made a backup of an
        // unchanged game claim it had updated hundreds of files when it wrote none.
        change.is_changed(),
        failed,
        ignored,
        error.map(|e| e.message.clone()),
    )
}

/// Turn an `ApiOutput` from a backup/restore into something a log can render.
///
/// `api.rs` returns `Ok` even when individual files fail - the failures live in the
/// output - so this is the only place they're visible to the frontend. Previously the
/// Tauri layer returned just a file count and dropped all of it.
fn summarize_output(game: &str, output: &ApiOutput) -> TaskOutcome {
    let mut outcome = TaskOutcome::default();

    if let Some(errors) = &output.errors {
        if let Some(unknown) = &errors.unknown_games {
            for name in unknown {
                outcome.warnings.push(format!("\"{name}\" isn't a game Ludusavi recognizes"));
            }
        }
        if errors.cloud_conflict.is_some() {
            outcome
                .warnings
                .push("Cloud and local saves disagree; upload/download conflict was not resolved".to_string());
        }
        if errors.cloud_sync_failed.is_some() {
            outcome
                .warnings
                .push("Couldn't synchronize with the cloud after the operation".to_string());
        }
    }

    if let Some(overall) = &output.overall {
        outcome.bytes = overall.processed_bytes;
    }

    let Some(ApiGame::Operative {
        decision,
        change,
        files,
        registry,
        ..
    }) = output.games.get(game)
    else {
        if output.games.is_empty() {
            outcome
                .warnings
                .push("No save data was found for this game".to_string());
        }
        return outcome;
    };

    outcome.change = Some(format!("{change:?}"));

    // Files and registry entries carry the same change/failure/ignore fields, so classify
    // them into one shape rather than writing the tally twice.
    let entries = files
        .iter()
        .map(|(path, file)| {
            classify_entry(
                path,
                file.change,
                file.failed,
                file.ignored,
                file.error.as_ref(),
            )
        })
        .chain(registry.iter().map(|(path, item)| {
            classify_entry(
                path,
                item.change,
                item.failed,
                item.ignored,
                item.error.as_ref(),
            )
        }));

    let mut skipped = 0;
    let mut reported = 0;
    let mut total_failed = 0;
    for (path, changed, failed, ignored, reason) in entries {
        if failed {
            total_failed += 1;
            if reported < MAX_REPORTED_ERRORS {
                reported += 1;
                let reason = reason.unwrap_or_else(|| "unknown error".to_string());
                outcome.errors.push(format!("{}: {reason}", short_path(path)));
            }
        } else if ignored {
            skipped += 1;
        } else if changed {
            outcome.changes += 1;
        }
    }
    if total_failed > reported {
        outcome
            .errors
            .push(format!("...and {} more file(s)", total_failed - reported));
    }
    if skipped > 0 {
        outcome
            .warnings
            .push(format!("{skipped} file(s) excluded from this operation"));
    }
    if *decision == OperationStepDecision::Ignored {
        outcome
            .warnings
            .push("Nothing was processed - the game is disabled for this operation".to_string());
    }

    outcome
}

/// Push a single game's local backup to the cloud (additive - never deletes other games).
/// Takes a fresh local backup first, so the cloud gets the current save rather than
/// whatever was last backed up.
#[tauri::command]
async fn sync_push(app: tauri::AppHandle, game: String) -> Result<TaskOutcome, String> {
    run_task(app, TaskKind::Push, game, |ludusavi, context| {
        let mut on_progress = context.progress();
        let hooks = SyncHooks {
            on_phase: Some(&mut |phase| context.phase(phase)),
            on_progress: Some(&mut on_progress),
            cancel: Some(context.cancel.clone()),
        };
        ludusavi
            .sync_push_hooked(context.game, Finality::Final, hooks)
            .map(|result| TaskOutcome {
                changes: result.changes.len(),
                ..Default::default()
            })
            .map_err(TaskError::from)
    })
    .await
}

/// Pull a single game's backup from the cloud (additive), then restore it over the
/// current local save - destructive locally, by design.
#[tauri::command]
async fn sync_pull(app: tauri::AppHandle, game: String) -> Result<TaskOutcome, String> {
    run_task(app, TaskKind::Pull, game, |ludusavi, context| {
        let mut on_progress = context.progress();
        let hooks = SyncHooks {
            on_phase: Some(&mut |phase| context.phase(phase)),
            on_progress: Some(&mut on_progress),
            cancel: Some(context.cancel.clone()),
        };
        ludusavi
            .sync_pull_hooked(context.game, Finality::Final, hooks)
            .map(|result| TaskOutcome {
                changes: result.changes.len(),
                ..Default::default()
            })
            .map_err(TaskError::from)
    })
    .await
}

/// Cancel the in-flight backup/restore/push/pull, if any. Cheap and immediate: it only
/// flips the shared token, which the copy and transfer loops notice (and rclone, which
/// gets killed). See [`run_task`].
#[tauri::command]
async fn task_cancel(state: tauri::State<'_, AppState>) -> Result<(), String> {
    if state.task.cancel() {
        if let Some((kind, game)) = state.task.current() {
            log::info!("cancel requested for {} of {}", kind.label(), game);
        }
    }
    Ok(())
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
/// was found locally. `sync_pull` restores as part of the pull now, so this hitting
/// `WinePrefixNotFound` (or silently misdirecting if `scan.redirect_wine` is off) is a
/// real failure mode - useful for the UI to warn about before Pull is even clicked,
/// though it only warns rather than gating the button itself.
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
async fn backup_game(app: tauri::AppHandle, game: String) -> Result<TaskOutcome, String> {
    run_task(app, TaskKind::Backup, game, |ludusavi, context| {
        context.phase(SyncPhase::BackingUp);
        let output = ludusavi
            .back_up(parameters::BackUp {
                games: vec![context.game.to_string()],
                finality: Finality::Final,
                cancel: Some(context.cancel.clone()),
                ..Default::default()
            })
            .map_err(TaskError::from)?;
        Ok(summarize_output(context.game, &output))
    })
    .await
}

/// Restore one game's latest local backup over its current save - no cloud involved,
/// just undoing back to what's already sitting in backup storage. Destructive locally,
/// same as the restore step `sync_pull` runs after downloading.
#[tauri::command]
async fn restore_game(app: tauri::AppHandle, game: String) -> Result<TaskOutcome, String> {
    run_task(app, TaskKind::Restore, game, |ludusavi, context| {
        context.phase(SyncPhase::Restoring);
        let output = ludusavi
            .restore(parameters::Restore {
                games: vec![context.game.to_string()],
                finality: Finality::Final,
                cancel: Some(context.cancel.clone()),
                ..Default::default()
            })
            .map_err(TaskError::from)?;
        Ok(summarize_output(context.game, &output))
    })
    .await
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

    // Cancelled: the partial preview is meaningless. Return nothing; the UI has
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

    // Results are deliberately NOT persisted. A scan is a snapshot of what's on
    // disk right now; holding it across restarts would keep showing games whose
    // saves are long gone. Only the starred set (`sync.enabled_games`) survives a
    // restart, and each starred game's own file list is rescanned on demand by
    // `game_scan_entries` when its modal opens.
    Ok(results)
}

/// Cancel the in-flight [`scan_games`], if any. Cheap and immediate: it only flips
/// the shared flag; the scan's per-game steps notice and unwind.
#[tauri::command]
async fn cancel_scan(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.scan_cancel.store(true, Ordering::Relaxed);
    Ok(())
}

/// Current cloud remote/path/rclone status, for the settings screen.
#[tauri::command]
async fn cloud_status(state: tauri::State<'_, AppState>) -> Result<CloudStatus, String> {
    with_ludusavi(&state, |l| Ok(l.cloud_status()))
}

/// Configure Google Drive as the cloud remote.
///
/// This drives rclone's own OAuth flow (opens a browser, waits for approval) and can
/// take a while - or hang indefinitely if the user never finishes it. Takes an
/// `AppHandle` (not `State`) so the whole call can run inside `spawn_blocking` -
/// `State`'s lifetime is tied to this invocation and can't move into a `'static`
/// closure, but `AppHandle` can, and re-derives `AppState` via `.state()` once inside.
///
/// Deliberately only locks `AppState`'s shared `Ludusavi` mutex twice, briefly, around
/// the OAuth wait rather than for its whole duration (via
/// `begin_cloud_remote_google_drive`/`commit_cloud_remote` instead of the one-shot
/// `set_cloud_remote_google_drive`) - every other command (Scan included) locks that
/// same mutex, so holding it for the wait would stall all of them until the user
/// finishes approving in the browser, confirmed live as the cause of Scan silently
/// hanging on this project's own dev Steam Deck after a Google Drive connect was left
/// dangling.
///
/// Emits a `"cloud-auth-url"` event with the link as soon as rclone prints it, well
/// before this command returns - rclone's own browser auto-open isn't reliable (e.g. a
/// broken/missing default-browser association just does nothing, confirmed live on
/// this project's own dev Steam Deck), so the frontend should always display this link
/// for the user to open/paste manually rather than assume a browser popped up.
#[tauri::command]
async fn connect_google_drive(app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let (rclone, remote) = with_ludusavi(&state, |l| Ok(l.begin_cloud_remote_google_drive()))?;

        let event_app = app.clone();
        rclone
            .configure_remote_reporting_url(move |url| {
                let _ = event_app.emit("cloud-auth-url", url.to_string());
            })
            .map_err(|e| format!("{e:?}"))?;

        let state = app.state::<AppState>();
        with_ludusavi_mut(&state, |l| {
            l.commit_cloud_remote(remote);
            Ok(())
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

/// Open the containing folder for `path` in the system's file manager.
/// On Windows this tries `explorer /select,`, on macOS `open -R`, on Linux `xdg-open`.
#[tauri::command]
async fn open_in_file_manager(path: String) -> Result<(), String> {
    use std::path::PathBuf;

    let pb = PathBuf::from(path);
    let parent = pb.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));

    let res = tauri::async_runtime::spawn_blocking(move || {
        #[cfg(target_os = "windows")]
        {
            // explorer supports `/select,` to highlight a file
            let arg = format!("/select,{}", pb.to_string_lossy());
            std::process::Command::new("explorer").arg(arg).spawn().map_err(|e| e.to_string())?;
            Ok(())
        }
        #[cfg(target_os = "macos")]
        {
            // `open -R` reveals the file in Finder
            std::process::Command::new("open").arg("-R").arg(pb).spawn().map_err(|e| e.to_string())?;
            Ok(())
        }
        #[cfg(target_os = "linux")]
        {
            // Open the parent directory; defaults like xdg-open will use the user's
            // preferred file manager (Dolphin, Nautilus, etc.). Selecting a file is
            // not portable across all managers.
            std::process::Command::new("xdg-open").arg(parent).spawn().map_err(|e| e.to_string())?;
            Ok(())
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        {
            Err("Unsupported platform".to_string())
        }
    })
    .await
    .map_err(|e| e.to_string())?;

    res
}

/// One line of the core's own logging, forwarded to the webview's task log.
#[derive(Clone, Serialize)]
struct TaskLogEvent {
    game: String,
    /// `"info"`, `"warn"`, or `"error"` - the frontend maps these straight onto its own
    /// log levels.
    level: &'static str,
    message: String,
}

/// Forwards the core crate's `log` records into the running task's log in the webview.
///
/// Without this, everything the core says goes nowhere: the CLI installs flexi_logger
/// to a file (`src/main.rs`), but a library consumer - which is what this Tauri backend
/// is - installs nothing, so `log::warn!`/`error!` calls like "rclone failed", "no Wine
/// prefix found", or "redirect_wine is disabled" were silently dropped. Those are
/// exactly the lines that explain a task that otherwise just stops.
///
/// Only records from a live task are forwarded, and only at `Info` and above. `Info` is
/// the useful transcript of a backup/restore ("backed up: X -> Y", "already matches: X",
/// plus the warnings and errors explaining a file that couldn't be read); the truly
/// chatty levels are below that - `Debug` logs every skipped file, and `Trace` every scan
/// hit - so those stay off. The frontend caps the panel at 400 lines regardless, since a
/// Baldur's Gate 3-scale backup is a few hundred `Info` lines on its own.
struct TaskLogBridge {
    app: tauri::AppHandle,
}

/// The levels worth putting in front of a user: a transcript at `Info`, with warnings
/// and errors being the reason this exists. `Debug` logs every skipped file and `Trace`
/// every scan hit, which would bury the useful lines rather than help.
fn is_forwarded_level(level: log::Level) -> bool {
    level <= log::Level::Info
}

/// A `log::Level` as the frontend's own level names.
fn log_level_name(level: log::Level) -> &'static str {
    match level {
        log::Level::Error => "error",
        log::Level::Warn => "warn",
        _ => "info",
    }
}

impl log::Log for TaskLogBridge {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        is_forwarded_level(metadata.level())
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Forward the core's records (target `ludusavi`) plus this crate's own
        // (`desktop_lib`, e.g. the "cancel requested" line in `task_cancel`). Anything
        // else is from a dependency, and would be noise in a per-game log.
        let target = record.target();
        if !(target.starts_with("ludusavi") || target.starts_with("desktop_lib")) {
            return;
        }
        // Records outside a task have no game to attach to. `eprintln!` keeps them
        // available in a terminal run (`tauri dev`) even though the UI can't show them.
        // `try_state` rather than `state`: this runs on the logger's thread, and a missing
        // state during teardown should not be a panic.
        let Some((kind, game)) = self
            .app
            .try_state::<AppState>()
            .and_then(|state| state.task.current())
        else {
            if record.level() >= log::Level::Warn {
                eprintln!("[{}] {}", record.level(), record.args());
            }
            return;
        };

        let _ = self.app.emit(
            "task-log",
            TaskLogEvent {
                game: game.clone(),
                level: log_level_name(record.level()),
                message: record.args().to_string(),
            },
        );
        // A short prefix makes `tauri dev`'s terminal readable when a task is running.
        eprintln!("[{} {}] {}", kind.label(), game, record.args());
    }

    fn flush(&self) {}
}

/// Install the log bridge. `log::set_boxed_logger` can only succeed once per process;
/// this app has no other logger, so failure would only mean something else got here first.
fn install_log_bridge(app: &tauri::AppHandle) {
    let bridge = TaskLogBridge { app: app.clone() };
    if log::set_boxed_logger(Box::new(bridge)).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // WebKitGTK fails to initialize under gamescope's nested Wayland compositor
    // on Steam Deck: "Could not create default EGL display: EGL_BAD_PARAMETER.
    // Aborting..." followed by the WebKitWebProcess (renderer) crashing
    // outright, leaving just the native window chrome with a blank content
    // area. Confirmed LIVE on real Deck hardware (strace + coredumpctl) that
    // none of these vars alone fix it - GDK_BACKEND=x11 in particular does
    // *not* help despite desktop/scripts/dev.sh's `tauri dev` fallback using
    // it for a superficially similar symptom, because the AppImage's own
    // linuxdeploy-plugin-gtk AppRun hook already forces GDK_BACKEND=x11
    // unconditionally before this binary ever runs, and the crash still
    // happened every time regardless. Root cause turned out to be a WebKitGTK
    // regression, not an env var this app controls: versions after 2.44.x
    // break exactly this way on Wayland-adjacent compositors (see
    // .github/workflows/desktop-build.yaml "Pin libwebkit2gtk" for the actual
    // fix - pinning the build's WebKitGTK to a pre-regression version).
    // Keeping these vars set is still a reasonable default (cheap, harmless,
    // helps unrelated Wayland/GPU-driver combos on desktop Linux) but they
    // are not what fixes Steam Deck. Don't overwrite a var the user already
    // set themselves.
    for (key, value) in [
        ("GDK_BACKEND", "x11"),
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

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            ludusavi: Mutex::new(load_ludusavi()),
            scan_cancel: Arc::new(AtomicBool::new(false)),
            task: TaskSlot::new(),
        })
        .invoke_handler(tauri::generate_handler![
            sync_push,
            sync_pull,
            sync_status,
            sync_status_batch,
            wine_prefix_check,
            enabled_games,
            search_games,
            game_cover,
            set_custom_cover,
            clear_custom_cover,
            game_scan_entries,
            toggle_game_path,
            set_group_ignored,
            set_game_enabled,
            backup_game,
            restore_game,
            task_cancel,
            scan_games,
            cancel_scan,
            cloud_status,
            connect_google_drive,
            disconnect_cloud,
            set_cloud_path,
            set_cloud_synchronize,
            set_rclone_path
            , open_in_file_manager
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // After `build`, so the bridge has a handle to emit through and the managed state
    // (which it reads the active task from) already exists.
    install_log_bridge(app.handle());

    app.run(|_app, _event| {});
}

#[cfg(test)]
mod tests {
    use super::*;
    use ludusavi::report::{ApiDump, ApiErrors, ApiFile, ApiRegistry};
    use ludusavi::scan::{OperationStatus, ScanChange};

    // ---- fixtures ---------------------------------------------------------------
    //
    // `ApiFile`/`ApiRegistry` aren't `Clone` (they're serialized straight to the
    // frontend), so fixtures are built once by value rather than copied.

    macro_rules! file_map {
        ($($path:literal => $file:expr),* $(,)?) => {
            vec![$(($path.to_string(), $file)),*]
        };
    }

    macro_rules! registry_map {
        ($($path:literal => $entry:expr),* $(,)?) => {
            vec![$(($path.to_string(), $entry)),*]
        };
    }

    fn file() -> ApiFile {
        ApiFile::default()
    }

    fn failed_file(message: &str) -> ApiFile {
        ApiFile {
            failed: true,
            error: Some(SaveError {
                message: message.to_string(),
            }),
            ..Default::default()
        }
    }

    fn ignored_file() -> ApiFile {
        ApiFile {
            ignored: true,
            ..Default::default()
        }
    }

    fn unchanged_file() -> ApiFile {
        ApiFile {
            change: ScanChange::Same,
            ..Default::default()
        }
    }

    fn failed_registry(message: &str) -> ApiRegistry {
        ApiRegistry {
            failed: true,
            error: Some(SaveError {
                message: message.to_string(),
            }),
            ..Default::default()
        }
    }

    /// An `ApiOutput` shaped like a real one-game backup: entries are keyed by the path
    /// they were found at, exactly as `api.rs` builds them.
    fn operative_output(
        game: &str,
        decision: OperationStepDecision,
        files: Vec<(String, ApiFile)>,
        registry: Vec<(String, ApiRegistry)>,
    ) -> ApiOutput {
        ApiOutput {
            games: [(
                game.to_string(),
                ApiGame::Operative {
                    decision,
                    change: ScanChange::New,
                    files: files.into_iter().collect(),
                    registry: registry.into_iter().collect(),
                    dump: ApiDump::default(),
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        }
    }

    fn contains(haystack: &[String], needle: &str) -> bool {
        haystack.iter().any(|line| line.contains(needle))
    }

    // ---- TaskSlot: the one-at-a-time policy -------------------------------------

    #[test]
    fn slot_starts_empty() {
        let slot = TaskSlot::new();
        assert_eq!(slot.current(), None);
    }

    #[test]
    fn begin_reports_the_running_task() {
        let slot = TaskSlot::new();
        let cancel = slot.begin(TaskKind::Push, "Baldur's Gate 3".to_string()).expect("free slot");

        assert!(!cancel.is_cancelled(), "a fresh task isn't already cancelled");
        assert_eq!(slot.current().map(|(kind, _)| kind), Some(TaskKind::Push));
        assert_eq!(
            slot.current().map(|(_, game)| game),
            Some("Baldur's Gate 3".to_string())
        );
    }

    #[test]
    fn begin_refuses_a_second_task_and_names_the_one_in_the_way() {
        let slot = TaskSlot::new();
        slot.begin(TaskKind::Pull, "Hades".to_string()).expect("free slot");

        // The message has to name the running task, or the UI just says "busy" and the
        // user has no idea what they're waiting for (or which game to cancel).
        let err = slot
            .begin(TaskKind::Backup, "Stardew Valley".to_string())
            .expect_err("a second task must be refused");
        assert!(err.contains("Pull"), "{err}");
        assert!(err.contains("Hades"), "{err}");
        assert!(!err.contains("Stardew Valley"), "{err}");

        // The refused task must not have displaced the running one.
        assert_eq!(slot.current().map(|(_, game)| game), Some("Hades".to_string()));
    }

    #[test]
    fn finish_frees_the_slot_for_the_next_task() {
        let slot = TaskSlot::new();
        slot.begin(TaskKind::Backup, "Hades".to_string()).expect("free slot");
        slot.finish();

        assert_eq!(slot.current(), None);
        slot.begin(TaskKind::Restore, "Hades".to_string()).expect("slot was released");
    }

    #[test]
    fn finish_is_idempotent() {
        let slot = TaskSlot::new();
        slot.begin(TaskKind::Backup, "Hades".to_string()).expect("free slot");
        slot.finish();
        slot.finish();
        assert!(slot.begin(TaskKind::Backup, "Hades".to_string()).is_ok());
    }

    #[test]
    fn cancel_only_touches_the_running_task() {
        let slot = TaskSlot::new();
        let first = slot.begin(TaskKind::Backup, "Hades".to_string()).expect("free slot");
        assert!(slot.cancel());
        assert!(first.is_cancelled(), "the running task sees the cancel");

        slot.finish();
        // A later task isn't affected by the previous one's cancel.
        let second = slot.begin(TaskKind::Push, "Hades".to_string()).expect("free slot");
        assert!(!second.is_cancelled());
    }

    #[test]
    fn cancel_with_nothing_running_is_a_no_op() {
        let slot = TaskSlot::new();
        assert!(!slot.cancel());
        assert_eq!(slot.current(), None);
    }

    // ---- Labels the frontend depends on -----------------------------------------

    #[test]
    fn task_kind_labels_match_the_frontend() {
        assert_eq!(TaskKind::Backup.label(), "Backup");
        assert_eq!(TaskKind::Restore.label(), "Restore");
        assert_eq!(TaskKind::Push.label(), "Push");
        assert_eq!(TaskKind::Pull.label(), "Pull");
    }

    #[test]
    fn task_kind_serializes_as_the_frontend_snake_case() {
        // The frontend keys its label tables by exactly these strings.
        let json = serde_json::to_string(&[TaskKind::Backup, TaskKind::Restore, TaskKind::Push, TaskKind::Pull])
            .expect("serializes");
        assert_eq!(json, r#"["backup","restore","push","pull"]"#);
    }

    #[test]
    fn phase_names_match_the_frontend() {
        // `tasks.ts`'s `PHASE_LABELS` is keyed by these; a rename here without one there
        // silently degrades the UI to "running…" with no idea which step it is on.
        assert_eq!(phase_name(SyncPhase::BackingUp), "backing-up");
        assert_eq!(phase_name(SyncPhase::Uploading), "uploading");
        assert_eq!(phase_name(SyncPhase::Downloading), "downloading");
        assert_eq!(phase_name(SyncPhase::Restoring), "restoring");
    }

    // ---- The log bridge's level policy ------------------------------------------

    #[test]
    fn only_info_and_above_reach_the_task_log() {
        assert!(is_forwarded_level(log::Level::Error));
        assert!(is_forwarded_level(log::Level::Warn));
        assert!(is_forwarded_level(log::Level::Info));
        // `Debug` logs every skipped file and `Trace` every scan hit.
        assert!(!is_forwarded_level(log::Level::Debug));
        assert!(!is_forwarded_level(log::Level::Trace));
    }

    #[test]
    fn levels_map_to_the_frontend_names() {
        assert_eq!(log_level_name(log::Level::Error), "error");
        assert_eq!(log_level_name(log::Level::Warn), "warn");
        assert_eq!(log_level_name(log::Level::Info), "info");
    }

    // ---- Error mapping ----------------------------------------------------------

    #[test]
    fn cancellation_is_not_an_error() {
        // The whole point of a distinct variant: a cancelled task resolves as `Ok` with
        // `cancelled: true` rather than as a failure the user has to dismiss.
        let TaskError::Cancelled = TaskError::from(Error::Cancelled) else {
            panic!("Error::Cancelled must map to TaskError::Cancelled");
        };
        assert_eq!(String::from(TaskError::Cancelled), "cancelled");
    }

    #[test]
    fn other_errors_become_the_translator_s_message() {
        let TaskError::Failed(message) = TaskError::from(Error::WinePrefixNotFound {
            game: "Baldur's Gate 3".to_string(),
            backup_prefix: "/home/deck/.steam/steam/steamapps/compatdata/1086940/pfx".to_string(),
        }) else {
            panic!("expected a failure");
        };
        // The translator's wording, not a Debug dump of the enum - and it has to name the
        // game, since that's what the user is looking at.
        assert!(message.contains("Baldur's Gate 3"), "{message}");
        assert!(!message.contains("WinePrefixNotFound"), "{message}");
    }

    #[test]
    fn plain_messages_become_failures() {
        let TaskError::Failed(message) = TaskError::from("config/manifest not loaded".to_string()) else {
            panic!("expected a failure");
        };
        assert_eq!(message, "config/manifest not loaded");
    }

    // ---- short_path -------------------------------------------------------------

    #[test]
    fn short_paths_are_left_alone() {
        assert_eq!(short_path("slot1.save"), "slot1.save");
        assert_eq!(short_path("profile0/slot1.save"), "profile0/slot1.save");
        assert_eq!(short_path("a/b/c.save"), "a/b/c.save");
    }

    #[test]
    fn long_paths_keep_their_last_three_segments() {
        assert_eq!(
            short_path("/home/deck/.steam/steam/steamapps/compatdata/1086940/pfx/drive_c/Saves/slot1.save"),
            ".../drive_c/Saves/slot1.save"
        );
        // Windows-style separators, since a Windows install's paths come through as-is.
        assert_eq!(
            short_path(r"C:\Users\deck\Saved Games\Hades\run.sav"),
            ".../Saved Games/Hades/run.sav"
        );
    }

    #[test]
    fn redundant_separators_do_not_become_empty_segments() {
        assert_eq!(short_path("//a///b//c/d"), ".../b/c/d");
        assert_eq!(short_path(""), "");
    }

    // ---- summarize_output -------------------------------------------------------

    #[test]
    fn an_empty_result_says_no_save_data_was_found() {
        // A game that isn't installed, or a name Ludusavi doesn't recognize: not a
        // failure, but the log shouldn't be empty about it.
        let outcome = summarize_output("Nonexistent", &ApiOutput::default());
        assert_eq!(outcome.changes, 0);
        assert!(contains(&outcome.warnings, "No save data was found"));
    }

    #[test]
    fn a_clean_backup_counts_every_file_it_wrote() {
        let output = operative_output(
            "Hades",
            OperationStepDecision::Processed,
            file_map! {
                "/saves/a.save" => file(),
                "/saves/b.save" => file(),
                "/saves/c.save" => ignored_file(),
            },
            vec![],
        );

        let outcome = summarize_output("Hades", &output);
        assert_eq!(outcome.changes, 2, "the ignored file isn't a change");
        assert!(outcome.errors.is_empty());
        assert!(contains(&outcome.warnings, "1 file(s) excluded"));
        assert_eq!(outcome.change.as_deref(), Some("New"));
        assert!(!outcome.cancelled);
    }

    #[test]
    fn a_file_that_already_matched_isnt_counted_as_a_change() {
        // Re-running a backup of an unchanged game used to report every file as changed,
        // claiming it had updated hundreds of files when it wrote none.
        let output = operative_output(
            "Hades",
            OperationStepDecision::Processed,
            file_map! {
                "/saves/a.save" => unchanged_file(),
                "/saves/b.save" => unchanged_file(),
                "/saves/c.save" => file(),
            },
            vec![],
        );

        let outcome = summarize_output("Hades", &output);
        assert_eq!(outcome.changes, 1);
        assert!(outcome.errors.is_empty());
    }

    #[test]
    fn bytes_come_from_the_overall_operation_status() {
        let mut output = operative_output(
            "Hades",
            OperationStepDecision::Processed,
            file_map! { "/saves/a.save" => file() },
            vec![],
        );
        output.overall = Some(OperationStatus {
            processed_bytes: 2048,
            ..Default::default()
        });

        assert_eq!(summarize_output("Hades", &output).bytes, 2048);
    }

    #[test]
    fn a_failed_file_is_reported_not_swallowed() {
        // This is the bug the structured outcome fixed: `api.rs` returns `Ok` here, so
        // anything not read out of the output is a failure the user never hears about.
        let output = operative_output(
            "Hades",
            OperationStepDecision::Processed,
            file_map! { "/home/deck/Saves/profile0/run.sav" => failed_file("permission denied") },
            vec![],
        );

        let outcome = summarize_output("Hades", &output);
        assert_eq!(outcome.changes, 0);
        assert_eq!(outcome.errors.len(), 1);
        assert_eq!(outcome.errors[0], ".../Saves/profile0/run.sav: permission denied");
    }

    #[test]
    fn failed_files_are_capped_with_a_count_of_the_rest() {
        // A game with 400 unreadable files should show a handful, not a wall of text.
        let files: Vec<(String, ApiFile)> = (0..20)
            .map(|i| (format!("/saves/file{i}.sav"), failed_file("permission denied")))
            .collect();
        let output = operative_output("Hades", OperationStepDecision::Processed, files, vec![]);

        let outcome = summarize_output("Hades", &output);
        assert_eq!(outcome.errors.len(), MAX_REPORTED_ERRORS + 1);
        assert_eq!(outcome.errors[MAX_REPORTED_ERRORS], "...and 12 more file(s)");
    }

    #[test]
    fn a_failure_without_a_message_still_reports_something() {
        let output = operative_output(
            "Hades",
            OperationStepDecision::Processed,
            file_map! { "/home/deck/Saves/profile0/a.sav" => ApiFile { failed: true, ..Default::default() } },
            vec![],
        );

        assert_eq!(
            summarize_output("Hades", &output).errors,
            [".../Saves/profile0/a.sav: unknown error"]
        );
    }

    #[test]
    fn registry_entries_are_counted_alongside_files() {
        // On Windows a game can be all registry and no files; the tally has to include it
        // rather than silently reporting zero.
        let output = operative_output(
            "Hades",
            OperationStepDecision::Processed,
            file_map! { "/saves/a.sav" => file() },
            registry_map! {
                r"HKCU\Software\Hades" => failed_registry("access is denied"),
                r"HKCU\Software\Hades\Other" => ApiRegistry::default(),
            },
        );

        let outcome = summarize_output("Hades", &output);
        assert_eq!(outcome.changes, 2, "one file plus the healthy registry entry");
        assert!(contains(&outcome.errors, "access is denied"));
    }

    #[test]
    fn a_disabled_operation_says_so_instead_of_looking_like_no_op_work() {
        let output = operative_output("Hades", OperationStepDecision::Ignored, vec![], vec![]);

        assert!(contains(&summarize_output("Hades", &output).warnings, "disabled"));
    }

    #[test]
    fn an_unknown_game_is_a_warning_naming_it() {
        let mut output = operative_output("Hades", OperationStepDecision::Processed, vec![], vec![]);
        output.errors = Some(ApiErrors {
            unknown_games: Some(vec!["Hades 2".to_string()]),
            ..Default::default()
        });

        assert!(contains(&summarize_output("Hades", &output).warnings, "Hades 2"));
    }

    #[test]
    fn a_cloud_sync_failure_after_the_operation_is_a_warning() {
        // The operation itself succeeded; the bulk mirror-sync that runs afterwards
        // didn't. It has to be visible without being reported as a failed backup.
        let mut output = operative_output(
            "Hades",
            OperationStepDecision::Processed,
            file_map! { "/saves/a.sav" => file() },
            vec![],
        );
        output.errors = Some(ApiErrors {
            cloud_sync_failed: Some(Default::default()),
            ..Default::default()
        });

        let outcome = summarize_output("Hades", &output);
        assert!(contains(&outcome.warnings, "Couldn't synchronize with the cloud"));
        assert_eq!(outcome.changes, 1);
        assert!(outcome.errors.is_empty());
    }

    #[test]
    fn a_game_missing_from_the_output_is_not_reported_as_missing_save_data() {
        // An empty map means "nothing found", but a map that simply doesn't have this
        // game is a different thing and shouldn't claim otherwise.
        let output = operative_output("Other", OperationStepDecision::Processed, vec![], vec![]);

        let outcome = summarize_output("Hades", &output);
        assert!(outcome.errors.is_empty());
        assert!(!contains(&outcome.warnings, "No save data was found"));
    }
}
