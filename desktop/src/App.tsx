import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { CloudSettings } from "./CloudSettings";
import { GameSettingsModal } from "./GameSettingsModal";
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

// Mirrors src-tauri's SyncProgressEvent.
interface SyncProgressEvent {
  game: string;
  current: number;
  total: number;
}

// Live byte progress of a push/pull for one game.
export interface GameProgress {
  current: number;
  total: number;
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
  const [discoveredGames, setDiscoveredGames] = useState<string[]>([]);
  const [query, setQuery] = useState("");
  const [searchResults, setSearchResults] = useState<string[]>([]);
  const [scanResults, setScanResults] = useState<Record<string, ScanResult>>({});
  const [statuses, setStatuses] = useState<Record<string, GameSyncEntry | null>>({});
  // Passive, always-visible sync badge per starred card - separate from `statuses`
  // (which is only populated on-demand by the "Status" button/a push/pull).
  const [syncBadges, setSyncBadges] = useState<Record<string, GameSyncEntry>>({});
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [progress, setProgress] = useState<Record<string, GameProgress | null>>({});
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

  function refreshDiscovered() {
    invoke<string[]>("discovered_games")
      .then(setDiscoveredGames)
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

  // Load both the starred games (always shown) and the persisted scan results,
  // so a restart doesn't force a manual re-scan.
  useEffect(() => {
    refreshEnabled();
    refreshDiscovered();
  }, []);

  // Re-fetch badges whenever the starred set changes (mount, star/unstar).
  useEffect(() => {
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
      refreshDiscovered();
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

  // Backup/restore have no per-file progress hook from the backend (unlike
  // push/pull, which stream rclone's own byte progress) - `total: 0` renders
  // the modal's indeterminate bar, so there's at least a "still working" cue
  // instead of the button just hanging with no feedback.
  async function backup(game: string) {
    setBusy(game);
    setError(null);
    setProgress((prev) => ({ ...prev, [game]: { current: 0, total: 0 } }));
    try {
      await invoke<number>("backup_game", { game });
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
      setProgress((prev) => ({ ...prev, [game]: null }));
    }
  }

  async function restore(game: string) {
    // Restores the latest local backup over the current save - no cloud involved,
    // just undoing back to what's already in backup storage. Destructive, confirm first.
    if (!window.confirm(`Restore "${game}" from its latest local backup? This will overwrite your current local save.`)) {
      return;
    }
    setBusy(game);
    setError(null);
    setProgress((prev) => ({ ...prev, [game]: { current: 0, total: 0 } }));
    try {
      await invoke<number>("restore_game", { game });
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
      setProgress((prev) => ({ ...prev, [game]: null }));
    }
  }

  // Run a push or pull, streaming the backend's "sync-progress" events into the
  // matching game's bar. The event listener is torn down when the invoke settles.
  async function syncTransfer(game: string, op: "sync_push" | "sync_pull") {
    setBusy(game);
    setError(null);
    setProgress((prev) => ({ ...prev, [game]: { current: 0, total: 0 } }));
    const unlisten = await listen<SyncProgressEvent>("sync-progress", (e) => {
      if (e.payload.game !== game) return;
      setProgress((prev) => ({ ...prev, [game]: { current: e.payload.current, total: e.payload.total } }));
    });
    try {
      await invoke<number>(op, { game, preview: false });
      await checkStatus(game);
      refreshSyncBadges(enabledGames);
    } catch (e) {
      setError(String(e));
    } finally {
      unlisten();
      setBusy(null);
      setProgress((prev) => ({ ...prev, [game]: null }));
    }
  }

  async function push(game: string) {
    await syncTransfer(game, "sync_push");
  }

  async function pull(game: string) {
    // Pull restores the downloaded save over the current local one - destructive,
    // unlike push (which only takes a backup and uploads). Confirm before overwriting.
    if (!window.confirm(`Pull "${game}" from the cloud and restore it? This will overwrite your current local save.`)) {
      return;
    }
    await syncTransfer(game, "sync_pull");
  }

  // One unified, deduplicated list: everything enabled (starred), plus whatever
  // the search or a Scan turned up that isn't already in that set. Starred games
  // always sort first, so starring/unstarring is what controls both "is this a
  // push/pull target" and "is this near the top".
  const enabledSet = new Set(enabledGames);
  const names = new Set<string>([
    ...enabledGames,
    ...discoveredGames,
    ...searchResults,
    ...Object.keys(scanResults),
  ]);
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
          busy={busy === settingsGame}
          entry={statuses[settingsGame]}
          progress={busy === settingsGame ? progress[settingsGame] ?? null : null}
          onBackup={() => backup(settingsGame)}
          onRestore={() => restore(settingsGame)}
          onPush={() => push(settingsGame)}
          onPull={() => pull(settingsGame)}
          onCheckStatus={() => checkStatus(settingsGame)}
        />
      )}
    </>
  );
}

export default App;
