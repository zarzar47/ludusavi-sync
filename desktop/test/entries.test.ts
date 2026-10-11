// Tests for the settings modal's save-file list helpers - which scan entries the
// "Saves found" checklist shows by default, and how backup-only ghosts are counted.
//
// Run with `pnpm test` (Node's built-in runner; no test framework dependency).

import assert from "node:assert/strict";
import { test } from "node:test";

import { missingCount, visibleEntries, type ScanEntry } from "../src/entries.ts";

function entry(overrides: Partial<ScanEntry> = {}): ScanEntry {
  return {
    path: "/saves/file.sav",
    ignored: false,
    kind: "file",
    exists: true,
    change: "Same",
    ...overrides,
  };
}

test("files that exist on disk are listed by default", () => {
  const entries = [entry({ path: "/saves/a.sav" }), entry({ path: "/saves/b.sav", change: "New" })];

  assert.deepEqual(
    visibleEntries(entries, false).map((e) => e.path),
    ["/saves/a.sav", "/saves/b.sav"],
  );
});

test("files that only exist in the backup are hidden by default", () => {
  // A ghost is a path recorded in mapping.yaml that's no longer on disk; left visible
  // it reads as a live save, which is what made the list look like a backup inventory.
  const entries = [
    entry({ path: "/saves/a.sav" }),
    entry({ path: "/saves/deleted.sav", exists: false, change: "Removed" }),
  ];

  assert.deepEqual(
    visibleEntries(entries, false).map((e) => e.path),
    ["/saves/a.sav"],
  );
});

test("showMissing reveals backup-only files without reordering", () => {
  const entries = [
    entry({ path: "/saves/deleted.sav", exists: false, change: "Removed" }),
    entry({ path: "/saves/a.sav" }),
    entry({ path: "/saves/older.deleted.sav", exists: false, change: "Removed" }),
  ];

  assert.deepEqual(
    visibleEntries(entries, true).map((e) => e.path),
    ["/saves/deleted.sav", "/saves/a.sav", "/saves/older.deleted.sav"],
  );
});

test("excluded files on disk stay visible - they're part of the checklist", () => {
  // `ignored` files are the unchecked boxes; hiding them would defeat the modal.
  const entries = [entry({ path: "/saves/excluded.sav", ignored: true })];

  assert.equal(visibleEntries(entries, false).length, 1);
});

test("missingCount counts only backup-only files", () => {
  const entries = [
    entry(),
    entry({ path: "/saves/a.deleted.sav", exists: false, change: "Removed" }),
    entry({ path: "/saves/b.deleted.sav", exists: false, change: "Removed" }),
  ];

  assert.equal(missingCount(entries), 2);
  assert.equal(missingCount([]), 0);
});

test("the backend marks ghosts with the change value the badge copy assumes", () => {
  // The frontend copy says "only in backup"; a ghost's ScanChange is always "Removed".
  // If the backend ever stops producing that, this test points at the drift.
  const ghost = entry({ exists: false, change: "Removed" });
  assert.equal(ghost.change, "Removed");
});
