import { Database } from "bun:sqlite";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  rmdirSync,
  rmSync,
  statSync,
} from "node:fs";
import os from "node:os";
import path from "node:path";

import { AppError, createServiceToken, ExitCode } from "../../engine/index.ts";
import type { ServiceProvider } from "../../engine/services.ts";
import { createId } from "./ids.ts";
import { nextFireAt } from "./triggers.ts";
import type { Trigger } from "./triggers.ts";

export type ScheduleStatus = "active" | "paused";
// Both kinds live in the schedules table so the daemon has one claim path.
// A one-shot job is a row the daemon fires exactly once and then removes;
// every schedule-facing read filters it out.
export type ScheduleKind = "schedule" | "once";
// How a gate opens: 'output' needs exit 0 and stdout, 'exit' only exit 0.
export type GateMode = "output" | "exit";
export type RunStatus =
  | "queued"
  | "running"
  | "clean"
  | "succeeded"
  | "failed"
  | "gate_failed"
  | "timed_out"
  | "canceled"
  | "interrupted"
  | "skipped"
  | "missed";
export type RunTrigger = "scheduled" | "manual" | "once";

// How late a fire may be reached and still count as on time.
export const ON_TIME_MS = 30_000;

export interface Schedule {
  id: string;
  name: string;
  kind: ScheduleKind;
  group: string | null;
  trigger: Trigger;
  gate: string | null;
  gateMode: GateMode;
  command: readonly string[];
  workingDirectory: string;
  status: ScheduleStatus;
  timeoutMs: number | null;
  catchUpMs: number;
  createdAt: number;
  updatedAt: number;
  nextFireAt: number | null;
}

export interface Run {
  id: string;
  scheduleId: string;
  scheduleName: string;
  machineId: string;
  workingDirectory: string | null;
  executor: string | null;
  trigger: RunTrigger;
  status: RunStatus;
  gateExit: number | null;
  actionExit: number | null;
  startedAt: number;
  finishedAt: number | null;
  logPointer: string | null;
  ownerPid: number;
  pgid: number | null;
  revision: number;
}

export interface DaemonInfo {
  pid: number;
  version: string;
  startedAt: number;
  heartbeatAt: number;
}

// The user's home folder. HOME is read on every call, because Bun's
// os.homedir() keeps the value it saw at startup, so a test or a caller that
// points HOME elsewhere would otherwise still reach the real home. An empty
// value counts as unset; it must never turn into a relative path.
export const userHome = (env: NodeJS.ProcessEnv = process.env): string =>
  env.HOME === undefined || env.HOME === "" ? os.homedir() : env.HOME;

export const resolveHome = (env: NodeJS.ProcessEnv = process.env): string =>
  env.ULTRADIAN_HOME === undefined || env.ULTRADIAN_HOME === ""
    ? path.join(userHome(env), ".ultradian")
    : path.resolve(env.ULTRADIAN_HOME);

export const machineId = (): string => os.hostname();

export const isPidAlive = (pid: number): boolean => {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error instanceof Error && "code" in error && error.code === "EPERM";
  }
};

interface ScheduleRow {
  id: string;
  name: string;
  kind: ScheduleKind;
  schedule_group: string | null;
  trigger_kind: Trigger["kind"];
  trigger_value: string | null;
  timezone: string | null;
  gate: string | null;
  gate_mode: GateMode;
  command: string;
  working_directory: string;
  status: ScheduleStatus;
  timeout_ms: number | null;
  catch_up_ms: number;
  created_at: number;
  updated_at: number;
  next_fire_at: number | null;
}

interface RunRow {
  id: string;
  schedule_id: string;
  schedule_name: string;
  machine_id: string;
  working_directory: string | null;
  executor: string | null;
  trigger: RunTrigger;
  status: RunStatus;
  gate_exit: number | null;
  action_exit: number | null;
  started_at: number;
  finished_at: number | null;
  log_pointer: string | null;
  owner_pid: number;
  pgid: number | null;
  revision: number;
}

const triggerFromRow = (row: ScheduleRow): Trigger => {
  if (row.trigger_kind === "cron") {
    return {
      expression: row.trigger_value ?? "",
      kind: "cron",
      timezone: row.timezone,
    };
  }
  if (row.trigger_kind === "every") {
    return { kind: "every", seconds: Number(row.trigger_value) };
  }
  return { kind: "manual" };
};

const triggerValue = (trigger: Trigger): string | null => {
  if (trigger.kind === "cron") {
    return trigger.expression;
  }
  if (trigger.kind === "every") {
    return String(trigger.seconds);
  }
  return null;
};

const timezoneOf = (trigger: Trigger): string | null =>
  trigger.kind === "cron" ? trigger.timezone : null;

const parseCommand = (json: string): string[] => {
  const parsed: unknown = JSON.parse(json);
  return Array.isArray(parsed)
    ? parsed.filter((part): part is string => typeof part === "string")
    : [];
};

