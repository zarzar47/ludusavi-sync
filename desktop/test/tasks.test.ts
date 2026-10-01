// Tests for the frontend half of the task pipeline - the pure functions in `tasks.ts`
// that turn a backend `TaskOutcome` into what the log and the card badge show.
//
// Run with `pnpm test` (Node's built-in runner; no test framework dependency).

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  PHASE_LABELS,
  TASK_LABELS,
  appendCapped,
  formatBytes,
  logLine,
  outcomeLines,
  outcomeResult,
  type TaskOutcome,
} from "../src/tasks.ts";

function outcome(overrides: Partial<TaskOutcome> = {}): TaskOutcome {
  return {
    cancelled: false,
    changes: 0,
    bytes: 0,
    change: null,
    warnings: [],
    errors: [],
    ...overrides,
  };
}

// ---- formatBytes -----------------------------------------------------------------

test("formatBytes reads as bytes, not as a raw count", () => {
  assert.equal(formatBytes(0), "0 B");
  assert.equal(formatBytes(-1), "0 B", "a negative size is nonsense, not a crash");
  assert.equal(formatBytes(512), "512 B");
  assert.equal(formatBytes(1024), "1.0 KiB");
  assert.equal(formatBytes(1536), "1.5 KiB");
  assert.equal(formatBytes(1024 * 1024), "1.0 MiB");
  assert.equal(formatBytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
});

test("formatBytes doesn't run off the end of the unit list", () => {
  // Beyond TiB it keeps using TiB rather than producing "1024.0 Tib" or NaN.
  assert.equal(formatBytes(1024 ** 5), "1024 TiB");
});

// ---- outcome -> log lines --------------------------------------------------------

test("a clean task logs one summary line", () => {
  const lines = outcomeLines("backup", outcome({ changes: 12, bytes: 2048, change: "New" }));

  assert.equal(lines.length, 1);
  assert.equal(lines[0].level, "info");
  assert.match(lines[0].message, /Backup finished/);
  assert.match(lines[0].message, /12 file\(s\), 2\.0 KiB/);
  assert.match(lines[0].message, /\(New\)/, "the game's change state is useful context");
});

test("a task with nothing to do says so", () => {
  const lines = outcomeLines("push", outcome());

  assert.equal(lines.length, 1);
  assert.match(lines[0].message, /nothing to do/);
  assert.doesNotMatch(lines[0].message, /0 file/, "an empty tally isn't worth a count");
});

test("an unchanged game doesn't have its state repeated", () => {
  const lines = outcomeLines("backup", outcome({ change: "Same" }));

  assert.doesNotMatch(lines[0].message, /\(Same\)/);
});

test("file problems are logged at error level after the summary", () => {
  const lines = outcomeLines(
    "restore",
    outcome({
      changes: 3,
      errors: [".../a.sav: permission denied", ".../b.sav: not found"],
    }),
  );

  // The task still succeeded overall, so the summary is a warning, not an error - a
  // partial restore isn't a total failure.
  assert.equal(lines[0].level, "warn");
  assert.deepEqual(
    lines.slice(1).map((line) => line.level),
    ["error", "error"],
  );
  assert.match(lines[1].message, /permission denied/);
  assert.match(lines[2].message, /not found/);
});

test("warnings are logged after the summary and before errors", () => {
  const lines = outcomeLines(
    "backup",
    outcome({ warnings: ["3 file(s) excluded from this operation"], errors: [".../a.sav: denied"] }),
  );

  // The summary is a warning here because the operation also has errors; the point of
  // the ordering is that the file problems come last, where the eye lands.
  assert.deepEqual(
    lines.map((line) => line.level),
    ["warn", "warn", "error"],
  );
  assert.match(lines[1].message, /3 file\(s\) excluded/);
  assert.match(lines[2].message, /denied/);
});

test("a cancelled task is reported as stopped, not failed", () => {
  const lines = outcomeLines("pull", outcome({ cancelled: true, changes: 4, bytes: 1000 }));

  assert.equal(lines[0].level, "warn");
  assert.match(lines[0].message, /Pull stopped early/);
  assert.equal(lines.some((line) => line.level === "error"), false);
});

test("a cancelled task claims no tally, because the backend can't report one", () => {
  // A mid-operation cancel aborts the `api.rs` call, so there's no output to summarize.
  // The work already done isn't lost, but it isn't counted either, so the log must not
  // imply a count exists.
  const lines = outcomeLines("pull", outcome({ cancelled: true, changes: 4, bytes: 1000 }));

  assert.equal(lines.length, 1);
  assert.equal(lines.some((line) => /Kept what had already been done/.test(line.message)), false);
});

test("a cancel that did nothing isn't padded with a 'kept' line", () => {
  const lines = outcomeLines("backup", outcome({ cancelled: true }));

  assert.equal(lines.length, 1);
  assert.match(lines[0].message, /stopped early/);
});

test("every logged line is timestamped", () => {
  const before = Date.now();
  const lines = outcomeLines("push", outcome({ changes: 1 }));

  for (const line of lines) {
    assert.ok(line.at >= before, "a line has to be timestamped when it's created");
  }
});

// ---- outcome -> card badge -------------------------------------------------------

test("a clean task's badge is informational", () => {
  const result = outcomeResult("push", outcome({ changes: 5, bytes: 10 }));

  assert.equal(result.level, "info");
  assert.equal(result.kind, "push");
  assert.match(result.summary, /Push finished: 5 object\(s\)/);
});

test("a task with warnings badges as a warning, not a failure", () => {
  const result = outcomeResult("backup", outcome({ warnings: ["nothing was processed"] }));

  assert.equal(result.level, "warn");
});

test("a task with file problems badges as a failure and counts them", () => {
  const result = outcomeResult("backup", outcome({ errors: ["a: denied", "b: denied"] }));

  assert.equal(result.level, "error");
  assert.match(result.summary, /2 problem\(s\)/);
});

test("a cancelled task badges as a warning", () => {
  assert.equal(outcomeResult("pull", outcome({ cancelled: true })).level, "warn");
});

test("the badge verb is the task that ran", () => {
  // A push transfers objects and a restore writes files; calling both "files" would be
  // wrong on the card.
  assert.match(outcomeResult("push", outcome({ changes: 2 })).summary, /2 object\(s\)/);
  assert.match(outcomeResult("restore", outcome({ changes: 2 })).summary, /2 file\(s\)/);
});

// ---- the log cap -----------------------------------------------------------------

test("a short log is left as it is, in order", () => {
  const existing = [logLine("info", "one"), logLine("warn", "two")];
  const merged = appendCapped(existing, [logLine("error", "three")]);

  assert.deepEqual(
    merged.map((line) => line.message),
    ["one", "two", "three"],
  );
});

test("an append of nothing doesn't churn the log", () => {
  const existing = [logLine("info", "one")];

  assert.equal(appendCapped(existing, []), existing);
});

test("a flood of lines keeps the newest and drops the oldest", () => {
  // A Baldur's Gate 3-scale backup logs one line per file; the panel must not grow
  // without bound, and the newest line is the one that matters.
  let log = Array.from({ length: 1000 }, (_, i) => logLine("info", `line ${i}`));
  log = appendCapped(log, [logLine("error", "the important one")]);

  assert.equal(log.length, 400, "capped at MAX_LOG_LINES");
  assert.equal(log[log.length - 1].message, "the important one");
  assert.equal(log[0].message, "line 601", "the oldest lines are the ones dropped");
});

// ---- labels shared with the backend ----------------------------------------------

test("the frontend knows a label for every task and phase", () => {
  assert.deepEqual(Object.keys(TASK_LABELS).sort(), ["backup", "pull", "push", "restore"]);
  assert.deepEqual(Object.keys(PHASE_LABELS).sort(), [
    "backing-up",
    "downloading",
    "restoring",
    "uploading",
  ]);
});

// Nothing stops someone renaming `SyncPhase::BackingUp` to produce a different string on
// the Rust side: the phase would still be emitted, the UI would just quietly lose its
// label and fall back to "running…". So check the two sides against each other.
const RUST_SOURCE = new URL("../src-tauri/src/lib.rs", import.meta.url);
const rust = readFileSync(RUST_SOURCE, "utf8");

test("the backend emits the phase names the frontend labels", () => {
  const variants: Record<string, string> = {
    BackingUp: "backing-up",
    Uploading: "uploading",
    Downloading: "downloading",
    Restoring: "restoring",
  };

  for (const [variant, name] of Object.entries(variants)) {
    assert.ok(
      new RegExp(`${variant}\\s*=>\\s*"${name}"`).test(rust),
      `src-tauri/src/lib.rs no longer maps SyncPhase::${variant} to "${name}", which is what PHASE_LABELS keys on`,
    );
  }
});

test("the backend uses the task labels the frontend shows", () => {
  for (const [variant, label] of Object.entries({ Backup: "Backup", Restore: "Restore", Push: "Push", Pull: "Pull" })) {
    assert.ok(
      new RegExp(`Self::${variant}\\s*=>\\s*"${label}"`).test(rust),
      `src-tauri/src/lib.rs no longer labels TaskKind::${variant} as "${label}"`,
    );
  }
});
