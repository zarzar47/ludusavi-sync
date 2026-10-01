import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { CloudSettings } from "./CloudSettings";
import { GameSettingsModal } from "./GameSettingsModal";
import {
  TASK_LABELS,
  appendCapped,
  logLine,
  outcomeLines,
  outcomeResult,
  type RunningTask,
  type TaskKind,
  type TaskLogEvent,
  type TaskLogLine,
  type TaskOutcome,
  type TaskPhase,
  type TaskPhaseEvent,
  type TaskProgressEvent,
  type TaskResult,
} from "./tasks";
import "./App.css";

// Mirrors resource::sync_state::GameSyncEntry.
export interface GameSyncEntry {
  last_push: string;
  device: string;
  mapping_path: string;
  prefixes?: Record<string, string>;
}

// Mirrors src-tauri's ScanResult.
interface ScanResult {
  name: string;
  file_count: number;
  registry_count: number;
  change: string;
}

// Coarse "how long ago" for a sync badge tooltip - doesn't need to be precise,
// just enough to tell "just synced" from "ages ago" at a glance.
function relativeTime(iso: string): string {
  const hours = Math.round((Date.now() - new Date(iso).getTime()) / 3_600_000);
  if (hours < 1) return "just now";
  if (hours < 24) return `${hours}h ago`;
  return `${Math.round(hours / 24)}d ago`;
}

type Page = "sync" | "settings";

// Drop a key from a per-game record without mutating it.
function omit<T>(record: Record<string, T>, key: string): Record<string, T> {
  const { [key]: _removed, ...rest } = record;
  return rest;
}

function App() {
  const [page, setPage] = useState<Page>("sync");
  const [scanning, setScanning] = useState(false);

  return (
    <main className="container">
      <header className="app-header">
        <h1>Ludusavi Sync</h1>
        {page === "sync" ? (
          <button
            className="icon-button"
            aria-label="Settings"
            title="Settings"
            disabled={scanning}
            onClick={() => setPage("settings")}
          >
            ⚙
          </button>
        ) : (
          <button className="icon-button" aria-label="Back" title="Back" onClick={() => setPage("sync")}>
            ←
          </button>
        )}
      </header>

      {page === "sync" ? (
        <SyncScreen scanning={scanning} onScanningChange={setScanning} />
      ) : (
        <CloudSettings />
      )}
    </main>
  );
}

