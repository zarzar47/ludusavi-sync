// Shapes and helpers for the per-game save-file checklist in the settings modal.
// Mirrors `ScanEntry` in `desktop/src-tauri/src/lib.rs` - keep the two in sync.

// One save file or registry entry found by the preview scan.
export interface ScanEntry {
  path: string;
  // Whether it's currently excluded from backup/sync (the checkbox's unchecked state).
  ignored: boolean;
  kind: "file" | "registry";
  // Whether the file is present at `path` right now. False means the entry is a ghost
  // from the last backup's mapping.yaml: backed up once, since deleted from disk.
  exists: boolean;
  // Debug-formatted ScanChange, e.g. "New", "Different", "Removed".
  change: string;
}

// How many entries no longer exist on disk. They're still reachable behind the modal's
// "only in backup" toggle, so a recoverable deleted save never becomes invisible.
export function missingCount(entries: ScanEntry[]): number {
  return entries.filter((entry) => !entry.exists).length;
}

// The entries the modal should list. Files that only exist in the backup are hidden by
// default: the list is meant to show what the game's saves look like where they're
// actually stored, and a backup-only ghost reads as a live file otherwise. `showMissing`
// reveals them (dimmed and badged), keeping their original order.
export function visibleEntries(entries: ScanEntry[], showMissing: boolean): ScanEntry[] {
  return showMissing ? entries : entries.filter((entry) => entry.exists);
}
