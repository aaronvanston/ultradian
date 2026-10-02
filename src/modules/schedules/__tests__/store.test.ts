import { Database } from "bun:sqlite";
import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import os from "node:os";
import path from "node:path";

import { schemaVersion, ScheduleStore } from "../store.ts";
import type { Run, RunTrigger, Schedule } from "../store.ts";

const homes: string[] = [];

const temporaryStore = (): ScheduleStore => {
  const home = mkdtempSync(path.join(os.tmpdir(), "ultradian-store-"));
  homes.push(home);
  return new ScheduleStore(home);
};

const begin = (
  store: ScheduleStore,
  schedule: Schedule,
  trigger: RunTrigger
): Run => {
  const run = store.beginRun({ schedule, trigger });
  if (run === undefined) {
    throw new Error("The schedule already had a run in flight.");
  }
  return run;
};

afterEach(() => {
  for (const home of homes.splice(0)) {
    rmSync(home, { force: true, recursive: true });
  }
});

describe("schema migrations", () => {
  test("a fresh database lands on the latest version and reopens cleanly", () => {
    const store = temporaryStore();
    const { home } = store;
    store.close();
    const raw = new Database(path.join(home, "ultradian.db"));
    expect(
      raw.query<{ user_version: number }, []>("PRAGMA user_version").get()
        ?.user_version
    ).toBe(schemaVersion);
    raw.close();
    new ScheduleStore(home).close();
  });

  test("refuses a database written by a newer release", () => {
    const home = mkdtempSync(path.join(os.tmpdir(), "ultradian-store-"));
    homes.push(home);
    const raw = new Database(path.join(home, "ultradian.db"));
    raw.run(`PRAGMA user_version = ${schemaVersion + 1}`);
    raw.close();
    expect(() => new ScheduleStore(home)).toThrow(/newer than this release/u);
  });

  test("refuses an unversioned database instead of guessing its shape", () => {
    const home = mkdtempSync(path.join(os.tmpdir(), "ultradian-store-"));
    homes.push(home);
    const raw = new Database(path.join(home, "ultradian.db"));
    raw.run("CREATE TABLE schedules (id TEXT PRIMARY KEY)");
    raw.close();
    expect(() => new ScheduleStore(home)).toThrow(/predates versioned/u);
  });
});