const scheduleFromRow = (row: ScheduleRow): Schedule => ({
  catchUpMs: row.catch_up_ms,
  command: parseCommand(row.command),
  createdAt: row.created_at,
  gate: row.gate,
  gateMode: row.gate_mode,
  group: row.schedule_group,
  id: row.id,
  kind: row.kind,
  name: row.name,
  nextFireAt: row.next_fire_at,
  status: row.status,
  timeoutMs: row.timeout_ms,
  trigger: triggerFromRow(row),
  updatedAt: row.updated_at,
  workingDirectory: row.working_directory,
});

const runFromRow = (row: RunRow): Run => ({
  actionExit: row.action_exit,
  executor: row.executor,
  finishedAt: row.finished_at,
  gateExit: row.gate_exit,
  id: row.id,
  logPointer: row.log_pointer,
  machineId: row.machine_id,
  ownerPid: row.owner_pid,
  pgid: row.pgid,
  revision: row.revision,
  scheduleId: row.schedule_id,
  scheduleName: row.schedule_name,
  startedAt: row.started_at,
  status: row.status,
  trigger: row.trigger,
  workingDirectory: row.working_directory,
});

// Ordered schema steps. Step N moves a database from user_version N-1 to
// N inside one immediate transaction, so concurrent openers serialize and
// each step applies exactly once. Steps are append-only once released: a
// change to the shape is a new step at the end, never an edit to an old one.
const migrations: readonly ((db: Database) => void)[] = [
  (db) => {
    db.run(`
      CREATE TABLE schedules (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL UNIQUE,
        kind TEXT NOT NULL DEFAULT 'schedule',
        schedule_group TEXT,
        trigger_kind TEXT NOT NULL,
        trigger_value TEXT,
        timezone TEXT,
        gate TEXT,
        gate_mode TEXT NOT NULL DEFAULT 'output',
        command TEXT NOT NULL,
        working_directory TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'active',
        timeout_ms INTEGER,
        catch_up_ms INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        next_fire_at INTEGER
      )`);
    db.run(`
      CREATE TABLE runs (
        id TEXT PRIMARY KEY,
        schedule_id TEXT NOT NULL,
        schedule_name TEXT NOT NULL,
        machine_id TEXT NOT NULL,
        working_directory TEXT,
        executor TEXT,
        trigger TEXT NOT NULL,
        status TEXT NOT NULL,
        gate_exit INTEGER,
        action_exit INTEGER,
        started_at INTEGER NOT NULL,
        finished_at INTEGER,
        log_pointer TEXT,
        owner_pid INTEGER NOT NULL,
        pgid INTEGER,
        revision INTEGER NOT NULL DEFAULT 0
      )`);
    db.run(
      "CREATE INDEX runs_by_schedule ON runs (schedule_name, started_at DESC)"
    );
    // Every insert or change to a run stamps it with the next value of one
    // counter, inside the writing transaction. Writers are serialized, so
    // revisions commit in order and 'runs --since <revision>' can never
    // miss a change that commits after a reader saw a higher one. Triggers
    // keep it out of every write path's hands.
    db.run(
      "CREATE TABLE counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL)"
    );
    db.run("INSERT INTO counters (name, value) VALUES ('runs', 0)");
    db.run("CREATE INDEX runs_by_revision ON runs (revision)");
    for (const event of ["INSERT", "UPDATE"]) {
      db.run(`
        CREATE TRIGGER runs_revision_${event.toLowerCase()}
        AFTER ${event} ON runs
        BEGIN
          UPDATE counters SET value = value + 1 WHERE name = 'runs';
          UPDATE runs SET revision = (SELECT value FROM counters WHERE name = 'runs')
          WHERE rowid = NEW.rowid;
        END`);
    }
    db.run(`
      CREATE TABLE daemon (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        pid INTEGER NOT NULL,
        version TEXT NOT NULL,
        started_at INTEGER NOT NULL,
        heartbeat_at INTEGER NOT NULL
      )`);
  },
];

export const schemaVersion = migrations.length;

const userVersion = (db: Database): number =>
  db.query<{ user_version: number }, []>("PRAGMA user_version").get()
    ?.user_version ?? 0;

const tableColumns = (db: Database, table: string): Set<string> =>
  new Set(
    db
      .query<{ name: string }, [string]>(
        "SELECT name FROM pragma_table_info(?)"
      )
      .all(table)
      .map((row) => row.name)
  );

// A column the table has, or a fallback expression when it lacks one.
const columnOr = (
  columns: Set<string>,
  name: string,
  fallback: string
): string => (columns.has(name) ? name : fallback);

