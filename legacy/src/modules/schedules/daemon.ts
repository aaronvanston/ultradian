import {
  appendFileSync,
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import os from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";

import { AppError, ExitCode } from "../../engine/index.ts";
import {
  executeFire,
  groupIsAlive,
  KILL_GRACE_MS,
  terminateGroup,
} from "./runner.ts";
import { isPidAlive, userHome } from "./store.ts";
import type { DaemonInfo, Run, Schedule, ScheduleStore } from "./store.ts";

const HEARTBEAT_STALE_MS = 15_000;
const TICK_MS = 1000;
const STALL_SWEEP_MS = 30_000;
const STALL_CONFIRM_MS = 60_000;
const RETENTION_SWEEP_MS = 86_400_000;

export const daemonIsLive = (info: DaemonInfo | undefined): boolean =>
  info !== undefined &&
  isPidAlive(info.pid) &&
  Date.now() - info.heartbeatAt < HEARTBEAT_STALE_MS;

const sleep = async (ms: number, signal?: AbortSignal): Promise<void> => {
  try {
    await delay(ms, undefined, signal === undefined ? {} : { signal });
  } catch {
    // An aborted sleep just returns early.
  }
};

// Reaps active runs whose process evidence says they can never finish: the
// owning process is gone (a killed manual run), or the child the run is
// waiting on is gone while its owner survives (a sleep killed the action but
// left the daemon wedged on its pipes). A run is reaped only after a second
// sweep confirms the pid stayed dead past the confirmation window, so a run
// finishing normally, including one still draining output after child exit,
// is never raced. Runs with no recorded child stay untouched unless their
// owner dies; absence of evidence reaps nothing.
export const sweepStalledRuns = (options: {
  store: ScheduleStore;
  suspects: Map<string, number>;
  nowMs: number;
  confirmMs: number;
  daemonPid: number;
  log: (line: string) => void;
}): number => {
  const { confirmMs, daemonPid, log, nowMs, store, suspects } = options;
  const active = store.activeRuns();
  const activeIds = new Set(active.map((run) => run.id));
  for (const id of suspects.keys()) {
    if (!activeIds.has(id)) {
      suspects.delete(id);
    }
  }
  let reaped = 0;
  for (const run of active) {
    const ownerGone = run.ownerPid !== daemonPid && !isPidAlive(run.ownerPid);
    const childGone = run.pgid !== null && !isPidAlive(run.pgid);
    if (!ownerGone && !childGone) {
      suspects.delete(run.id);
      continue;
    }
    const firstSeen = suspects.get(run.id);
    if (firstSeen === undefined) {
      suspects.set(run.id, nowMs);
      continue;
    }
    if (nowMs - firstSeen < confirmMs) {
      continue;
    }
    store.finishRun(run.id, {
      actionExit: run.actionExit,
      gateExit: run.gateExit,
      status: "interrupted",
    });
    suspects.delete(run.id);
    reaped += 1;
    log(
      `reap ${run.scheduleName} ${run.id}: ${ownerGone ? "owner" : "child"} pid gone`
    );
  }
  return reaped;
};

// The foreground daemon loop: heartbeat, claim due schedules, fire them.
// Fires run concurrently; a schedule with a run still in flight is skipped
// and the skip is recorded so the gap is visible in its history. A tick
// that throws is logged and the loop carries on. On shutdown every fire in
// flight has its process group terminated and is recorded as interrupted,
// so a stopped daemon never leaves runs behind.
export const runDaemonLoop = async (options: {
  store: ScheduleStore;
  signal: AbortSignal;
  version: string;
  retentionMs: number | null;
  log: (line: string) => void;
}): Promise<void> => {
  const { log, retentionMs, signal, store, version } = options;
  const startedAt = Date.now();
  const holder = store.claimDaemon(
    { pid: process.pid, startedAt, version },
    daemonIsLive
  );
  if (holder !== undefined) {
    throw new AppError({
      code: "daemon_already_running",
      exitCode: ExitCode.TEMPFAIL,
      hint: "Stop it with 'daemon stop' first.",
      message: `A daemon is already running (pid ${holder.pid}).`,
    });
  }

  const recovered = store.recover();
  for (const pgid of recovered.orphanGroups) {
    if (groupIsAlive(pgid)) {
      log(`terminating orphaned process group ${pgid}`);
      void terminateGroup(pgid);
    }
  }
  log(
    `daemon started pid=${process.pid} version=${version} interrupted=${recovered.interrupted} swept=${recovered.swept}`
  );

  // Aborted on shutdown: every executeFire in flight terminates its group.
  const shutdown = new AbortController();
  const inflight = new Set<Promise<unknown>>();
  const suspects = new Map<string, number>();
  let lastSweepAt = startedAt;
  let lastPruneAt = 0;

  const launch = (schedule: Schedule, run: Run): void => {
    log(`fire ${schedule.name} ${run.id} trigger=${run.trigger}`);
    const fire = (async (): Promise<void> => {
      try {
        const finished = await executeFire({
          run,
          schedule,
          signal: shutdown.signal,
          store,
        });
        log(`done ${schedule.name} ${run.id} status=${finished.status}`);
      } catch (error) {
        log(`fire ${schedule.name} failed: ${String(error)}`);
      } finally {
        // The job row is consumed by its one fire; the run record is what
        // survives.
        if (schedule.kind === "once") {
          store.removeJob(schedule.id);
        }
      }
    })();
    inflight.add(fire);
    // oxlint-disable-next-line promise/prefer-await-to-then -- completion tracking must not block the tick loop.
    void fire.then(() => inflight.delete(fire));
  };

  const tick = (): boolean => {
    if (!store.heartbeat(process.pid)) {
      log("another daemon took over the lock; stopping");
      return false;
    }
    if (Date.now() - lastSweepAt >= STALL_SWEEP_MS) {
      lastSweepAt = Date.now();
      sweepStalledRuns({
        confirmMs: STALL_CONFIRM_MS,
        daemonPid: process.pid,
        log,
        nowMs: lastSweepAt,
        store,
        suspects,
      });
    }
    if (
      retentionMs !== null &&
      Date.now() - lastPruneAt >= RETENTION_SWEEP_MS
    ) {
      lastPruneAt = Date.now();
      const pruned = store.pruneHistory(lastPruneAt - retentionMs);
      if (pruned.removed > 0) {
        log(
          `pruned ${pruned.removed} run(s) older than ${retentionMs}ms, freed ${pruned.freedBytes} bytes`
        );
      }
    }
    for (const { run, schedule } of store.claimQueued(process.pid)) {
      launch(schedule, run);
    }
    const { due, missed } = store.claimDue(Date.now());
    for (const schedule of missed) {
      log(
        `missed ${schedule.name}: beyond its catch-up window, skipped forward`
      );
    }
    for (const schedule of due) {
      const trigger = schedule.kind === "once" ? "once" : "scheduled";
      const run = store.beginRun({ schedule, trigger });
      if (run === undefined) {
        store.recordRun({ schedule, status: "skipped", trigger });
        log(`skip ${schedule.name}: previous run still in flight`);
        continue;
      }
      launch(schedule, run);
    }
    return true;
  };

  let holding = true;
  while (!signal.aborted && holding) {
    try {
      holding = tick();
    } catch (error) {
      log(
        `tick failed: ${error instanceof Error ? error.message : String(error)}`
      );
    }
    // oxlint-disable-next-line no-await-in-loop -- the tick loop is deliberately sequential.
    await sleep(TICK_MS, signal);
  }

  if (inflight.size > 0) {
    log(`shutting down, interrupting ${inflight.size} run(s)`);
    shutdown.abort();
    await Promise.race([
      Promise.allSettled(inflight),
      sleep(KILL_GRACE_MS + 5000),
    ]);
  }
  if (holding) {
    store.clearDaemon(process.pid);
  }
  log("daemon stopped");
};

// Re-invoke this CLI. Compiled binaries embed their entrypoint, so execPath
// alone is the command; under 'bun src/index.ts' the script path is needed.
const selfCommand = (): string[] =>
  Bun.main.startsWith("/$bunfs")
    ? [process.execPath]
    : [process.execPath, Bun.main];

const DAEMON_LOG_ROTATE_BYTES = 5_000_000;
const DAEMON_LOG_KEEP = 3;

// The daemon owns its log file and rotates it by size, so no supervisor's
// open descriptor ever points at a rotated file. daemon.log holds lifecycle
// lines; daemon.log.1 to .3 hold older ones. Mirrors to stderr when a
// person is watching a foreground daemon.
export const openDaemonLog = (
  home: string,
  mirror: boolean
): ((line: string) => void) => {
  const file = path.join(home, "daemon.log");
  let size = statSync(file, { throwIfNoEntry: false })?.size ?? 0;
  return (line) => {
    const text = `${new Date().toISOString()} ${line}\n`;
    if (size + text.length > DAEMON_LOG_ROTATE_BYTES) {
      for (let index = DAEMON_LOG_KEEP - 1; index >= 1; index -= 1) {
        if (existsSync(`${file}.${index}`)) {
          renameSync(`${file}.${index}`, `${file}.${index + 1}`);
        }
      }
      if (existsSync(file)) {
        renameSync(file, `${file}.1`);
      }
      size = 0;
    }
    appendFileSync(file, text, { mode: 0o600 });
    size += text.length;
    if (mirror) {
      process.stderr.write(text);
    }
  };
};

// Where a daemon's own stdout and stderr go when it runs detached or under
// a supervisor: only output the daemon did not write itself, such as a
// crash, lands here.
const daemonOutputPath = (home: string): string =>
  path.join(home, "daemon.out.log");

export const startDaemon = async (
  store: ScheduleStore
): Promise<DaemonInfo> => {
  const existing = store.readDaemon();
  if (daemonIsLive(existing) && existing !== undefined) {
    throw new AppError({
      code: "daemon_already_running",
      exitCode: ExitCode.TEMPFAIL,
      hint: "Check it with 'status' or stop it with 'daemon stop'.",
      message: `A daemon is already running (pid ${existing.pid}).`,
    });
  }
  const logDescriptor = openSync(daemonOutputPath(store.home), "a", 0o600);
  // Its own session, so the daemon outlives the terminal or SSH session
  // that started it instead of dying with that session's hangup.
  const child = Bun.spawn([...selfCommand(), "daemon", "run"], {
    detached: true,
    env: { ...process.env, ULTRADIAN_HOME: store.home },
    stderr: logDescriptor,
    stdin: "ignore",
    stdout: logDescriptor,
  });
  child.unref();
  closeSync(logDescriptor);

  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    const info = store.readDaemon();
    if (info !== undefined && info.pid === child.pid && daemonIsLive(info)) {
      return info;
    }
    // oxlint-disable-next-line no-await-in-loop -- polling for daemon startup.
    await sleep(100);
  }
  throw new AppError({
    code: "daemon_start_timeout",
    exitCode: ExitCode.TEMPFAIL,
    hint: `Check ${path.join(store.home, "daemon.log")} for details.`,
    message: "The daemon did not report a heartbeat within 5 seconds.",
  });
};

