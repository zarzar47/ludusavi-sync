import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";

// Mirrors src-tauri's ScanEntry.
interface ScanEntry {
  path: string;
  ignored: boolean;
  kind: "file" | "registry";
}

// Mirrors src-tauri's WinePrefixCheck.
interface WinePrefixCheck {
  backup_prefixes: string[];
  local_prefixes: string[];
  registered: Record<string, string>;
}

// Last 3 path segments, e.g. ".../profile0/slot01.save" - the full path is rarely
// useful once a game has more than a handful of files (Baldur's Gate 3-scale saves),
// since every entry shares the same 5+ segment prefix. Full path stays in the title
// tooltip for anyone who needs it.
function shortPath(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean);
  const tail = parts.slice(-3).join("/");
  return parts.length > 3 ? `.../${tail}` : tail;
}

// Directory a file lives in (path minus its last segment), used to group entries -
// with hundreds of files, "select all in this folder" does far more work than
// clicking checkboxes one at a time.
function parentDir(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts.slice(0, -1).join("/") || "/";
}

// Opened by right-click or long-press on a game card (App.tsx). Shows the game's
// cover art (with a picker to override it) and the save files/registry entries a
// scan found for it, each with a checkbox to include/exclude from backup - the same
// include/exclude model as upstream ludusavi's per-game file list. Entries are
// grouped by folder and paths are shortened, since some games (Baldur's Gate 3,
// Ultrakill with a big save folder, ...) have far too many files to scan as a flat
// list of full paths.
export function GameSettingsModal({
  game,
  cover,
  onCoverChange,
  onClose,
}: {
  game: string;
  cover: string | null | undefined;
  onCoverChange: (cover: string | null) => void;
  onClose: () => void;
}) {
  const [entries, setEntries] = useState<ScanEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [coverBusy, setCoverBusy] = useState(false);
  const [filter, setFilter] = useState("");
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [wineCheck, setWineCheck] = useState<WinePrefixCheck | null>(null);

  useEffect(() => {
    invoke<ScanEntry[]>("game_scan_entries", { game })
      .then(setEntries)
      .catch((e) => setError(String(e)));
  }, [game]);

  // Non-critical diagnostic - a restore-hazard warning, not a blocking check, so
  // failures are swallowed rather than surfaced as a modal-wide error.
  useEffect(() => {
    invoke<WinePrefixCheck>("wine_prefix_check", { game })
      .then(setWineCheck)
      .catch(() => setWineCheck(null));
  }, [game]);

  useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") onClose();
    }
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [onClose]);

  async function changeCover() {
    const picked = await open({
      multiple: false,
      filters: [{ name: "Images", extensions: ["png", "jpg", "jpeg", "webp", "gif", "bmp"] }],
    });
    if (!picked || typeof picked !== "string") return;
    setCoverBusy(true);
    setError(null);
    try {
      const dataUri = await invoke<string>("set_custom_cover", { game, sourcePath: picked });
      onCoverChange(dataUri);
    } catch (e) {
      setError(String(e));
    } finally {
      setCoverBusy(false);
    }
  }

  async function resetCover() {
    setCoverBusy(true);
    setError(null);
    try {
      await invoke("clear_custom_cover", { game });
      const dataUri = await invoke<string | null>("game_cover", { game });
      onCoverChange(dataUri);
    } catch (e) {
      setError(String(e));
    } finally {
      setCoverBusy(false);
    }
  }

  async function toggleEntry(entry: ScanEntry) {
    try {
      const ignored = await invoke<boolean>("toggle_game_path", {
        game,
        path: entry.path,
        kind: entry.kind,
      });
      setEntries((prev) => prev && prev.map((e) => (e === entry ? { ...e, ignored } : e)));
    } catch (e) {
      setError(String(e));
    }
  }

  async function setGroupIgnored(groupEntries: ScanEntry[], ignored: boolean) {
    // A folder mixes files and registry entries only in principle; in practice it's
    // one or the other, so send them as separate batches to match the backend's
    // per-kind toggle tables.
    const byKind = new Map<string, string[]>();
    for (const e of groupEntries) {
      if (e.ignored === ignored) continue;
      byKind.set(e.kind, [...(byKind.get(e.kind) ?? []), e.path]);
    }
    try {
      await Promise.all(
        [...byKind.entries()].map(([kind, paths]) => invoke("set_group_ignored", { game, paths, kind, ignored })),
      );
      const changed = new Set(groupEntries.map((e) => e.path));
      setEntries((prev) => prev && prev.map((e) => (changed.has(e.path) ? { ...e, ignored } : e)));
    } catch (e) {
      setError(String(e));
    }
  }

  function toggleCollapsed(dir: string) {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(dir)) next.delete(dir);
      else next.add(dir);
      return next;
    });
  }

  const groups = useMemo(() => {
    if (!entries) return [];
    const needle = filter.trim().toLowerCase();
    const filtered = needle ? entries.filter((e) => e.path.toLowerCase().includes(needle)) : entries;
    const byDir = new Map<string, ScanEntry[]>();
    for (const entry of filtered) {
      const dir = parentDir(entry.path);
      byDir.set(dir, [...(byDir.get(dir) ?? []), entry]);
    }
    return [...byDir.entries()].sort(([a], [b]) => a.localeCompare(b));
  }, [entries, filter]);

  const totalCount = entries?.length ?? 0;
  const shownCount = groups.reduce((sum, [, list]) => sum + list.length, 0);

  return (
    <div className="modal-overlay" onClick={onClose}>
      <div className="modal-panel" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <h2 title={game}>{game}</h2>
          <button className="icon-button modal-close" aria-label="Close" title="Close" onClick={onClose}>
            ✕
          </button>
        </div>

        {error && <p className="error-text">{error}</p>}

        {wineCheck && wineCheck.backup_prefixes.length > 0 && wineCheck.local_prefixes.length === 0 && (
          <p className="warning-text">
            This save was backed up from a Wine/Proton prefix that wasn't found on this
            machine. A CLI restore would fail — launch the game once to recreate its
            prefix, or set a custom wine_prefix.
          </p>
        )}

        <div className="modal-cover-row">
          <div className="game-card-cover modal-cover">
            {cover ? (
              <img src={cover} alt="" />
            ) : (
              <div className="game-card-cover-fallback">{game.charAt(0).toUpperCase()}</div>
            )}
          </div>
          <div className="modal-cover-actions">
            <button disabled={coverBusy} onClick={changeCover}>
              Change art…
            </button>
            <button disabled={coverBusy} onClick={resetCover}>
              Reset to default
            </button>
          </div>
        </div>

        <div className="modal-saves-header">
          <h3>
            Saves found{totalCount > 0 && ` (${shownCount === totalCount ? totalCount : `${shownCount}/${totalCount}`})`}
          </h3>
          {totalCount > 8 && (
            <input
              className="search-field entry-filter"
              value={filter}
              onChange={(e) => setFilter(e.currentTarget.value)}
              placeholder="Filter by path…"
            />
          )}
        </div>

        {entries === null && <p>Scanning…</p>}
        {entries !== null && totalCount === 0 && <p>No save files found for this game.</p>}
        {entries !== null && totalCount > 0 && groups.length === 0 && <p>No files match "{filter}".</p>}

        {groups.length > 0 && (
          <div className="entry-groups">
            {groups.map(([dir, groupEntries]) => {
              const allIncluded = groupEntries.every((e) => !e.ignored);
              const noneIncluded = groupEntries.every((e) => e.ignored);
              const isCollapsed = collapsed.has(dir);
              return (
                <div className="entry-group" key={dir}>
                  <div className="entry-group-header">
                    <button
                      className="entry-group-toggle"
                      onClick={() => toggleCollapsed(dir)}
                      aria-label={isCollapsed ? "Expand" : "Collapse"}
                    >
                      {isCollapsed ? "▸" : "▾"}
                    </button>
                    <GroupCheckbox
                      allIncluded={allIncluded}
                      noneIncluded={noneIncluded}
                      onChange={() => setGroupIgnored(groupEntries, allIncluded)}
                    />
                    <span className="entry-group-dir" title={dir}>
                      {shortPath(dir) || "/"}
                    </span>
                    <span className="entry-group-count">{groupEntries.length}</span>
                  </div>
                  {!isCollapsed && (
                    <ul className="entry-list">
                      {groupEntries.map((entry) => (
                        <li key={`${entry.kind}:${entry.path}`} className="entry-row">
                          <label>
                            <input type="checkbox" checked={!entry.ignored} onChange={() => toggleEntry(entry)} />
                            {entry.kind === "registry" && <span className="entry-kind">registry</span>}
                            <span className="entry-path" title={entry.path}>
                              {entry.path.split(/[\\/]/).filter(Boolean).pop()}
                            </span>
                          </label>
                        </li>
                      ))}
                    </ul>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}

// Tri-state "select all in this folder" checkbox: checked when every entry is
// included, unchecked when none are, indeterminate (the dash) when it's a mix -
// `indeterminate` has no JSX prop, so it's set imperatively via ref.
function GroupCheckbox({
  allIncluded,
  noneIncluded,
  onChange,
}: {
  allIncluded: boolean;
  noneIncluded: boolean;
  onChange: () => void;
}) {
  const ref = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (ref.current) ref.current.indeterminate = !allIncluded && !noneIncluded;
  }, [allIncluded, noneIncluded]);

  return (
    <input
      ref={ref}
      type="checkbox"
      checked={allIncluded}
      onChange={onChange}
      title={allIncluded ? "Exclude all in this folder" : "Include all in this folder"}
    />
  );
}