// Databases written before schemas were versioned (0.1.x) carry the same
// three tables in an older shape, grown column by column. They are carried
// into the first versioned shape in place, schedules and run history
// included: the old tables step aside, step 1 builds the new ones, rows are
// copied across with defaults for what the old shape lacked, and the old
// tables go. Columns an even older build never added read as their default.
const upgradeLegacy = (db: Database): void => {
  const schedules = tableColumns(db, "schedules");
  const runs = tableColumns(db, "runs");
  const daemon = tableColumns(db, "daemon");
  db.run("DROP INDEX IF EXISTS runs_by_schedule");
  db.run("ALTER TABLE schedules RENAME TO legacy_schedules");
  if (runs.size > 0) {
    db.run("ALTER TABLE runs RENAME TO legacy_runs");
  }
  if (daemon.size > 0) {
    db.run("ALTER TABLE daemon RENAME TO legacy_daemon");
  }
  // 0.1 kept an interval as the duration typed ('15m'); now it is seconds.
  // add validated it as digits then one of s, m, h or d.
  const intervalSeconds = `CAST(substr(trigger_value, 1, length(trigger_value) - 1) AS INTEGER) *
    CASE substr(trigger_value, -1) WHEN 'd' THEN 86400 WHEN 'h' THEN 3600
      WHEN 'm' THEN 60 ELSE 1 END`;
  const [first] = migrations;
  first?.(db);
  db.run(`
    INSERT INTO schedules
      (id, name, kind, schedule_group, trigger_kind, trigger_value, gate,
       command, working_directory, status, timeout_ms, created_at,
       updated_at, next_fire_at)
    SELECT id, name, ${columnOr(schedules, "kind", "'schedule'")},
      ${columnOr(schedules, "schedule_group", "NULL")},
      CASE trigger_kind WHEN 'interval' THEN 'every' ELSE trigger_kind END,
      CASE trigger_kind WHEN 'interval' THEN ${intervalSeconds}
        ELSE trigger_value END,
      gate, command, working_directory, status,
      ${columnOr(schedules, "timeout_ms", "NULL")}, created_at, created_at,
      next_fire_at
    FROM legacy_schedules`);
  if (runs.size > 0) {
    // Copied oldest first, so the revision triggers number history in the
    // order it happened and a follower starting from zero reads it in order.
    const workingDirectory = runs.has("working_directory")
      ? "working_directory"
      : "(SELECT working_directory FROM legacy_schedules s WHERE s.id = r.schedule_id)";
    db.run(`
      INSERT INTO runs
        (id, schedule_id, schedule_name, machine_id, working_directory,
         executor, trigger, status, gate_exit, action_exit, started_at,
         finished_at, log_pointer, owner_pid)
      SELECT id, schedule_id, schedule_name, machine_id, ${workingDirectory},
        executor, trigger, status, gate_exit, action_exit, started_at,
        finished_at, log_pointer, owner_pid
      FROM legacy_runs r ORDER BY started_at, rowid`);
    db.run("DROP TABLE legacy_runs");
  }
  if (daemon.size > 0) {
    // A 0.1 daemon may still be running. Keeping its row keeps the
    // single-instance lock on it, so a newer daemon refuses to start beside
    // it and 'daemon stop' or 'restart' can still reach it.
    db.run(`
      INSERT INTO daemon (id, pid, version, started_at, heartbeat_at)
      SELECT id, pid, '0.1', started_at, heartbeat_at FROM legacy_daemon`);
    db.run("DROP TABLE legacy_daemon");
  }
  db.run("DROP TABLE legacy_schedules");
};

const migrate = (db: Database, file: string): void => {
  const apply = db.transaction((): void => {
    const current = userVersion(db);
    if (current > schemaVersion) {
      throw new AppError({
        code: "database_too_new",
        exitCode: ExitCode.CONFIG,
        hint: "Upgrade ultradian to the release that wrote this database.",
        message: `${file} is at schema version ${current}, newer than this release understands (${schemaVersion}).`,
      });
    }
    let start = current;
    if (current === 0 && tableColumns(db, "schedules").size > 0) {
      upgradeLegacy(db);
      start = 1;
    }
    for (const [index, step] of migrations.entries()) {
      if (index >= start) {
        step(db);
      }
    }
    if (current < schemaVersion) {
      db.run(`PRAGMA user_version = ${schemaVersion}`);
    }
  });
  apply.immediate();
};

export class ScheduleStore {
  readonly home: string;
  readonly #db: Database;

