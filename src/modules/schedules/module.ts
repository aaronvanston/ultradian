import { accessSync, constants, readdirSync, statSync } from "node:fs";
import path from "node:path";

import { defineModule } from "../../engine/index.ts";
import {
  addCommand,
  cancelCommand,
  daemonInstallCommand,
  daemonRestartCommand,
  daemonRunCommand,
  daemonStartCommand,
  daemonStopCommand,
  daemonUninstallCommand,
  listCommand,
  logsCommand,
  onceCommand,
  pauseCommand,
  pruneCommand,
  resumeCommand,
  rmCommand,
  runCommand,
  selfInstallCommand,
  runsCommand,
  setCommand,
  statusCommand,
} from "./commands.ts";
import { daemonIsLive, serviceInstalled, systemdLinger } from "./daemon.ts";
import { resolveHome, ScheduleStore, scheduleStoreProvider } from "./store.ts";

export const schedulesModule = defineModule({
  commandGroups: {
    daemon: "Run and manage the scheduling daemon",
    self: "Manage this ultradian binary",
  },
  commands: [
    addCommand,
    onceCommand,
    listCommand,
    runCommand,
    runsCommand,
    cancelCommand,
    statusCommand,
    logsCommand,
    setCommand,
    pauseCommand,
    resumeCommand,
    rmCommand,
    pruneCommand,
    daemonStartCommand,
    daemonStopCommand,
    daemonRestartCommand,
    daemonInstallCommand,
    daemonUninstallCommand,
    daemonRunCommand,
    selfInstallCommand,
  ],
  healthChecks: [
    {
      name: "Data directory",
      run() {
        const home = resolveHome();
        try {
          const store = new ScheduleStore(home);
          store.close();
          accessSync(home, constants.W_OK);
          return {
            detail: home,
            name: "Data directory",
            status: "pass" as const,
          };
        } catch (error) {
          return {
            detail: `${home}: ${
              error instanceof Error ? error.message : String(error)
            }`,
            fix: "Set ULTRADIAN_HOME to a writable directory.",
            name: "Data directory",
            status: "fail" as const,
          };
        }
      },
    },
    {
      name: "Run history",
      run() {
        const store = new ScheduleStore(resolveHome());
        try {
          const total = store.countRuns();
          let bytes = 0;
          const walk = (directory: string): void => {
            for (const entry of readdirSync(directory, {
              withFileTypes: true,
            })) {
              const entryPath = path.join(directory, entry.name);
              if (entry.isDirectory()) {
                walk(entryPath);
              } else {
                bytes += statSync(entryPath).size;
              }
            }
          };
          try {
            walk(path.join(store.home, "logs"));
          } catch {
            // No logs directory yet.
          }
          const size =
            bytes > 1_000_000_000
              ? `${(bytes / 1_000_000_000).toFixed(1)}GB`
              : `${(bytes / 1_000_000).toFixed(1)}MB`;
          const detail = `${total} run(s), ${size} of captured logs`;
          if (bytes > 1_000_000_000) {
            return {
              detail,
              fix: "Reclaim space with 'prune --older-than 30d'.",
              name: "Run history",
              status: "warn" as const,
            };
          }
          return { detail, name: "Run history", status: "pass" as const };
        } finally {
          store.close();
        }
      },
    },
    {
      name: "Login service",
      run() {
        if (!serviceInstalled()) {
          return {
            detail: "not installed; 'daemon start' runs until logout or reboot",
            name: "Login service",
            status: "pass" as const,
          };
        }
        if (process.platform !== "linux") {
          return {
            detail: "installed with launchd",
            name: "Login service",
            status: "pass" as const,
          };
        }
        const linger = systemdLinger();
        if (linger === true) {
          return {
            detail: "installed with systemd, lingering enabled",
            name: "Login service",
            status: "pass" as const,
          };
        }
        return {
          detail:
            linger === false
              ? "installed with systemd, but lingering is off: the daemon stops when your last session ends"
              : "installed with systemd; lingering could not be checked",
          fix: "Run 'loginctl enable-linger $USER'.",
          name: "Login service",
          status: "warn" as const,
        };
      },
    },
    {
      name: "Daemon",
      run() {
        const store = new ScheduleStore(resolveHome());
        try {
          const info = store.readDaemon();
          if (daemonIsLive(info) && info !== undefined) {
            return {
              detail: `running (pid ${info.pid})`,
              name: "Daemon",
              status: "pass" as const,
            };
          }
          return {
            detail: "not running",
            fix: "Start it with 'daemon start'.",
            name: "Daemon",
            status: "warn" as const,
          };
        } finally {
          store.close();
        }
      },
    },
  ],
  id: "schedules",
  services: [scheduleStoreProvider],
  summary: "Own schedules, gates, runs, and the daemon",
});