export const stopDaemon = async (
  store: ScheduleStore
): Promise<{ stopped: boolean; pid: number | null }> => {
  const info = store.readDaemon();
  if (info === undefined) {
    return { pid: null, stopped: false };
  }
  if (!isPidAlive(info.pid)) {
    store.clearDaemon(info.pid);
    return { pid: info.pid, stopped: false };
  }
  process.kill(info.pid, "SIGTERM");
  const deadline = Date.now() + KILL_GRACE_MS + 5000;
  // Only pid death counts as stopped. The daemon row disappearing says
  // nothing: a wedged daemon can lose its row while the process survives,
  // and reporting that as stopped is how two daemons end up sharing the
  // database.
  while (Date.now() < deadline) {
    if (!isPidAlive(info.pid)) {
      return { pid: info.pid, stopped: true };
    }
    // oxlint-disable-next-line no-await-in-loop -- polling for daemon shutdown.
    await sleep(100);
  }
  throw new AppError({
    code: "daemon_stop_timeout",
    exitCode: ExitCode.TEMPFAIL,
    hint: `Inspect the process manually: kill -9 ${info.pid}`,
    message: `The daemon (pid ${info.pid}) did not stop in time.`,
  });
};

const launchdLabel = "com.ultradian.daemon";

const launchdPlistPath = (): string =>
  path.join(userHome(), "Library", "LaunchAgents", `${launchdLabel}.plist`);

