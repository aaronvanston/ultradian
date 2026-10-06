import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import os from "node:os";
import path from "node:path";

import { executeFire, groupIsAlive, RUN_LOG_CAP_BYTES } from "../runner.ts";
import { isPidAlive, ScheduleStore } from "../store.ts";
import type { Schedule } from "../store.ts";

const homes: string[] = [];

const temporaryStore = (): ScheduleStore => {
  const home = mkdtempSync(path.join(os.tmpdir(), "ultradian-runner-"));
  homes.push(home);
  return new ScheduleStore(home);
};

// Permission bits only, without the file type.
const permissions = (file: string): number => statSync(file).mode % 0o1000;

afterEach(() => {
  for (const home of homes.splice(0)) {
    rmSync(home, { force: true, recursive: true });
  }
});

const fire = async (
  store: ScheduleStore,
  schedule: Schedule,
  signal?: AbortSignal
) => {
  const run = store.beginRun({ schedule, trigger: "manual" });
  if (run === undefined) {
    throw new Error("The schedule already had a run in flight.");
  }
  return await executeFire({
    run,
    schedule,
    store,
    ...(signal === undefined ? {} : { signal }),
  });
};

const addShell = (
  store: ScheduleStore,
  script: string,
  timeoutMs: number | null
): Schedule =>
  store.addSchedule({
    command: ["/bin/sh", "-c", script],
    gate: null,
    group: null,
    name: "job",
    timeoutMs,
    trigger: { kind: "manual" },
    workingDirectory: store.home,
  });

describe("process groups", () => {
  test("a timeout terminates the whole group, grandchildren included", async () => {
    const store = temporaryStore();
    const pidFile = path.join(store.home, "grandchild.pid");
    const schedule = addShell(
      store,
      `sleep 30 & echo $! > ${pidFile}; sleep 30`,
      1000
    );
    const started = Date.now();
    const run = await fire(store, schedule);
    expect(run.status).toBe("timed_out");
    expect(Date.now() - started).toBeLessThan(5000);
    expect(run.pgid).not.toBeNull();
    expect(groupIsAlive(run.pgid ?? 0)).toBe(false);
    const grandchild = Number(readFileSync(pidFile, "utf-8").trim());
    expect(isPidAlive(grandchild)).toBe(false);
    store.close();
  }, 15_000);

  test("an action that exits leaves nothing running and never hangs on its pipes", async () => {
    const store = temporaryStore();
    const pidFile = path.join(store.home, "grandchild.pid");
    const schedule = addShell(
      store,
      `sleep 30 & echo $! > ${pidFile}; exit 0`,
      null
    );
    const started = Date.now();
    const run = await fire(store, schedule);
    expect(run.status).toBe("succeeded");
    expect(Date.now() - started).toBeLessThan(5000);
    const grandchild = Number(readFileSync(pidFile, "utf-8").trim());
    expect(isPidAlive(grandchild)).toBe(false);
    store.close();
  }, 15_000);

  test("a shutdown signal interrupts the fire and its group", async () => {
    const store = temporaryStore();
    const schedule = addShell(store, "sleep 30", null);
    const controller = new AbortController();
    setTimeout(() => {
      controller.abort();
    }, 300);
    const run = await fire(store, schedule, controller.signal);
    expect(run.status).toBe("interrupted");
    expect(groupIsAlive(run.pgid ?? 0)).toBe(false);
    store.close();
  }, 15_000);

  test("a cancel written to the store stops the fire within a second or so", async () => {
    const store = temporaryStore();
    const schedule = addShell(store, "sleep 30", null);
    const run = store.beginRun({ schedule, trigger: "manual" });
    if (run === undefined) {
      throw new Error("The schedule already had a run in flight.");
    }
    setTimeout(() => {
      store.cancelRun(run.id);
    }, 300);
    const started = Date.now();
    const finished = await executeFire({ run, schedule, store });
    expect(finished.status).toBe("canceled");
    expect(Date.now() - started).toBeLessThan(5000);
    expect(groupIsAlive(finished.pgid ?? 0)).toBe(false);
    expect(() => store.cancelRun(run.id)).toThrow(/already finished/u);
    store.close();
  }, 15_000);

  test("a run log is private and capped, and still records how the run ended", async () => {
    const store = temporaryStore();
    const schedule = addShell(store, "head -c 11000000 /dev/zero", null);
    const run = await fire(store, schedule);
    expect(run.status).toBe("succeeded");
    const pointer = run.logPointer ?? "";
    const { size } = statSync(pointer);
    expect(permissions(pointer)).toBe(0o600);
    expect(size).toBeLessThan(RUN_LOG_CAP_BYTES + 4096);
    const tail = readFileSync(pointer).subarray(-400).toString("utf-8");
    expect(tail).toContain("# output truncated");
    expect(tail).toContain("# finished exit=0");
    expect(permissions(store.home)).toBe(0o700);
    expect(permissions(path.join(store.home, "ultradian.db"))).toBe(0o600);
    expect(permissions(path.dirname(pointer))).toBe(0o700);
    store.close();
  }, 15_000);
});