describe("schedule store", () => {
  test("claims a due schedule exactly once per fire", () => {
    const store = temporaryStore();
    store.addSchedule({
      command: ["echo", "ok"],
      gate: null,
      group: null,
      name: "tick",
      timeoutMs: null,
      trigger: { kind: "every", seconds: 1 },
      workingDirectory: "/tmp",
    });
    const fireTime = Date.now() + 1500;
    const first = store.claimDue(fireTime).due;
    expect(first.map((schedule) => schedule.name)).toEqual(["tick"]);
    expect(store.claimDue(fireTime).due).toHaveLength(0);
    const advanced = store.getSchedule("tick");
    expect(advanced?.nextFireAt).toBe(fireTime + 1000);
    store.close();
  });

  test("paused schedules never fire and resume reschedules", () => {
    const store = temporaryStore();
    store.addSchedule({
      command: ["echo", "ok"],
      gate: null,
      group: null,
      name: "tick",
      timeoutMs: null,
      trigger: { kind: "every", seconds: 1 },
      workingDirectory: "/tmp",
    });
    store.setScheduleStatus("tick", "paused");
    expect(store.claimDue(Date.now() + 60_000).due).toHaveLength(0);
    const resumed = store.setScheduleStatus("tick", "active");
    expect(resumed.nextFireAt).not.toBeNull();
    store.close();
  });

  test("recovery marks dead-owner runs interrupted", () => {
    const store = temporaryStore();
    const schedule = store.addSchedule({
      command: ["echo", "ok"],
      gate: null,
      group: null,
      name: "tick",
      timeoutMs: null,
      trigger: { kind: "every", seconds: 1 },
      workingDirectory: "/tmp",
    });
    const { home } = store;
    // A run owned by a process that no longer exists, written the way a
    // crashed daemon would have left it.
    const raw = new Database(path.join(home, "ultradian.db"));
    raw
      .query(
        `INSERT INTO runs
           (id, schedule_id, schedule_name, machine_id, executor,
            trigger, status, gate_exit, action_exit, started_at, finished_at,
            log_pointer, owner_pid)
         VALUES (?, ?, ?, ?, NULL, 'scheduled', 'running', NULL, NULL,
                 ?, NULL, NULL, ?)`
      )
      .run("run_orphan", schedule.id, "tick", "test", Date.now(), 3_999_999);
    raw.close();

    const result = store.recover();
    expect(result.interrupted).toBe(1);
    expect(store.getRun("run_orphan")?.status).toBe("interrupted");
    store.close();
  });

  test("a late fire runs once inside its catch-up window and is missed beyond it", () => {
    const store = temporaryStore();
    const add = (name: string, catchUpMs: number) =>
      store.addSchedule({
        catchUpMs,
        command: ["echo", "ok"],
        gate: null,
        group: null,
        name,
        timeoutMs: null,
        trigger: { kind: "every", seconds: 3600 },
        workingDirectory: "/tmp",
      });
    const strict = add("strict", 0);
    const forgiving = add("forgiving", 4 * 3_600_000);
    // Asleep for three hours: three fires slept through for each.
    const wake = strict.createdAt + 3 * 3_600_000 + 60_000;
    const claimed = store.claimDue(wake);
    expect(claimed.due.map((schedule) => schedule.name)).toEqual([
      forgiving.name,
    ]);
    expect(claimed.missed.map((schedule) => schedule.name)).toEqual([
      strict.name,
    ]);
    expect(store.listRuns({ limit: 10, scheduleName: "strict" })).toMatchObject(
      [{ status: "missed", trigger: "scheduled" }]
    );
    // Both skip forward to one fire from now, never a burst.
    expect(store.getSchedule("strict")?.nextFireAt).toBe(wake + 3_600_000);
    expect(store.getSchedule("forgiving")?.nextFireAt).toBe(wake + 3_600_000);
    expect(store.claimDue(wake + 1000).due).toHaveLength(0);
    // Ordinary tick latency is never a miss, even with no catch-up.
    const onTime = wake + 3_600_000 + 2000;
    expect(
      store
        .claimDue(onTime)
        .due.map((schedule) => schedule.name)
        .toSorted()
    ).toEqual(["forgiving", "strict"]);
    store.close();
  });

  test("a schedule never has two runs in flight, queued or running", () => {
    const store = temporaryStore();
    const schedule = store.addSchedule({
      command: ["echo", "ok"],
      gate: null,
      group: null,
      name: "tick",
      timeoutMs: null,
      trigger: { kind: "manual" },
      workingDirectory: "/tmp",
    });
    const queued = store.beginRun({
      queued: true,
      schedule,
      trigger: "manual",
    });
    expect(queued?.status).toBe("queued");
    expect(store.beginRun({ schedule, trigger: "manual" })).toBeUndefined();
    const [claimed] = store.claimQueued(4242);
    expect(claimed?.run.id).toBe(queued?.id);
    expect(store.getRun(queued?.id ?? "")?.ownerPid).toBe(4242);
    expect(store.claimQueued(4242)).toHaveLength(0);
    expect(store.beginRun({ schedule, trigger: "manual" })).toBeUndefined();
    store.finishRun(queued?.id ?? "", { status: "succeeded" });
    expect(store.beginRun({ schedule, trigger: "manual" })?.status).toBe(
      "running"
    );
    store.close();
  });

  test("the runs cursor sees every change in commit order, late finishers included", () => {
    const store = temporaryStore();
    const add = (name: string) =>
      store.addSchedule({
        command: ["echo", "ok"],
        gate: null,
        group: null,
        name,
        timeoutMs: null,
        trigger: { kind: "manual" },
        workingDirectory: "/tmp",
      });
    const slow = begin(store, add("slow"), "manual");
    const quick = begin(store, add("quick"), "manual");
    store.finishRun(quick.id, { actionExit: 0, status: "succeeded" });
    const firstPage = store.exportRuns();
    expect(firstPage.map((run) => run.id)).toEqual([slow.id, quick.id]);
    const cursor = firstPage.at(-1)?.revision ?? 0;
    expect(store.exportRuns({ since: cursor })).toHaveLength(0);

    // The run that started first finishes last: a started_at cursor would
    // never show it again, the revision cursor does.
    store.finishRun(slow.id, { actionExit: 1, status: "failed" });
    const changed = store.exportRuns({ since: cursor });
    expect(changed.map((run) => [run.id, run.status])).toEqual([
      [slow.id, "failed"],
    ]);
    expect(store.exportRuns({ limit: 1 }).map((run) => run.id)).toEqual([
      quick.id,
    ]);
    store.close();
  });

  test("a one-shot job is claimed once and never lists as a schedule", () => {
    const store = temporaryStore();
    const job = store.addOnce({
      command: ["echo", "ok"],
      name: "once-echo-abcdef",
      timeoutMs: null,
      workingDirectory: "/tmp",
    });
    expect(store.listSchedules()).toHaveLength(0);
    expect(store.getSchedule(job.name)).toBeUndefined();

    const claimed = store.claimDue(Date.now()).due;
    expect(claimed.map((entry) => entry.id)).toEqual([job.id]);
    expect(claimed[0]?.kind).toBe("once");
    expect(store.claimDue(Date.now() + 60_000).due).toHaveLength(0);
    store.close();
  });

  test("recovery interrupts a one-shot run and sweeps only spent jobs", () => {
    const store = temporaryStore();
    const spent = store.addOnce({
      command: ["echo", "ok"],
      name: "once-spent-abcdef",
      timeoutMs: null,
      workingDirectory: "/tmp",
    });
    // The daemon claimed and started that job, then died mid-run.
    store.claimDue(spent.createdAt);
    store.addOnce({
      command: ["echo", "ok"],
      name: "once-pending-abcdef",
      timeoutMs: null,
      workingDirectory: "/tmp",
    });
    const raw = new Database(path.join(store.home, "ultradian.db"));
    raw
      .query(
        `INSERT INTO runs
           (id, schedule_id, schedule_name, machine_id, executor,
            trigger, status, gate_exit, action_exit, started_at, finished_at,
            log_pointer, owner_pid)
         VALUES (?, ?, ?, ?, NULL, 'once', 'running', NULL, NULL,
                 ?, NULL, NULL, ?)`
      )
      .run("run_once", spent.id, spent.name, "test", Date.now(), 3_999_999);
    raw.close();

    const result = store.recover();
    expect(result.interrupted).toBe(1);
    expect(result.swept).toBe(1);
    expect(store.getRun("run_once")?.status).toBe("interrupted");
    // The unclaimed job survives, still due, so the daemon runs it now.
    const pending = store.claimDue(Date.now()).due;
    expect(pending.map((entry) => entry.name)).toEqual(["once-pending-abcdef"]);
    store.close();
  });

  test("a run owned by a live process survives recovery", () => {
    const store = temporaryStore();
    const schedule = store.addSchedule({
      command: ["echo", "ok"],
      gate: null,
      group: null,
      name: "tick",
      timeoutMs: null,
      trigger: { kind: "manual" },
      workingDirectory: "/tmp",
    });
    begin(store, schedule, "manual");
    const result = store.recover();
    expect(result.interrupted).toBe(0);
    expect(store.activeRuns()).toHaveLength(1);
    store.close();
  });
});