  constructor(home: string) {
    this.home = home;
    // Everything under the home is private to its owner: run logs hold
    // whatever gates and actions printed.
    mkdirSync(path.join(home, "logs"), { mode: 0o700, recursive: true });
    chmodSync(home, 0o700);
    chmodSync(path.join(home, "logs"), 0o700);
    const file = path.join(home, "ultradian.db");
    this.#db = new Database(file, { create: true, strict: true });
    // The busy timeout comes first so a second process opening a fresh
    // database waits for the first one's migration instead of failing.
    this.#db.run("PRAGMA busy_timeout = 5000;");
    this.#db.run("PRAGMA journal_mode = WAL;");
    migrate(this.#db, file);
    for (const companion of [file, `${file}-wal`, `${file}-shm`]) {
      if (existsSync(companion)) {
        chmodSync(companion, 0o600);
      }
    }
  }

  close(): void {
    this.#db.close();
  }

  #insert(schedule: Schedule): Schedule {
    const taken = this.#db
      .query<{ id: string }, [string]>(
        "SELECT id FROM schedules WHERE name = ?"
      )
      .get(schedule.name);
    if (taken !== null) {
      throw new AppError({
        code: "schedule_exists",
        exitCode: ExitCode.USAGE,
        hint: `Remove it first with 'rm ${schedule.name}' or pick another name.`,
        message: `A schedule named "${schedule.name}" already exists.`,
      });
    }
    this.#db
      .query(
        `INSERT INTO schedules
           (id, name, kind, schedule_group, trigger_kind, trigger_value,
            timezone, gate, gate_mode, command, working_directory, status,
            timeout_ms, catch_up_ms, created_at, updated_at, next_fire_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`
      )
      .run(
        schedule.id,
        schedule.name,
        schedule.kind,
        schedule.group,
        schedule.trigger.kind,
        triggerValue(schedule.trigger),
        timezoneOf(schedule.trigger),
        schedule.gate,
        schedule.gateMode,
        JSON.stringify(schedule.command),
        schedule.workingDirectory,
        schedule.status,
        schedule.timeoutMs,
        schedule.catchUpMs,
        schedule.createdAt,
        schedule.updatedAt,
        schedule.nextFireAt
      );
    return schedule;
  }