function SyncScreen({
  scanning,
  onScanningChange,
}: {
  scanning: boolean;
  onScanningChange: (scanning: boolean) => void;
}) {
  const [enabledGames, setEnabledGames] = useState<string[]>([]);
  // Mirror of `enabledGames` for async continuations, which would otherwise read the
  // value from the render that started the task rather than the current one.
  const enabledRef = useRef<string[]>([]);
  const [query, setQuery] = useState("");
  const [searchResults, setSearchResults] = useState<string[]>([]);
  const [scanResults, setScanResults] = useState<Record<string, ScanResult>>({});
  const [statuses, setStatuses] = useState<Record<string, GameSyncEntry | null>>({});
  // Passive, always-visible sync badge per starred card - separate from `statuses`
  // (which is only populated on-demand by the "Status" button/a push/pull).
  const [syncBadges, setSyncBadges] = useState<Record<string, GameSyncEntry>>({});
  // The task (backup/restore/push/pull) currently running for each game. The backend
  // allows only one at a time, so there is at most one entry - keyed by game anyway, so
  // every event it streams needs no "is this still mine?" check.
  const [running, setRunning] = useState<Record<string, RunningTask>>({});
  // The same map, readable synchronously. `running` is what renders; this is what guards,
  // so a second click in the same tick can't slip past the one-task-at-a-time check.
  const runningRef = useRef<Record<string, RunningTask>>({});
  // Task log per game, kept after the task ends so the modal still shows what happened.
  // Seeded when a task starts and appended to by the backend's `task-log` events, which
  // can land before the invoke resolves - hence a separate store from `running`.
  const [logs, setLogs] = useState<Record<string, TaskLogLine[]>>({});
  // How the last task for each game ended, for the card badge.
  const [results, setResults] = useState<Record<string, TaskResult>>({});
  const [error, setError] = useState<string | null>(null);
  // Low-res Steam-style cover art per game, `data:` URIs from the backend.
  // `null` means "asked, Steam has none" - distinct from "haven't asked yet" (absent
  // key), so `coverRequested` below is the source of truth for what's in flight.
  const [covers, setCovers] = useState<Record<string, string | null>>({});
  const coverRequested = useRef<Set<string>>(new Set());
  // Game whose per-game settings modal (cover art + save-file checklist) is open,
  // opened by right-click or long-press on its card. Null = closed.
  const [settingsGame, setSettingsGame] = useState<string | null>(null);
  const longPressTimer = useRef<number | null>(null);

  function startLongPress(e: React.PointerEvent, game: string) {
    // Don't hijack a long-press on the star/action buttons themselves.
    if ((e.target as HTMLElement).closest("button")) return;
    longPressTimer.current = window.setTimeout(() => setSettingsGame(game), 550);
  }

  function cancelLongPress() {
    if (longPressTimer.current !== null) {
      clearTimeout(longPressTimer.current);
      longPressTimer.current = null;
    }
  }

  // Bumped on every new scan and on cancel, so a stale scan's late resolution
  // (and its partial results) can be ignored instead of clobbering the UI.
  const scanIdRef = useRef(0);

  function refreshEnabled() {
    invoke<string[]>("enabled_games")
      .then(setEnabledGames)
      .catch((e) => setError(String(e)));
  }

  // One `settings.config` read for every starred game, batched - see
  // `sync_status_batch` on `Ludusavi`. Games with no cloud record simply have no key.
  function refreshSyncBadges(games: string[]) {
    if (games.length === 0) {
      setSyncBadges({});
      return;
    }
    invoke<Record<string, GameSyncEntry>>("sync_status_batch", { games })
      .then(setSyncBadges)
      .catch((e) => setError(String(e)));
  }

  // Task events from the backend, for whichever game's task is running. Registered once
  // and never torn down: they are tagged with the game, and a late event for a task that
  // already finished is simply appended to that game's log.
  useEffect(() => {
    const subscriptions = [
      listen<TaskPhaseEvent>("task-phase", ({ payload }) => {
        setRunning((prev) => {
          const task = prev[payload.game];
          if (!task) return prev;
          return { ...prev, [payload.game]: { ...task, phase: payload.phase as TaskPhase } };
        });
      }),
      listen<TaskProgressEvent>("task-progress", ({ payload }) => {
        setRunning((prev) => {
          const task = prev[payload.game];
          if (!task) return prev;
          return {
            ...prev,
            [payload.game]: { ...task, progress: { current: payload.current, total: payload.total } },
          };
        });
      }),
      listen<TaskLogEvent>("task-log", ({ payload }) => {
        setLogs((prev) => ({
          ...prev,
          [payload.game]: appendCapped(prev[payload.game] ?? [], [logLine(payload.level, payload.message)]),
        }));
      }),
    ];
    // Each `listen` resolves asynchronously; by cleanup time they're all registered.
    let disposed = false;
    const unlisteners: UnlistenFn[] = [];
    subscriptions.forEach((subscription) => subscription.then((unlisten) => {
      if (disposed) unlisten();
      else unlisteners.push(unlisten);
    }));
    return () => {
      disposed = true;
      unlisteners.forEach((unlisten) => unlisten());
    };
  }, []);

  // Only the starred games persist across restarts. Scan results are a snapshot
  // of what was on disk when you pressed Scan and deliberately aren't kept - so a
  // game whose saves you since deleted drops off the next launch instead of
  // lingering as a ghost. Each starred game's own file list is rescanned fresh
  // whenever its modal opens.
  useEffect(() => {
    refreshEnabled();
  }, []);

  // Re-fetch badges whenever the starred set changes (mount, star/unstar).
  useEffect(() => {
    enabledRef.current = enabledGames;
    refreshSyncBadges(enabledGames);
  }, [enabledGames]);

  // Only search once there's a query - avoids fetching/rendering the whole
  // (potentially 19,000+ game) manifest by default. With no query, the list
  // below is just whatever's already enabled (plus anything found by Scan).
  useEffect(() => {
    if (query.trim() === "") {
      setSearchResults([]);
      return;
    }
    const handle = setTimeout(() => {
      invoke<string[]>("search_games", { query })
        .then(setSearchResults)
        .catch((e) => setError(String(e)));
    }, 150);
    return () => clearTimeout(handle);
  }, [query]);

  async function scan() {
    const id = ++scanIdRef.current;
    onScanningChange(true);
    setError(null);
    try {
      const results = await invoke<ScanResult[]>("scan_games");
      if (id !== scanIdRef.current) return;
      setScanResults(Object.fromEntries(results.map((r) => [r.name, r])));
    } catch (e) {
      if (id === scanIdRef.current) setError(String(e));
    } finally {
      if (id === scanIdRef.current) onScanningChange(false);
    }
  }

  async function cancelScan() {
    scanIdRef.current++; // invalidate the in-flight scan so its results are dropped
    onScanningChange(false);
    try {
      await invoke("cancel_scan");
    } catch (e) {
      setError(String(e));
    }
  }

  async function toggleEnabled(game: string, enabled: boolean) {
    try {
      await invoke("set_game_enabled", { game, enabled });
      refreshEnabled();
    } catch (e) {
      setError(String(e));
    }
  }

  async function checkStatus(game: string) {
    try {
      const entry = await invoke<GameSyncEntry | null>("sync_status", { game });
      setStatuses((prev) => ({ ...prev, [game]: entry }));
    } catch (e) {
      setError(String(e));
    }
  }

  // One entry point for all four operations: start the task, stream whatever the
  // backend emits into this game's log, then record the verdict.
  //
  // Progress/phase/log events arrive through the listeners registered once on mount -
  // they carry the game name, so there's nothing to attach and detach per invocation,
  // and a line emitted while the invoke is still settling isn't lost.
  async function runTask(game: string, kind: TaskKind, command: string) {
    // Guard on a ref, not `running`: two clicks in the same tick both see the same
    // rendered state, so a state-based check lets both through. The backend would refuse
    // the second, but its `finally` would then clear the *first* task's entry and unlock
    // the modal while the real task is still running.
    if (Object.keys(runningRef.current).length > 0) {
      setError("Another operation is still running. Wait for it to finish, or cancel it.");
      return;
    }
    setError(null);
    setResults((prev) => omit(prev, game));
    setLogs((prev) => ({ ...prev, [game]: [logLine("info", `${TASK_LABELS[kind]} started.`)] }));
    runningRef.current = { ...runningRef.current, [game]: { kind, phase: null, progress: null } };
    setRunning(runningRef.current);
    try {
      const outcome = await invoke<TaskOutcome>(command, { game });
      appendLines(game, outcomeLines(kind, outcome));
      setResults((prev) => ({ ...prev, [game]: outcomeResult(kind, outcome) }));
      // A push writes `settings.config`'s last_push record; a pull may have restored, so
      // the badge on the card is stale either way. Read the starred list from a ref: the
      // user may have starred or unstarred games while the task ran.
      if (kind === "push" || kind === "pull") {
        await checkStatus(game);
        refreshSyncBadges(enabledRef.current);
      }
    } catch (e) {
      const message = String(e);
      appendLines(game, [logLine("error", message)]);
      setResults((prev) => ({ ...prev, [game]: { kind, level: "error", summary: message } }));
    } finally {
      runningRef.current = omit(runningRef.current, game);
      setRunning(runningRef.current);
    }
  }

  function appendLines(game: string, lines: TaskLogLine[]) {
    if (lines.length === 0) return;
    setLogs((prev) => ({ ...prev, [game]: appendCapped(prev[game] ?? [], lines) }));
  }

  async function cancelTask(game: string) {
    try {
      await invoke("task_cancel");
      appendLines(game, [logLine("warn", "Cancelling...")]);
    } catch (e) {
      appendLines(game, [logLine("error", String(e))]);
    }
  }

  function backup(game: string) {
    return runTask(game, "backup", "backup_game");
  }

  // Restores the latest local backup over the current save - no cloud involved,
  // just undoing back to what's already in backup storage. Destructive, confirm first.
  function restore(game: string) {
    if (
      !window.confirm(`Restore "${game}" from its latest local backup? This will overwrite your current local save.`)
    ) {
      return;
    }
    return runTask(game, "restore", "restore_game");
  }

  function push(game: string) {
    return runTask(game, "push", "sync_push");
  }

  // Pull restores the downloaded save over the current local one - destructive, unlike
  // push (which only takes a backup and uploads). Confirm before overwriting.
  function pull(game: string) {
    if (
      !window.confirm(
        `Pull "${game}" from the cloud and restore it? This will overwrite your current local save.`,
      )
    ) {
      return;
    }
    return runTask(game, "pull", "sync_pull");
  }

  // One unified, deduplicated list: everything enabled (starred), plus whatever
  // the search or this session's Scan turned up that isn't already in that set.
  // Starred games always sort first, so starring/unstarring is what controls
  // both "is this a push/pull target" and "is this near the top".
  const enabledSet = new Set(enabledGames);
  const names = new Set<string>([...enabledGames, ...searchResults, ...Object.keys(scanResults)]);
  const rows = [...names].sort((a, b) => {
    const aEnabled = enabledSet.has(a);
    const bEnabled = enabledSet.has(b);
    if (aEnabled !== bEnabled) return aEnabled ? -1 : 1;
    return a.localeCompare(b);
  });

  // Fetch each card's cover art once, the first time it shows up in `rows` - not on
  // every render, and not all 19k titles up front (only what's actually displayed).
  useEffect(() => {
    for (const game of rows) {
      if (coverRequested.current.has(game)) continue;
      coverRequested.current.add(game);
      invoke<string | null>("game_cover", { game })
        .then((url) => setCovers((prev) => ({ ...prev, [game]: url })))
        .catch(() => setCovers((prev) => ({ ...prev, [game]: null })));
    }
  }, [rows]);

  function renderCard(game: string) {
    const enabled = enabledSet.has(game);
    const scanned = scanResults[game];
    const cover = covers[game];
    return (
      <div
        className="game-card"
        key={game}
        onContextMenu={(e) => {
          e.preventDefault();
          setSettingsGame(game);
        }}
        onPointerDown={(e) => startLongPress(e, game)}
        onPointerUp={cancelLongPress}
        onPointerLeave={cancelLongPress}
      >
        <div className="game-card-cover" role="button" title="Game settings" onClick={() => setSettingsGame(game)}>
          {cover ? (
            <img src={cover} alt="" loading="lazy" />
          ) : (
            <div className="game-card-cover-fallback">{game.charAt(0).toUpperCase()}</div>
          )}
          {enabled && (
            <span
              className={`sync-badge ${syncBadges[game] ? "sync-badge-synced" : "sync-badge-none"}`}
              title={
                syncBadges[game]
                  ? `synced ${relativeTime(syncBadges[game].last_push)} from ${syncBadges[game].device}`
                  : "never synced"
              }
            />
          )}
          {renderTaskBadge(game, running[game], results[game])}
          <button
            className="star-button card-star"
            aria-label={enabled ? "Remove from sync" : "Add to sync"}
            title={enabled ? "Remove from sync" : "Add to sync"}
            onClick={(e) => {
              e.stopPropagation();
              toggleEnabled(game, !enabled);
            }}
          >
            {enabled ? "★" : "☆"}
          </button>
        </div>
        <div className="game-card-body">
          <span className="game-name" title={game}>
            {game}
          </span>
          {!enabled && scanned && (
            <span className="game-status">
              found: {scanned.file_count} file(s), {scanned.change}
            </span>
          )}
        </div>
      </div>
    );
  }

  // A running task takes over the badge (the log itself is only in the modal, which is
  // closed for most of the session); once it finishes the badge keeps the verdict, so a
  // failure isn't lost when the modal is closed.
  function renderTaskBadge(game: string, task: RunningTask | undefined, result: TaskResult | undefined) {
    if (task) {
      return (
        <span className="task-badge task-badge-running" title={`${TASK_LABELS[task.kind]} running - open for details`}>
          <span className="spinner spinner-small" />
        </span>
      );
    }
    if (!result) return null;
    const glyph = { info: "✓", warn: "!", error: "✕" }[result.level];
    return (
      <span
        className={`task-badge task-badge-${result.level}`}
        title={result.summary}
        onClick={(e) => {
          e.stopPropagation();
          setSettingsGame(game);
        }}
      >
        {glyph}
      </span>
    );
  }

  const starredRows = rows.filter((game) => enabledSet.has(game));
  const unstarredRows = rows.filter((game) => !enabledSet.has(game));

  return (
    <>
      {error && <p className="error-text">{error}</p>}

      <div className="search-row">
        <input
          className="search-field"
          value={query}
          onChange={(e) => setQuery(e.currentTarget.value)}
          placeholder="Search for a game to add..."
        />
        <button disabled={scanning} onClick={scan} title="Scan your configured roots for installed games">
          {scanning ? "Scanning..." : "Scan"}
        </button>
      </div>

      {scanning && (
        <div className="scanning-indicator">
          <div className="spinner" />
          <span>Scanning for games…</span>
          <button onClick={cancelScan} title="Stop the scan">
            Cancel
          </button>
        </div>
      )}

      {rows.length === 0 && (
        <p>
          {query.trim() === ""
            ? "No games enabled yet. Search above, or hit Scan to find installed games."
            : "No matches."}
        </p>
      )}

      {starredRows.length > 0 && <div className="game-grid">{starredRows.map(renderCard)}</div>}

      {starredRows.length > 0 && unstarredRows.length > 0 && (
        <div className="grid-divider">Not starred</div>
      )}

      {unstarredRows.length > 0 && <div className="game-grid">{unstarredRows.map(renderCard)}</div>}

      {settingsGame && (
        <GameSettingsModal
          game={settingsGame}
          cover={covers[settingsGame]}
          onCoverChange={(cover) => setCovers((prev) => ({ ...prev, [settingsGame]: cover }))}
          onClose={() => setSettingsGame(null)}
          enabled={enabledSet.has(settingsGame)}
          task={running[settingsGame] ?? null}
          entry={statuses[settingsGame]}
          log={logs[settingsGame] ?? []}
          onBackup={() => backup(settingsGame)}
          onRestore={() => restore(settingsGame)}
          onPush={() => push(settingsGame)}
          onPull={() => pull(settingsGame)}
          onCancel={() => cancelTask(settingsGame)}
          onCheckStatus={() => checkStatus(settingsGame)}
        />
      )}
    </>
  );
}

export default App;
