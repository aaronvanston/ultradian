import { Database } from "bun:sqlite";
import { afterEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, rmSync, statSync } from "node:fs";
import os from "node:os";
import path from "node:path";

import {
  daemonIsLive,
  openDaemonLog,
  renderLaunchdPlist,
  tidyPath,
  renderSystemdUnit,
  runDaemonLoop,
  sweepStalledRuns,
} from "../daemon.ts";
import { groupIsAlive } from "../runner.ts";
import { ScheduleStore } from "../store.ts";
import type { Run, RunTrigger, Schedule } from "../store.ts";

const homes: string[] = [];

const temporaryStore = (): ScheduleStore => {
  const home = mkdtempSync(path.join(os.tmpdir(), "ultradian-daemon-"));
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

// Permission bits only, without the file type.
const permissions = (file: string): number => statSync(file).mode % 0o1000;

afterEach(() => {
  for (const home of homes.splice(0)) {
    rmSync(home, { force: true, recursive: true });
  }
});

const quiet = (): void => {
  // Sweep logging is asserted through store state, never through output.
};

const addTickSchedule = (store: ScheduleStore): Schedule =>
  store.addSchedule({
    command: ["echo", "ok"],
    gate: null,
    group: null,
    name: "tick",
    timeoutMs: null,
    trigger: { kind: "every", seconds: 1 },
    workingDirectory: "/tmp",
  });

const deadPid = async (): Promise<number> => {
  const child = Bun.spawn(["true"], { stdout: "ignore" });
  await child.exited;
  return child.pid;
};

describe("stall sweep", () => {
  test("reaps a run whose child died, but only after a confirming sweep", async () => {
    const store = temporaryStore();
    const schedule = addTickSchedule(store);
    const run = begin(store, schedule, "scheduled");
    store.setRunProcessGroup(run.id, await deadPid());
    const suspects = new Map<string, number>();
    const base = Date.now();

    const first = sweepStalledRuns({
      confirmMs: 60_000,
      daemonPid: process.pid,
      log: quiet,
      nowMs: base,
      store,
      suspects,
    });
    expect(first).toBe(0);
    expect(store.getRun(run.id)?.status).toBe("running");

    const early = sweepStalledRuns({
      confirmMs: 60_000,
      daemonPid: process.pid,
      log: quiet,
      nowMs: base + 30_000,
      store,
      suspects,
    });
    expect(early).toBe(0);

    const confirmed = sweepStalledRuns({
      confirmMs: 60_000,
      daemonPid: process.pid,
      log: quiet,
      nowMs: base + 61_000,
      store,
      suspects,
    });
    expect(confirmed).toBe(1);
    const reaped = store.getRun(run.id);
    expect(reaped?.status).toBe("interrupted");
    expect(reaped?.finishedAt).not.toBeNull();
    expect(store.hasActiveRun(schedule.id)).toBe(false);
    store.close();
  });

  test("a live child is never suspected", () => {
    const store = temporaryStore();
    const schedule = addTickSchedule(store);
    const run = begin(store, schedule, "scheduled");
    store.setRunProcessGroup(run.id, process.pid);
    const suspects = new Map<string, number>();
    const base = Date.now();
    for (const offset of [0, 61_000, 122_000]) {
      sweepStalledRuns({
        confirmMs: 60_000,
        daemonPid: process.pid,
        log: quiet,
        nowMs: base + offset,
        store,
        suspects,
      });
    }
    expect(store.getRun(run.id)?.status).toBe("running");
    expect(suspects.size).toBe(0);
    store.close();
  });

  test("a run with no recorded child owned by the live daemon stays untouched", () => {
    const store = temporaryStore();
    const schedule = addTickSchedule(store);
    const run = begin(store, schedule, "scheduled");
    const suspects = new Map<string, number>();
    const base = Date.now();
    for (const offset of [0, 61_000]) {
      sweepStalledRuns({
        confirmMs: 60_000,
        daemonPid: process.pid,
        log: quiet,
        nowMs: base + offset,
        store,
        suspects,
      });
    }
    expect(store.getRun(run.id)?.status).toBe("running");
    store.close();
  });

  test("a suspect that finishes normally between sweeps is cleared, never reaped", async () => {
    const store = temporaryStore();
    const schedule = addTickSchedule(store);
    const run = begin(store, schedule, "scheduled");
    store.setRunProcessGroup(run.id, await deadPid());
    const suspects = new Map<string, number>();
    const base = Date.now();
    sweepStalledRuns({
      confirmMs: 60_000,
      daemonPid: process.pid,
      log: quiet,
      nowMs: base,
      store,
      suspects,
    });
    expect(suspects.size).toBe(1);
    store.finishRun(run.id, { actionExit: 0, status: "succeeded" });
    sweepStalledRuns({
      confirmMs: 60_000,
      daemonPid: process.pid,
      log: quiet,
      nowMs: base + 61_000,
      store,
      suspects,
    });
    expect(store.getRun(run.id)?.status).toBe("succeeded");
    expect(suspects.size).toBe(0);
    store.close();
  });

  test("reaps a manual run whose owning process died", async () => {
    const store = temporaryStore();
    const schedule = addTickSchedule(store);
    const run = begin(store, schedule, "manual");
    const gone = await deadPid();
    const raw = new Database(path.join(store.home, "ultradian.db"));
    raw.query("UPDATE runs SET owner_pid = ? WHERE id = ?").run(gone, run.id);
    raw.close();
    const suspects = new Map<string, number>();
    const base = Date.now();
    sweepStalledRuns({
      confirmMs: 60_000,
      daemonPid: process.pid,
      log: quiet,
      nowMs: base,
      store,
      suspects,
    });
    const reaped = sweepStalledRuns({
      confirmMs: 60_000,
      daemonPid: process.pid,
      log: quiet,
      nowMs: base + 61_000,
      store,
      suspects,
    });
    expect(reaped).toBe(1);
    expect(store.getRun(run.id)?.status).toBe("interrupted");
    store.close();
  });

  test("a reaped run keeps its status when the wedged finish arrives late", async () => {
    const store = temporaryStore();
    const schedule = addTickSchedule(store);
    const run = begin(store, schedule, "scheduled");
    store.setRunProcessGroup(run.id, await deadPid());
    const suspects = new Map<string, number>();
    const base = Date.now();
    for (const offset of [0, 61_000]) {
      sweepStalledRuns({
        confirmMs: 60_000,
        daemonPid: process.pid,
        log: quiet,
        nowMs: base + offset,
        store,
        suspects,
      });
    }
    expect(store.getRun(run.id)?.status).toBe("interrupted");
    // The wedged executeFire finally resolves and tries to record success.
    store.finishRun(run.id, { actionExit: 0, status: "succeeded" });
    const settled: Run | undefined = store.getRun(run.id);
    expect(settled?.status).toBe("interrupted");
    store.close();
  });
});

describe("daemon loop", () => {
  test("only one daemon holds the lock, and a displaced one learns it", () => {
    const store = temporaryStore();
    const self = { pid: process.pid, startedAt: Date.now(), version: "test" };
    expect(store.claimDaemon(self, daemonIsLive)).toBeUndefined();
    const rival = { pid: process.ppid, startedAt: Date.now(), version: "test" };
    expect(store.claimDaemon(rival, daemonIsLive)?.pid).toBe(process.pid);
    // A holder whose heartbeat went stale can be displaced, and then its
    // own heartbeat fails, which is its cue to stop.
    expect(store.claimDaemon(rival, () => false)).toBeUndefined();
    expect(store.heartbeat(process.pid)).toBe(false);
    expect(store.heartbeat(process.ppid)).toBe(true);
    store.close();
  });

  test("recovery terminates the process group a dead daemon left running", async () => {
    const store = temporaryStore();
    const schedule = store.addSchedule({
      command: ["sleep", "30"],
      gate: null,
      group: null,
      name: "orphaned",
      timeoutMs: null,
      trigger: { kind: "manual" },
      workingDirectory: "/tmp",
    });
    const run = begin(store, schedule, "scheduled");
    const leftover = Bun.spawn(["sleep", "30"], {
      detached: true,
      stdout: "ignore",
    });
    store.setRunProcessGroup(run.id, leftover.pid);
    const raw = new Database(path.join(store.home, "ultradian.db"));
    raw
      .query("UPDATE runs SET owner_pid = ? WHERE id = ?")
      .run(await deadPid(), run.id);
    raw.close();

    const controller = new AbortController();
    setTimeout(() => {
      controller.abort();
    }, 1500);
    await runDaemonLoop({
      log: quiet,
      retentionMs: null,
      signal: controller.signal,
      store,
      version: "test",
    });
    expect(store.getRun(run.id)?.status).toBe("interrupted");
    await leftover.exited;
    expect(groupIsAlive(leftover.pid)).toBe(false);
    store.close();
  }, 15_000);

  test("shutdown interrupts runs in flight and leaves no processes behind", async () => {
    const store = temporaryStore();
    store.addSchedule({
      command: ["sleep", "30"],
      gate: null,
      group: null,
      name: "long",
      timeoutMs: null,
      trigger: { kind: "every", seconds: 1 },
      workingDirectory: "/tmp",
    });
    const controller = new AbortController();
    const loop = runDaemonLoop({
      log: quiet,
      retentionMs: null,
      signal: controller.signal,
      store,
      version: "test",
    });
    const deadline = Date.now() + 5000;
    while (
      (store.activeRuns()[0]?.pgid ?? null) === null &&
      Date.now() < deadline
    ) {
      // oxlint-disable-next-line no-await-in-loop -- waiting for the daemon to fire.
      await Bun.sleep(100);
    }
    const [inFlight] = store.activeRuns();
    expect(inFlight?.pgid).toBeNumber();
    controller.abort();
    await loop;
    expect(store.getRun(inFlight?.id ?? "")?.status).toBe("interrupted");
    expect(groupIsAlive(inFlight?.pgid ?? 0)).toBe(false);
    expect(store.readDaemon()).toBeUndefined();
    store.close();
  }, 20_000);

  test("the daemon prunes history past its retention", async () => {
    const store = temporaryStore();
    const schedule = store.addSchedule({
      command: ["true"],
      gate: null,
      group: null,
      name: "old",
      timeoutMs: null,
      trigger: { kind: "manual" },
      workingDirectory: "/tmp",
    });
    const stale = begin(store, schedule, "manual");
    store.finishRun(stale.id, { actionExit: 0, status: "succeeded" });
    const fresh = begin(store, schedule, "manual");
    store.finishRun(fresh.id, { actionExit: 0, status: "succeeded" });
    const raw = new Database(path.join(store.home, "ultradian.db"));
    raw
      .query("UPDATE runs SET started_at = ? WHERE id = ?")
      .run(Date.now() - 40 * 86_400_000, stale.id);
    raw.close();
    const controller = new AbortController();
    setTimeout(() => {
      controller.abort();
    }, 500);
    await runDaemonLoop({
      log: quiet,
      retentionMs: 30 * 86_400_000,
      signal: controller.signal,
      store,
      version: "test",
    });
    expect(store.getRun(stale.id)).toBeUndefined();
    expect(store.getRun(fresh.id)?.status).toBe("succeeded");
    store.close();
  });
});

describe("daemon log", () => {
  test("rotates by size and keeps three old files", () => {
    const store = temporaryStore();
    const log = openDaemonLog(store.home, false);
    const line = "x".repeat(100_000);
    for (let index = 0; index < 260; index += 1) {
      log(line);
    }
    const file = path.join(store.home, "daemon.log");
    expect(statSync(file).size).toBeLessThanOrEqual(5_000_000);
    expect(permissions(file)).toBe(0o600);
    for (const suffix of [".1", ".2", ".3"]) {
      expect(existsSync(`${file}${suffix}`)).toBe(true);
    }
    expect(existsSync(`${file}.4`)).toBe(false);
    store.close();
  });
});

describe("service files", () => {
  const spec = {
    environment: {
      PATH: "/opt/tools & more/bin:/usr/bin",
      ULTRADIAN_HOME: "/Users/casey/state 100%",
    },
    home: "/Users/casey/state 100%",
    program: ["/Users/casey/.ultradian/bin/udian", "daemon", "run"],
  };

  test("a plist escapes every value it embeds", () => {
    const plist = renderLaunchdPlist({
      ...spec,
      program: ['/tmp/<odd> "name"/udian', "daemon", "run"],
    });
    expect(plist).toContain(
      "<string>/tmp/&lt;odd&gt; &quot;name&quot;/udian</string>"
    );
    expect(plist).toContain(
      "<string>/opt/tools &amp; more/bin:/usr/bin</string>"
    );
    expect(plist).toContain("<key>PATH</key>");
    expect(plist).not.toContain("& more");
  });

  test("a unit quotes arguments and escapes systemd's specifiers and variables", () => {
    const unit = renderSystemdUnit({
      ...spec,
      program: ['/srv/a dir/udi$an "x"', "daemon", "run"],
    });
    expect(unit).toContain(
      'ExecStart="/srv/a dir/udi$$an \\"x\\"" "daemon" "run"'
    );
    expect(unit).toContain(
      'Environment="ULTRADIAN_HOME=/Users/casey/state 100%%"'
    );
    expect(unit).toContain('Environment="PATH=/opt/tools & more/bin:/usr/bin"');
  });
});

describe("service PATH", () => {
  test("keeps each existing folder once, in order", () => {
    const { home } = temporaryStore();
    expect(tidyPath(`/usr/bin::${home}/gone:/bin:/usr/bin:${home}`)).toBe(
      `/usr/bin:/bin:${home}`
    );
  });
});