const systemdUnitPath = (): string =>
  path.join(userHome(), ".config", "systemd", "user", "ultradian.service");

// What a supervised daemon runs with. PATH is spelled out because launchd
// and systemd start services with a bare system PATH, and actions such as
// 'claude' or 'codex' usually live elsewhere.
export interface ServiceSpec {
  program: readonly string[];
  home: string;
  environment: Readonly<Record<string, string>>;
}

const xmlEscape = (value: string): string =>
  value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&apos;");

export const renderLaunchdPlist = (spec: ServiceSpec): string => {
  const program = spec.program
    .map((argument) => `    <string>${xmlEscape(argument)}</string>`)
    .join("\n");
  const environment = Object.entries(spec.environment)
    .map(
      ([key, value]) =>
        `    <key>${xmlEscape(key)}</key>\n    <string>${xmlEscape(value)}</string>`
    )
    .join("\n");
  const logPath = xmlEscape(daemonOutputPath(spec.home));
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${launchdLabel}</string>
  <key>ProgramArguments</key>
  <array>
${program}
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ExitTimeOut</key>
  <integer>30</integer>
  <key>EnvironmentVariables</key>
  <dict>
${environment}
  </dict>
  <key>StandardOutPath</key>
  <string>${logPath}</string>
  <key>StandardErrorPath</key>
  <string>${logPath}</string>
</dict>
</plist>
`;
};

// systemd expands % specifiers everywhere and $ variables in ExecStart, and
// unquotes C-style escapes inside double quotes, so every value is quoted
// and each of those characters is escaped.
const systemdQuote = (value: string, expandsVariables: boolean): string => {
  const escaped = value
    .replaceAll("\\", "\\\\")
    .replaceAll('"', '\\"')
    .replaceAll("%", "%%");
  return `"${expandsVariables ? escaped.replaceAll("$", "$$$$") : escaped}"`;
};

export const renderSystemdUnit = (spec: ServiceSpec): string => {
  const execStart = spec.program
    .map((argument) => systemdQuote(argument, true))
    .join(" ");
  const environment = Object.entries(spec.environment)
    .map(
      ([key, value]) => `Environment=${systemdQuote(`${key}=${value}`, false)}`
    )
    .join("\n");
  return `[Unit]
Description=Ultradian scheduling daemon

[Service]
ExecStart=${execStart}
${environment}
Restart=on-failure
TimeoutStopSec=30

[Install]
WantedBy=default.target
`;
};

// A PATH for a long-lived service: each folder once, in order, and only
// folders that exist now. Version managers such as fnm add per-shell
// folders that vanish later; those that are already gone are dropped here.
export const tidyPath = (value: string): string => {
  const seen = new Set<string>();
  return value
    .split(":")
    .filter((folder) => {
      if (folder === "" || seen.has(folder)) {
        return false;
      }
      seen.add(folder);
      return existsSync(folder);
    })
    .join(":");
};

// The PATH a login shell sets up, where people install their tools.
const loginShellPath = (): string | undefined => {
  const shell = process.env.SHELL ?? "/bin/sh";
  try {
    const result = Bun.spawnSync([shell, "-lc", 'printf %s "$PATH"'], {
      stderr: "ignore",
      stdin: "ignore",
      stdout: "pipe",
      timeout: 5000,
    });
    const value = tidyPath(result.stdout.toString().trim());
    return result.exitCode === 0 && value !== "" ? value : undefined;
  } catch {
    return undefined;
  }
};

const serviceSpec = (
  store: ScheduleStore,
  options: { path?: string | undefined; retention?: string | undefined }
): ServiceSpec => ({
  environment: {
    PATH:
      options.path ?? loginShellPath() ?? process.env.PATH ?? "/usr/bin:/bin",
    ULTRADIAN_HOME: store.home,
    ...(options.retention === undefined
      ? {}
      : { ULTRADIAN_RETENTION: options.retention }),
  },
  home: store.home,
  program: [...selfCommand(), "daemon", "run"],
});

const control = (command: string[], tolerateFailure = false): boolean => {
  const result = Bun.spawnSync(command, { stderr: "pipe", stdout: "pipe" });
  if (result.exitCode !== 0 && !tolerateFailure) {
    throw new AppError({
      code: "daemon_install_failed",
      details: result.stderr.toString().trim(),
      exitCode: ExitCode.TEMPFAIL,
      message: `'${command.join(" ")}' failed with exit ${result.exitCode}.`,
    });
  }
  return result.exitCode === 0;
};

// Waits for a daemon other than the one that was running before.
const waitForLive = async (
  store: ScheduleStore,
  sinceMs: number,
  previousPid: number | null
): Promise<DaemonInfo> => {
  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline) {
    const info = store.readDaemon();
    if (
      info !== undefined &&
      info.pid !== previousPid &&
      info.heartbeatAt >= sinceMs &&
      daemonIsLive(info)
    ) {
      return info;
    }
    // oxlint-disable-next-line no-await-in-loop -- polling for supervised daemon startup.
    await sleep(200);
  }
  throw new AppError({
    code: "daemon_start_timeout",
    exitCode: ExitCode.TEMPFAIL,
    hint: `Check ${path.join(store.home, "daemon.log")} for details.`,
    message: "The supervised daemon did not report a heartbeat in time.",
  });
};

export interface DaemonService {
  platform: "launchd" | "systemd";
  path: string;
  program: readonly string[];
  environment: Readonly<Record<string, string>>;
  content: string;
}

// Renders the service file this machine would get, without touching it.
export const planDaemonService = (
  store: ScheduleStore,
  options: { path?: string | undefined; retention?: string | undefined }
): DaemonService => {
  const spec = serviceSpec(store, options);
  return process.platform === "darwin"
    ? {
        content: renderLaunchdPlist(spec),
        environment: spec.environment,
        path: launchdPlistPath(),
        platform: "launchd",
        program: spec.program,
      }
    : {
        content: renderSystemdUnit(spec),
        environment: spec.environment,
        path: systemdUnitPath(),
        platform: "systemd",
        program: spec.program,
      };
};

const launchdDomain = (): string => `gui/${process.getuid?.() ?? 0}`;

// Registers the daemon with the user's service manager so it survives
// reboots. Any manually started daemon is stopped first so the
// supervised one can take over.
export const installDaemon = async (
  store: ScheduleStore,
  service: DaemonService
): Promise<DaemonInfo> => {
  await stopDaemon(store);
  const installedAt = Date.now();
  mkdirSync(path.dirname(service.path), { recursive: true });
  writeFileSync(service.path, service.content);
  if (service.platform === "launchd") {
    control(
      ["launchctl", "bootout", `${launchdDomain()}/${launchdLabel}`],
      true
    );
    control(["launchctl", "bootstrap", launchdDomain(), service.path]);
  } else {
    control(["systemctl", "--user", "daemon-reload"]);
    control(["systemctl", "--user", "enable", "--now", "ultradian.service"]);
  }
  return await waitForLive(store, installedAt, null);
};

export const serviceInstalled = (): boolean =>
  existsSync(
    process.platform === "darwin" ? launchdPlistPath() : systemdUnitPath()
  );

// Restarts through the service manager when the daemon is installed as a
// service, so the supervisor keeps tracking it, and directly otherwise.
// Either way the new daemon runs whatever binary the program path names
// now, which is what makes 'self install' followed by this an upgrade.
export const restartDaemon = async (
  store: ScheduleStore
): Promise<DaemonInfo> => {
  const previous = store.readDaemon();
  const previousPid = daemonIsLive(previous) ? (previous?.pid ?? null) : null;
  const since = Date.now();
  if (serviceInstalled()) {
    if (process.platform === "darwin") {
      const target = `${launchdDomain()}/${launchdLabel}`;
      if (!control(["launchctl", "kickstart", "-k", target], true)) {
        control([
          "launchctl",
          "bootstrap",
          launchdDomain(),
          launchdPlistPath(),
        ]);
      }
    } else {
      control(["systemctl", "--user", "restart", "ultradian.service"]);
    }
    return await waitForLive(store, since, previousPid);
  }
  await stopDaemon(store);
  return await startDaemon(store);
};

export const uninstallDaemon = async (
  store: ScheduleStore
): Promise<{ platform: "launchd" | "systemd"; path: string }> => {
  if (process.platform === "darwin") {
    const plistPath = launchdPlistPath();
    control(
      ["launchctl", "bootout", `${launchdDomain()}/${launchdLabel}`],
      true
    );
    rmSync(plistPath, { force: true });
    await stopDaemon(store);
    return { path: plistPath, platform: "launchd" };
  }
  const unitPath = systemdUnitPath();
  if (existsSync(unitPath)) {
    control(
      ["systemctl", "--user", "disable", "--now", "ultradian.service"],
      true
    );
    rmSync(unitPath, { force: true });
    control(["systemctl", "--user", "daemon-reload"], true);
  }
  await stopDaemon(store);
  return { path: unitPath, platform: "systemd" };
};

// A systemd user service only outlives the user's last login session,
// SSH included, when lingering is enabled for that user.
export const systemdLinger = (): boolean | undefined => {
  try {
    const result = Bun.spawnSync(
      [
        "loginctl",
        "show-user",
        os.userInfo().username,
        "--property=Linger",
        "--value",
      ],
      { stderr: "ignore", stdin: "ignore", stdout: "pipe", timeout: 5000 }
    );
    if (result.exitCode !== 0) {
      return undefined;
    }
    return result.stdout.toString().trim() === "yes";
  } catch {
    return undefined;
  }
};

// Copies the running compiled binary to a stable path atomically: write a
// sibling temp file, make it executable, rename it over the target. A
// daemon running the old binary keeps its open inode and moves to the new
// one on its next restart.
export const installSelf = (target: string): { replaced: boolean } => {
  if (!Bun.main.startsWith("/$bunfs")) {
    throw new AppError({
      code: "not_a_compiled_binary",
      exitCode: ExitCode.USAGE,
      hint: "Build one with 'bun run build' and run 'self install' from it.",
      message: "Only a compiled ultradian binary can install itself.",
    });
  }
  const destination = path.resolve(target);
  mkdirSync(path.dirname(destination), { mode: 0o700, recursive: true });
  const replaced = existsSync(destination);
  const staging = path.join(
    path.dirname(destination),
    `.${path.basename(destination)}.${process.pid}.tmp`
  );
  try {
    // Byte copy, not clonefile, so no quarantine attribute travels along.
    writeFileSync(staging, readFileSync(process.execPath), { mode: 0o755 });
    renameSync(staging, destination);
  } finally {
    rmSync(staging, { force: true });
  }
  return { replaced };
};
