// Shared shapes and helpers for the per-game task pipeline (backup/restore/push/pull):
// the backend streams `task-phase`/`task-progress`/`task-log` events while a task runs,
// and answers with a `TaskOutcome` when it finishes. Everything here mirrors
// `desktop/src-tauri/src/lib.rs` - keep the two in sync.

// Which backend command a task came from.
export type TaskKind = "backup" | "restore" | "push" | "pull";

// Which step of a push/pull is currently running - a push is two steps (take a backup,
// then upload) and a pull three (download, then restore), so "working…" alone is
// ambiguous about what's actually going on.
export type TaskPhase = "backing-up" | "uploading" | "downloading" | "restoring";

export type TaskLevel = "info" | "warn" | "error";

export const TASK_LABELS: Record<TaskKind, string> = {
  backup: "Backup",
  restore: "Restore",
  push: "Push",
  pull: "Pull",
};

export const PHASE_LABELS: Record<TaskPhase, string> = {
  "backing-up": "Backing up",
  uploading: "Uploading to cloud",
  downloading: "Downloading from cloud",
  restoring: "Restoring save",
};

// Mirrors src-tauri's TaskOutcome.
export interface TaskOutcome {
  cancelled: boolean;
  changes: number;
  bytes: number;
  change: string | null;
  warnings: string[];
  errors: string[];
}

// Mirrors src-tauri's TaskProgressEvent.
export interface TaskProgressEvent {
  game: string;
  current: number;
  total: number;
}

// Mirrors src-tauri's TaskPhaseEvent.
export interface TaskPhaseEvent {
  game: string;
  phase: string;
}

// Mirrors src-tauri's TaskLogEvent.
export interface TaskLogEvent {
  game: string;
  level: TaskLevel;
  message: string;
}

// Live byte progress of a push/pull's rclone transfer, for one game.
export interface GameProgress {
  current: number;
  total: number;
}

// One rendered line of a game's task log. Not an extension of `TaskLogEvent`: the game is
// the key it is stored under, and the frontend's own lines (task started, finished) have
// no game of their own to carry.
export interface TaskLogLine {
  level: TaskLevel;
  message: string;
  at: number;
}

// A task currently running for one game.
export interface RunningTask {
  kind: TaskKind;
  phase: TaskPhase | null;
  progress: GameProgress | null;
}

// How a task ended, kept after it finishes so the card can show it (and the modal's log
// stays scrolled to the summary) without re-running anything.
export interface TaskResult {
  kind: TaskKind;
  level: TaskLevel;
  summary: string;
}

// A log with hundreds of backup/restore lines would grow without bound in memory and
// make the panel unusable to scroll; the tail is what matters.
export const MAX_LOG_LINES = 400;

// Append to a game's log, keeping only the tail. Without this, a Baldur's Gate 3-scale
// backup (one `Info` line per file) grows the panel and React state without bound.
export function appendCapped(existing: TaskLogLine[], lines: TaskLogLine[]): TaskLogLine[] {
  if (lines.length === 0) return existing;
  const merged = [...existing, ...lines];
  return merged.length > MAX_LOG_LINES ? merged.slice(merged.length - MAX_LOG_LINES) : merged;
}

export function logLine(level: TaskLevel, message: string): TaskLogLine {
  return { level, message, at: Date.now() };
}

export function clockTime(at: number): string {
  return new Date(at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

export function formatBytes(bytes: number): string {
  if (!isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  const exp = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / Math.pow(1024, exp);
  return `${value.toFixed(value >= 100 || exp === 0 ? 0 : 1)} ${units[exp]}`;
}

// Turn a finished task's outcome into log lines. Kept apart from the invoke call so a
// cancelled or partially-failed task still reads the same way in the log as in the
// summary the card shows.
export function outcomeLines(kind: TaskKind, outcome: TaskOutcome): TaskLogLine[] {
  const lines: TaskLogLine[] = [];

  if (outcome.cancelled) {
    lines.push(logLine("warn", `${TASK_LABELS[kind]} stopped early - you cancelled it.`));
    return lines;
  }

  const verb = { backup: "backed up", restore: "restored", push: "pushed", pull: "pulled" }[kind];
  lines.push(
    logLine(
      outcome.errors.length > 0 ? "warn" : "info",
      `${TASK_LABELS[kind]} finished: ${verb} ${describeChanges(kind, outcome)}${formatChange(outcome)}`,
    ),
  );
  for (const warning of outcome.warnings) lines.push(logLine("warn", warning));
  for (const error of outcome.errors) lines.push(logLine("error", error));
  return lines;
}

// The one-line verdict shown on the game card.
export function outcomeResult(kind: TaskKind, outcome: TaskOutcome): TaskResult {
  if (outcome.cancelled) {
    return { kind, level: "warn", summary: `${TASK_LABELS[kind]} cancelled` };
  }
  const detail = describeChanges(kind, outcome);
  if (outcome.errors.length > 0) {
    return {
      kind,
      level: "error",
      summary: `${TASK_LABELS[kind]} finished with ${outcome.errors.length} problem(s): ${detail}`,
    };
  }
  if (outcome.warnings.length > 0) {
    return { kind, level: "warn", summary: `${TASK_LABELS[kind]} finished: ${detail}` };
  }
  return { kind, level: "info", summary: `${TASK_LABELS[kind]} finished: ${detail}` };
}

function describeChanges(kind: TaskKind, outcome: TaskOutcome): string {
  const noun = kind === "backup" || kind === "restore" ? "file" : "object";
  const parts: string[] = [];
  if (outcome.changes > 0) parts.push(`${outcome.changes} ${noun}(s)`);
  if (outcome.bytes > 0) parts.push(formatBytes(outcome.bytes));
  return parts.length > 0 ? parts.join(", ") : "nothing to do";
}

function formatChange(outcome: TaskOutcome): string {
  return outcome.change && outcome.change !== "Same" ? ` (${outcome.change})` : "";
}