  addSchedule(input: {
    name: string;
    group: string | null;
    trigger: Trigger;
    gate: string | null;
    command: readonly string[];
    workingDirectory: string;
    timeoutMs: number | null;
    catchUpMs?: number;
    gateMode?: GateMode;
  }): Schedule {
    const now = Date.now();
    return this.#insert({
      catchUpMs: input.catchUpMs ?? 0,
      command: input.command,
      createdAt: now,
      gate: input.gate,
      gateMode: input.gateMode ?? "output",
      group: input.group,
      id: createId("schedule"),
      kind: "schedule",
      name: input.name,
      nextFireAt: nextFireAt(input.trigger, now),
      status: "active",
      timeoutMs: input.timeoutMs,
      trigger: input.trigger,
      updatedAt: now,
      workingDirectory: input.workingDirectory,
    });
  }

  // A one-shot job is due the moment it is registered, so the daemon claims
  // it on its next tick. Claiming advances a manual trigger to no next fire,
  // which is what makes it fire exactly once, whenever the daemon returns.
  addOnce(input: {
    name: string;
    command: readonly string[];
    workingDirectory: string;
    timeoutMs: number | null;
  }): Schedule {
    const now = Date.now();
    return this.#insert({
      catchUpMs: 0,
      command: input.command,
      createdAt: now,
      gate: null,
      gateMode: "output",
      group: null,
      id: createId("job"),
      kind: "once",
      name: input.name,
      nextFireAt: now,
      status: "active",
      timeoutMs: input.timeoutMs,
      trigger: { kind: "manual" },
      updatedAt: now,
      workingDirectory: input.workingDirectory,
    });
  }

  removeJob(id: string): void {
    this.#db.query("DELETE FROM schedules WHERE id = ?").run(id);
  }

  // Looks a schedule up by its id or its name, so tools can hold on to the
  // stable id while people type names.
  getSchedule(reference: string): Schedule | undefined {
    const row = this.#db
      .query<ScheduleRow, [string]>(
        `SELECT * FROM schedules
         WHERE (id = ?1 OR name = ?1) AND kind = 'schedule'
         ORDER BY (id = ?1) DESC LIMIT 1`
      )
      .get(reference);
    return row === null ? undefined : scheduleFromRow(row);
  }

  requireSchedule(reference: string): Schedule {
    const schedule = this.getSchedule(reference);
    if (schedule === undefined) {
      throw new AppError({
        code: "schedule_not_found",
        exitCode: ExitCode.ERROR,
        hint: "List schedules with 'list'.",
        message: `No schedule named "${reference}".`,
      });
    }
    return schedule;
  }

  listSchedules(): Schedule[] {
    return this.#db
      .query<ScheduleRow, []>(
        "SELECT * FROM schedules WHERE kind = 'schedule' ORDER BY name"
      )
      .all()
      .map(scheduleFromRow);
  }

  setScheduleStatus(name: string, status: ScheduleStatus): Schedule {
    const schedule = this.requireSchedule(name);
    const now = Date.now();
    const nextFire =
      status === "active" ? nextFireAt(schedule.trigger, now) : null;
    this.#db
      .query(
        "UPDATE schedules SET status = ?, next_fire_at = ?, updated_at = ? WHERE id = ?"
      )
      .run(status, nextFire, now, schedule.id);
    return { ...schedule, nextFireAt: nextFire, status, updatedAt: now };
  }

  // Applies only the fields present in the patch. A new trigger recomputes
  // the next fire from now unless the schedule is paused.
  updateSchedule(
    reference: string,
    patch: {
      trigger?: Trigger;
      gate?: string | null;
      gateMode?: GateMode;
      timeoutMs?: number | null;
      catchUpMs?: number;
      group?: string | null;
      command?: readonly string[];
      workingDirectory?: string;
    }
  ): Schedule {
    const existing = this.requireSchedule(reference);
    const now = Date.now();
    const updated: Schedule = {
      ...existing,
      catchUpMs: patch.catchUpMs ?? existing.catchUpMs,
      command: patch.command ?? existing.command,
      gate: patch.gate === undefined ? existing.gate : patch.gate,
      gateMode: patch.gateMode ?? existing.gateMode,
      group: patch.group === undefined ? existing.group : patch.group,
      timeoutMs:
        patch.timeoutMs === undefined ? existing.timeoutMs : patch.timeoutMs,
      trigger: patch.trigger ?? existing.trigger,
      updatedAt: now,
      workingDirectory: patch.workingDirectory ?? existing.workingDirectory,
    };
    updated.nextFireAt =
      patch.trigger === undefined || existing.status === "paused"
        ? existing.nextFireAt
        : nextFireAt(updated.trigger, now);
    this.#db
      .query(
        `UPDATE schedules
         SET trigger_kind = ?, trigger_value = ?, timezone = ?, gate = ?,
             gate_mode = ?, timeout_ms = ?, catch_up_ms = ?,
             schedule_group = ?, command = ?, working_directory = ?,
             next_fire_at = ?, updated_at = ?
         WHERE id = ?`
      )
      .run(
        updated.trigger.kind,
        triggerValue(updated.trigger),
        timezoneOf(updated.trigger),
        updated.gate,
        updated.gateMode,
        updated.timeoutMs,
        updated.catchUpMs,
        updated.group,
        JSON.stringify(updated.command),
        updated.workingDirectory,
        updated.nextFireAt,
        updated.updatedAt,
        existing.id
      );
    return updated;
  }

  setGroupStatus(group: string, status: ScheduleStatus): Schedule[] {
    const members = this.listSchedules().filter(
      (schedule) => schedule.group === group
    );
    if (members.length === 0) {
      throw new AppError({
        code: "group_not_found",
        exitCode: ExitCode.ERROR,
        hint: "List schedules and their groups with 'list'.",
        message: `No schedules in group "${group}".`,
      });
    }
    return members.map((member) => this.setScheduleStatus(member.id, status));
  }

  // Queued fires die with their schedule; a running one finishes as is.
  removeSchedule(name: string): Schedule {
    const schedule = this.requireSchedule(name);
    this.#db
      .query(
        `UPDATE runs SET status = 'canceled', finished_at = ?
         WHERE schedule_id = ? AND status = 'queued'`
      )
      .run(Date.now(), schedule.id);
    this.#db.query("DELETE FROM schedules WHERE id = ?").run(schedule.id);
    return schedule;
  }

  // Claims every schedule whose fire has come, advancing each to its next
  // fire in the same transaction so a fire is claimed once. A fire reached
  // late, because the machine slept or the daemon was down, is only taken
  // within the schedule's catch-up window (never less than ON_TIME_MS, so
  // ordinary tick latency is not a miss); otherwise one missed run is
  // recorded and the schedule skips forward. Either way a schedule fires
  // at most once per claim, however many fires it slept through. One-shot
  // jobs are never missed: they wait for the daemon however long it takes.
  claimDue(nowMs: number): { due: Schedule[]; missed: Schedule[] } {
    const claim = this.#db.transaction(
      (): { due: Schedule[]; missed: Schedule[] } => {
        const ready = this.#db
          .query<ScheduleRow, [number]>(
            `SELECT * FROM schedules
             WHERE status = 'active'
               AND next_fire_at IS NOT NULL
               AND next_fire_at <= ?`
          )
          .all(nowMs)
          .map(scheduleFromRow);
        const due: Schedule[] = [];
        const missed: Schedule[] = [];
        for (const schedule of ready) {
          this.#db
            .query("UPDATE schedules SET next_fire_at = ? WHERE id = ?")
            .run(nextFireAt(schedule.trigger, nowMs), schedule.id);
          const lateBy = nowMs - (schedule.nextFireAt ?? nowMs);
          const window = Math.max(schedule.catchUpMs, ON_TIME_MS);
          if (schedule.kind === "schedule" && lateBy > window) {
            this.recordRun({
              schedule,
              status: "missed",
              trigger: "scheduled",
            });
            missed.push(schedule);
          } else {
            due.push(schedule);
          }
        }
        return { due, missed };
      }
    );
    return claim.immediate();
  }

  // A run is in flight from the moment it is queued until it finishes.
  hasActiveRun(scheduleId: string): boolean {
    const row = this.#db
      .query<{ id: string }, [string]>(
        `SELECT id FROM runs
         WHERE schedule_id = ? AND status IN ('running', 'queued') LIMIT 1`
      )
      .get(scheduleId);
    return row !== null;
  }

  #insertRun(input: {
    schedule: Schedule;
    trigger: RunTrigger;
    status: RunStatus;
    withLog: boolean;
  }): Run {
    const now = Date.now();
    const id = createId("run");
    const finished = input.status !== "running" && input.status !== "queued";
    const run: Run = {
      actionExit: null,
      executor: null,
      finishedAt: finished ? now : null,
      gateExit: null,
      id,
      logPointer: input.withLog
        ? path.join(
            this.home,
            "logs",
            input.schedule.name,
            new Date(now).toISOString().slice(0, 10),
            `${id}.log`
          )
        : null,
      machineId: machineId(),
      ownerPid: process.pid,
      pgid: null,
      revision: 0,
      scheduleId: input.schedule.id,
      scheduleName: input.schedule.name,
      startedAt: now,
      status: input.status,
      trigger: input.trigger,
      workingDirectory: input.schedule.workingDirectory,
    };
    this.#db
      .query(
        `INSERT INTO runs
           (id, schedule_id, schedule_name, machine_id, working_directory,
            executor, trigger, status, gate_exit, action_exit, started_at,
            finished_at, log_pointer, owner_pid)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`
      )
      .run(
        run.id,
        run.scheduleId,
        run.scheduleName,
        run.machineId,
        run.workingDirectory,
        run.executor,
        run.trigger,
        run.status,
        run.gateExit,
        run.actionExit,
        run.startedAt,
        run.finishedAt,
        run.logPointer,
        run.ownerPid
      );
    return this.getRun(id) ?? run;
  }

  // Starts a run, or queues one for the daemon, unless the schedule already
  // has a run in flight. The check and the insert share one immediate
  // transaction, so two processes racing to fire the same schedule cannot
  // both win. Returns undefined when the schedule is busy.
  beginRun(input: {
    schedule: Schedule;
    trigger: RunTrigger;
    queued?: boolean;
  }): Run | undefined {
    const begin = this.#db.transaction((): Run | undefined =>
      this.hasActiveRun(input.schedule.id)
        ? undefined
        : this.#insertRun({
            schedule: input.schedule,
            status: input.queued === true ? "queued" : "running",
            trigger: input.trigger,
            withLog: true,
          })
    );
    return begin.immediate();
  }

  // Records a fire that never ran, such as one skipped for overlap.
  recordRun(input: {
    schedule: Schedule;
    trigger: RunTrigger;
    status: "skipped" | "missed";
  }): Run {
    return this.#insertRun({ ...input, withLog: false });
  }

  // Hands queued manual fires to the daemon. Claiming moves them to running
  // under the daemon's pid in the same transaction, so each is claimed once.
  claimQueued(ownerPid: number): { run: Run; schedule: Schedule }[] {
    const claim = this.#db.transaction(
      (): { run: Run; schedule: Schedule }[] => {
        const now = Date.now();
        const queued = this.#db
          .query<RunRow, []>(
            "SELECT * FROM runs WHERE status = 'queued' ORDER BY started_at"
          )
          .all()
          .map(runFromRow);
        const claimed: { run: Run; schedule: Schedule }[] = [];
        for (const run of queued) {
          const row = this.#db
            .query<ScheduleRow, [string]>(
              "SELECT * FROM schedules WHERE id = ?"
            )
            .get(run.scheduleId);
          if (row === null) {
            continue;
          }
          this.#db
            .query(
              `UPDATE runs SET status = 'running', owner_pid = ?, started_at = ?
               WHERE id = ?`
            )
            .run(ownerPid, now, run.id);
          claimed.push({
            run: { ...run, ownerPid, startedAt: now, status: "running" },
            schedule: scheduleFromRow(row),
          });
        }
        return claimed;
      }
    );
    return claim.immediate();
  }

  setRunExecutor(runId: string, executor: string): void {
    this.#db
      .query("UPDATE runs SET executor = ? WHERE id = ?")
      .run(executor, runId);
  }

  // The process group a run is currently waiting on: the gate's while it
  // gates, then the action's. Its leader's pid is the group id, so it is
  // both the liveness evidence for the stall reaper and the handle that
  // recovery uses to terminate whatever a dead owner left running.
  setRunProcessGroup(runId: string, pgid: number): void {
    this.#db.query("UPDATE runs SET pgid = ? WHERE id = ?").run(pgid, runId);
  }

  // First writer wins: a run finishes exactly once. A reaped run keeps its
  // reaped status even if the wedged fire's finish eventually arrives.
  finishRun(
    runId: string,
    result: {
      status: RunStatus;
      gateExit?: number | null;
      actionExit?: number | null;
    }
  ): void {
    this.#db
      .query(
        `UPDATE runs
         SET status = ?, gate_exit = ?, action_exit = ?, finished_at = ?
         WHERE id = ? AND status = 'running'`
      )
      .run(
        result.status,
        result.gateExit ?? null,
        result.actionExit ?? null,
        Date.now(),
        runId
      );
  }

  // Cancellation is a store write like any other: the run finishes as
  // canceled here and now, and the process that owns the fire sees the
  // status change and terminates its process group.
  cancelRun(runId: string): Run {
    const run = this.getRun(runId);
    if (run === undefined) {
      throw new AppError({
        code: "run_not_found",
        exitCode: ExitCode.ERROR,
        hint: "List recent runs with 'runs' or 'logs'.",
        message: `No run with id "${runId}".`,
      });
    }
    if (run.status !== "running" && run.status !== "queued") {
      throw new AppError({
        code: "run_finished",
        exitCode: ExitCode.ERROR,
        message: `Run ${runId} already finished as ${run.status}.`,
      });
    }
    this.#db
      .query(
        `UPDATE runs SET status = 'canceled', finished_at = ?
         WHERE id = ? AND status IN ('running', 'queued')`
      )
      .run(Date.now(), runId);
    return this.getRun(runId) ?? run;
  }

  getRun(runId: string): Run | undefined {
    const row = this.#db
      .query<RunRow, [string]>("SELECT * FROM runs WHERE id = ?")
      .get(runId);
    return row === null ? undefined : runFromRow(row);
  }

  // The read surface for other tools: every run changed after a revision
  // cursor, oldest change first. A run reappears each time it changes, so
  // a consumer that keeps the last revision it saw sees every start,
  // finish, and cancellation exactly in commit order. Pruned runs simply
  // stop appearing.
  exportRuns(options: { since?: number; limit?: number } = {}): Run[] {
    const limit = options.limit ?? -1;
    return this.#db
      .query<RunRow, [number, number]>(
        "SELECT * FROM runs WHERE revision > ? ORDER BY revision ASC LIMIT ?"
      )
      .all(options.since ?? 0, limit)
      .map(runFromRow);
  }

  listRuns(options: { scheduleName?: string; limit: number }): Run[] {
    if (options.scheduleName !== undefined) {
      return this.#db
        .query<RunRow, [string, number]>(
          `SELECT * FROM runs WHERE schedule_name = ?
           ORDER BY started_at DESC LIMIT ?`
        )
        .all(options.scheduleName, options.limit)
        .map(runFromRow);
    }
    return this.#db
      .query<RunRow, [number]>(
        "SELECT * FROM runs ORDER BY started_at DESC LIMIT ?"
      )
      .all(options.limit)
      .map(runFromRow);
  }

  activeRuns(): Run[] {
    return this.#db
      .query<RunRow, []>(
        "SELECT * FROM runs WHERE status = 'running' ORDER BY started_at"
      )
      .all()
      .map(runFromRow);
  }

  queuedRuns(): Run[] {
    return this.#db
      .query<RunRow, []>(
        "SELECT * FROM runs WHERE status = 'queued' ORDER BY started_at"
      )
      .all()
      .map(runFromRow);
  }

  lastRun(scheduleName: string): Run | undefined {
    const row = this.#db
      .query<RunRow, [string]>(
        `SELECT * FROM runs WHERE schedule_name = ?
         ORDER BY started_at DESC LIMIT 1`
      )
      .get(scheduleName);
    return row === null ? undefined : runFromRow(row);
  }

  countRuns(scheduleName?: string): number {
    if (scheduleName !== undefined) {
      const row = this.#db
        .query<{ total: number }, [string]>(
          "SELECT COUNT(*) AS total FROM runs WHERE schedule_name = ?"
        )
        .get(scheduleName);
      return row?.total ?? 0;
    }
    const row = this.#db
      .query<{ total: number }, []>("SELECT COUNT(*) AS total FROM runs")
      .get();
    return row?.total ?? 0;
  }

  // Deletes finished runs that started before the cutoff together with
  // their log files, then any day or schedule log directory left empty.
  // Queued and running runs are never touched.
  pruneHistory(
    cutoffMs: number,
    scheduleName?: string
  ): { removed: number; freedBytes: number } {
    const stale = this.#db
      .query<
        { id: string; log_pointer: string | null },
        [number, string | null]
      >(
        `SELECT id, log_pointer FROM runs
         WHERE status NOT IN ('running', 'queued') AND started_at < ?1
           AND (?2 IS NULL OR schedule_name = ?2)`
      )
      .all(cutoffMs, scheduleName ?? null);
    const remove = this.#db.transaction((): void => {
      for (const row of stale) {
        this.#db.query("DELETE FROM runs WHERE id = ?").run(row.id);
      }
    });
    remove();
    let freedBytes = 0;
    const emptied = new Set<string>();
    for (const row of stale) {
      if (row.log_pointer === null) {
        continue;
      }
      freedBytes +=
        statSync(row.log_pointer, { throwIfNoEntry: false })?.size ?? 0;
      rmSync(row.log_pointer, { force: true });
      emptied.add(path.dirname(row.log_pointer));
    }
    for (const directory of emptied) {
      for (const candidate of [directory, path.dirname(directory)]) {
        try {
          rmdirSync(candidate);
        } catch {
          // Not empty yet; a later prune will get it.
        }
      }
    }
    return { freedBytes, removed: stale.length };
  }

  readDaemon(): DaemonInfo | undefined {
    const row = this.#db
      .query<
        {
          pid: number;
          version: string;
          started_at: number;
          heartbeat_at: number;
        },
        []
      >(
        "SELECT pid, version, started_at, heartbeat_at FROM daemon WHERE id = 1"
      )
      .get();
    return row === null
      ? undefined
      : {
          heartbeatAt: row.heartbeat_at,
          pid: row.pid,
          startedAt: row.started_at,
          version: row.version,
        };
  }

  // The single-instance lock. Under one immediate transaction, a daemon
  // takes the row unless another live daemon holds it, so two daemons
  // starting together cannot both win. Returns the holder on conflict.
  claimDaemon(
    claimant: Omit<DaemonInfo, "heartbeatAt">,
    isLive: (holder: DaemonInfo) => boolean
  ): DaemonInfo | undefined {
    const claim = this.#db.transaction((): DaemonInfo | undefined => {
      const holder = this.readDaemon();
      if (
        holder !== undefined &&
        holder.pid !== claimant.pid &&
        isLive(holder)
      ) {
        return holder;
      }
      this.#db
        .query(
          `INSERT INTO daemon (id, pid, version, started_at, heartbeat_at)
           VALUES (1, ?, ?, ?, ?)
           ON CONFLICT (id) DO UPDATE
             SET pid = excluded.pid,
                 version = excluded.version,
                 started_at = excluded.started_at,
                 heartbeat_at = excluded.heartbeat_at`
        )
        .run(claimant.pid, claimant.version, claimant.startedAt, Date.now());
      return undefined;
    });
    return claim.immediate();
  }

  // Refreshes the holder's heartbeat. False means the lock was taken over
  // (this daemon stalled past the stale window), and it must stop.
  heartbeat(pid: number): boolean {
    const result = this.#db
      .query("UPDATE daemon SET heartbeat_at = ? WHERE id = 1 AND pid = ?")
      .run(Date.now(), pid);
    return result.changes === 1;
  }

  clearDaemon(pid: number): void {
    this.#db.query("DELETE FROM daemon WHERE id = 1 AND pid = ?").run(pid);
  }

  // Called on daemon start: orphaned runs get marked interrupted and their
  // process groups are handed back for the daemon to terminate, and
  // one-shot jobs a crashed daemon fired but never removed are swept. Fires
  // missed while the daemon was down are settled by the first claimDue,
  // under each schedule's catch-up window, exactly as after a sleep.
  recover(): {
    interrupted: number;
    orphanGroups: number[];
    swept: number;
  } {
    const running = this.activeRuns();
    let interrupted = 0;
    const orphanGroups: number[] = [];
    for (const run of running) {
      if (!isPidAlive(run.ownerPid)) {
        this.finishRun(run.id, {
          actionExit: run.actionExit,
          gateExit: run.gateExit,
          status: "interrupted",
        });
        interrupted += 1;
        if (run.pgid !== null) {
          orphanGroups.push(run.pgid);
        }
      }
    }
    const spent = this.#db
      .query<{ total: number }, []>(
        `SELECT COUNT(*) AS total FROM schedules
         WHERE kind = 'once' AND next_fire_at IS NULL`
      )
      .get();
    this.#db.run(
      "DELETE FROM schedules WHERE kind = 'once' AND next_fire_at IS NULL"
    );
    return {
      interrupted,
      orphanGroups,
      swept: spent?.total ?? 0,
    };
  }
}

export const scheduleStore =
  createServiceToken<ScheduleStore>("schedule-store");

export const scheduleStoreProvider: ServiceProvider<ScheduleStore> = {
  create() {
    return new ScheduleStore(resolveHome());
  },
  dispose(store) {
    store.close();
  },
  token: scheduleStore,
};
