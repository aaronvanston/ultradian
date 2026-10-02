import { statSync } from "node:fs";
import path from "node:path";

import { z } from "zod";

import { AppError, defineCommand, ExitCode } from "../../engine/index.ts";
import {
  daemonIsLive,
  installDaemon,
  installSelf,
  openDaemonLog,
  planDaemonService,
  restartDaemon,
  runDaemonLoop,
  startDaemon,
  stopDaemon,
  uninstallDaemon,
} from "./daemon.ts";
import { createJobName } from "./ids.ts";
import { executeFire } from "./runner.ts";
import { resolveHome, scheduleStore } from "./store.ts";
import type { DaemonInfo, Run, Schedule } from "./store.ts";
import {
  describeTrigger,
  formatSeconds,
  nextFireAt,
  parseDuration,
  parseTrigger,
} from "./triggers.ts";

const iso = (ms: number | null): string | null =>
  ms === null ? null : new Date(ms).toISOString();

const relative = (ms: number, now = Date.now()): string => {
  const delta = ms - now;
  const magnitude = Math.abs(delta);
  const units: readonly [number, string][] = [
    [86_400_000, "d"],
    [3_600_000, "h"],
    [60_000, "m"],
    [1000, "s"],
  ];
  const [size, label] = units.find(([value]) => magnitude >= value) ?? [
    1000,
    "s",
  ];
  const amount = Math.max(1, Math.round(magnitude / size));
  return delta >= 0 ? `in ${amount}${label}` : `${amount}${label} ago`;
};

const runStatusSchema = z.enum([
  "queued",
  "running",
  "clean",
  "succeeded",
  "failed",
  "gate_failed",
  "timed_out",
  "canceled",
  "interrupted",
  "skipped",
  "missed",
]);

const runRecordSchema = z.object({
  action_exit: z.number().nullable(),
  cwd: z.string().nullable(),
  executor: z.string().nullable(),
  finished_at: z.string().nullable(),
  gate_exit: z.number().nullable(),
  log_pointer: z.string().nullable(),
  machine_id: z.string(),
  pgid: z.number().nullable(),
  run_id: z.string(),
  schedule: z.string(),
  schedule_id: z.string(),
  started_at: z.string(),
  status: runStatusSchema,
  trigger: z.enum(["scheduled", "manual", "once"]),
});

type RunRecord = z.infer<typeof runRecordSchema>;

const toRunRecord = (run: Run): RunRecord => ({
  action_exit: run.actionExit,
  cwd: run.workingDirectory,
  executor: run.executor,
  finished_at: iso(run.finishedAt),
  gate_exit: run.gateExit,
  log_pointer: run.logPointer,
  machine_id: run.machineId,
  pgid: run.pgid,
  run_id: run.id,
  schedule: run.scheduleName,
  schedule_id: run.scheduleId,
  started_at: iso(run.startedAt) ?? "",
  status: run.status,
  trigger: run.trigger,
});

const triggerSchema = z.discriminatedUnion("kind", [
  z.object({
    expression: z.string(),
    kind: z.literal("cron"),
    timezone: z.string().nullable(),
  }),
  z.object({ kind: z.literal("every"), seconds: z.number() }),
  z.object({ kind: z.literal("manual") }),
]);

const scheduleRecordSchema = z.object({
  active: z.boolean(),
  catch_up_seconds: z.number(),
  command: z.array(z.string()),
  created_at: z.string(),
  cwd: z.string(),
  gate: z.string().nullable(),
  gate_mode: z.enum(["output", "exit"]),
  group: z.string().nullable(),
  id: z.string(),
  name: z.string(),
  next_fire_at: z.string().nullable(),
  timeout_seconds: z.number().nullable(),
  trigger: triggerSchema,
  updated_at: z.string(),
});

type ScheduleRecord = z.infer<typeof scheduleRecordSchema>;

const seconds = (ms: number | null): number | null =>
  ms === null ? null : ms / 1000;

const toScheduleRecord = (schedule: Schedule): ScheduleRecord => ({
  active: schedule.status === "active",
  catch_up_seconds: schedule.catchUpMs / 1000,
  command: [...schedule.command],
  created_at: iso(schedule.createdAt) ?? "",
  cwd: schedule.workingDirectory,
  gate: schedule.gate,
  gate_mode: schedule.gateMode,
  group: schedule.group,
  id: schedule.id,
  name: schedule.name,
  next_fire_at: iso(schedule.nextFireAt),
  timeout_seconds: seconds(schedule.timeoutMs),
  trigger: schedule.trigger,
  updated_at: iso(schedule.updatedAt) ?? "",
});

const requireCommand = (value: unknown, example: string): string[] => {
  const parsed = z.array(z.string()).safeParse(value);
  const command = parsed.success ? parsed.data : [];
  if (command.length === 0) {
    throw new AppError({
      code: "command_required",
      exitCode: ExitCode.USAGE,
      hint: `Example: ${example}`,
      message: "A command to run is required after --.",
    });
  }
  return command;
};

const namePattern = /^[a-zA-Z0-9][a-zA-Z0-9._-]*$/u;

const requireName = (value: unknown): string => {
  const name = typeof value === "string" ? value.trim() : "";
  if (!namePattern.test(name)) {
    throw new AppError({
      code: "invalid_schedule_name",
      exitCode: ExitCode.USAGE,
      message:
        "Schedule names use letters, digits, dots, dashes, and underscores.",
    });
  }
  return name;
};

const statusSymbol = (
  status: Run["status"],
  ui: {
    success: (v: string) => string;
    danger: (v: string) => string;
    warning: (v: string) => string;
    muted: (v: string) => string;
    info: (v: string) => string;
  }
): string => {
  switch (status) {
    case "succeeded":
    case "clean": {
      return ui.success(status);
    }
    case "failed":
    case "gate_failed":
    case "timed_out": {
      return ui.danger(status);
    }
    case "canceled":
    case "interrupted":
    case "skipped":
    case "missed": {
      return ui.warning(status);
    }
    case "running":
    case "queued": {
      return ui.info(status);
    }
    default: {
      return ui.muted(status);
    }
  }
};

const confirm = async (
  message: string,
  signal: AbortSignal
): Promise<boolean> => {
  const prompts = await import("@clack/prompts");
  const answer = await prompts.confirm({
    initialValue: false,
    input: process.stdin,
    message,
    output: process.stderr,
    signal,
  });
  return prompts.isCancel(answer) ? false : answer;
};

const resolveWorkingDirectory = (
  value: string | undefined,
  base: string
): string => {
  if (value === undefined) {
    return base;
  }
  const directory = path.resolve(base, value);
  if (statSync(directory, { throwIfNoEntry: false })?.isDirectory() !== true) {
    throw new AppError({
      code: "invalid_working_directory",
      exitCode: ExitCode.USAGE,
      hint: "Pass a directory that exists to --cwd.",
      message: `No such directory "${value}".`,
    });
  }
  return directory;
};

// ULTRADIAN_RETENTION: how long the daemon keeps run history, as a
// duration, or 'off' to keep everything. Defaults to 30 days.
const parseRetention = (value: string | undefined): number | null => {
  if (value === undefined || value.trim() === "") {
    return parseDuration("30d");
  }
  return value.trim() === "off" ? null : parseDuration(value);
};

const parseCatchUp = (value: string | undefined): number =>
  value === undefined || value.trim() === "0" ? 0 : parseDuration(value);

const addOptions = z.object({
  catchUp: z.string().optional(),
  cron: z.string().optional(),
  cwd: z.string().optional(),
  dryRun: z.boolean().default(false),
  every: z.string().optional(),
  gate: z.string().optional(),
  gateMode: z.enum(["output", "exit"]).default("output"),
  group: z.string().optional(),
  timeout: z.string().optional(),
  tz: z.string().optional(),
  yes: z.boolean().default(false),
});

const addOutput = z.union([
  z.object({ mode: z.literal("plan"), schedule: scheduleRecordSchema }),
  z.object({ mode: z.literal("applied"), schedule: scheduleRecordSchema }),
]);

export const addCommand = defineCommand({
  arguments: [
    { description: "Schedule name", name: "name", required: true },
    {
      description: "Command to run, after --",
      name: "command",
      required: true,
      variadic: true,
    },
  ],
  description:
    "Creates a schedule. With --gate, the gate command runs first on every fire: exit 0 with no stdout records a clean pass, exit 0 with stdout opens the gate and the stdout is piped to the command's stdin, and a nonzero exit records a gate failure.",
  examples: [
    'ultradian add nightly-backup --cron "0 2 * * *" -- ./backup.sh',
    'ultradian add sentry-check --cron "0 * * * *" --gate "bun check-sentry.ts" -- claude -p "Investigate the issues on stdin"',
    "ultradian add heartbeat --every 30s -- echo ok",
    "ultradian add oneshot -- ./task.sh",
  ],
  kind: "write",
  module: "schedules",
  options: [
    { description: "Cron expression trigger", flags: "--cron <expression>" },
    {
      description:
        "IANA time zone for the cron expression, defaulting to the machine's",
      flags: "--tz <zone>",
    },
    {
      description: "Interval trigger such as 30s, 15m, 2h, or 1d",
      flags: "--every <duration>",
    },
    {
      description: "Gate command that decides whether the command runs",
      flags: "--gate <command>",
    },
    {
      choices: ["output", "exit"],
      defaultValue: "output",
      description:
        "When the gate opens: 'output' on exit 0 with stdout, 'exit' on exit 0 alone",
      flags: "--gate-mode <mode>",
    },
    {
      description: "Kill the gate or command after this long (such as 30m)",
      flags: "--timeout <duration>",
    },
    {
      description:
        "Fire once after downtime if no later than this (such as 2h); 0 records a missed run instead",
      flags: "--catch-up <duration>",
    },
    {
      description:
        "Directory the gate and command run in, defaulting to the current one",
      flags: "--cwd <dir>",
    },
    {
      description: "Group this schedule with others under one heading",
      flags: "--group <name>",
    },
    { description: "Show the schedule without saving it", flags: "--dry-run" },
    { description: "Confirm without prompting", flags: "-y, --yes" },
  ],
  optionsSchema: addOptions,
  outputSchema: addOutput,
  path: ["add"],
  render(data, context) {
    const record = data.schedule;
    const lines = [
      `${context.ui.muted("name")}     ${record.name}`,
      ...(record.group === null
        ? []
        : [`${context.ui.muted("group")}    ${record.group}`]),
      `${context.ui.muted("trigger")}  ${describeTrigger(record.trigger)}`,
      ...(record.gate === null
        ? []
        : [`${context.ui.muted("gate")}     ${record.gate}`]),
      `${context.ui.muted("command")}  ${record.command.join(" ")}`,
      ...(record.next_fire_at === null
        ? []
        : [
            `${context.ui.muted("next")}     ${relative(
              Date.parse(record.next_fire_at)
            )}`,
          ]),
    ];
    if (data.mode === "plan") {
      return [
        `${context.ui.info(context.ui.symbols.pending)} ${context.ui.heading(
          "Dry run"
        )}`,
        ...lines,
      ].join("\n");
    }
    return [
      `${context.ui.success(context.ui.symbols.success)} Added ${context.ui.command(
        record.name
      )}`,
      ...lines,
    ].join("\n");
  },
  async run(context) {
    const name = requireName(context.arguments.name);
    const command = requireCommand(
      context.arguments.command,
      'add backup --cron "0 2 * * *" -- ./backup.sh'
    );
    const trigger = parseTrigger({
      cron: context.options.cron,
      every: context.options.every,
      tz: context.options.tz,
    });
    const input = {
      catchUpMs: parseCatchUp(context.options.catchUp),
      command,
      gate: context.options.gate ?? null,
      gateMode: context.options.gateMode,
      group:
        context.options.group === undefined
          ? null
          : requireName(context.options.group),
      name,
      timeoutMs:
        context.options.timeout === undefined
          ? null
          : parseDuration(context.options.timeout),
      trigger,
      workingDirectory: resolveWorkingDirectory(
        context.options.cwd,
        context.cwd
      ),
    };
    if (context.options.dryRun) {
      const preview: Schedule = {
        ...input,
        createdAt: Date.now(),
        id: "schedule_preview",
        kind: "schedule",
        nextFireAt: nextFireAt(trigger, Date.now()),
        status: "active",
        updatedAt: Date.now(),
      };
      return {
        data: { mode: "plan" as const, schedule: toScheduleRecord(preview) },
        hint: `Apply with '${context.app.meta.name} add ${name} ... --yes'.`,
      };
    }
    const confirmed =
      context.options.yes ||
      (context.interactive
        ? await confirm(`Add the schedule "${name}"?`, context.signal)
        : false);
    if (!confirmed) {
      throw new AppError({
        code: context.interactive ? "action_canceled" : "action_required",
        exitCode: context.interactive ? ExitCode.ERROR : ExitCode.USAGE,
        hint: "Preview with '--dry-run' or apply with '--yes'.",
        message: context.interactive
          ? "The schedule was not added."
          : "This command needs explicit confirmation in non-interactive mode.",
      });
    }
    const store = await context.services.get(scheduleStore);
    const schedule = store.addSchedule(input);
    return {
      data: { mode: "applied" as const, schedule: toScheduleRecord(schedule) },
      ...(daemonIsLive(store.readDaemon())
        ? {}
        : {
            hint: `The daemon is not running; start it with '${context.app.meta.name} daemon start'.`,
          }),
    };
  },
  summary: "Add a schedule with an optional gate",
});

const onceOptions = z.object({
  cwd: z.string().optional(),
  name: z.string().optional(),
  timeout: z.string().optional(),
});

const onceRecordSchema = z.object({
  command: z.array(z.string()),
  created_at: z.string(),
  cwd: z.string(),
  job: z.string(),
  job_id: z.string(),
  timeout_seconds: z.number().nullable(),
});

export const onceCommand = defineCommand({
  arguments: [
    {
      description: "Command to run, after --",
      name: "command",
      required: true,
      variadic: true,
    },
  ],
  description:
    "Registers a one-shot job and returns its identity immediately, without waiting. The daemon runs it once, as soon as it can, and records the outcome as an ordinary run: the same statuses, the same captured log, and the same appearance in 'runs', where the job id is the record's schedule_id and the trigger reads as 'once'. A job registered while the daemon is stopped runs when the daemon returns. One-shot jobs have no gate, and they never appear in 'list'.",
  examples: [
    "ultradian once -- ./deploy.sh",
    "ultradian once --name import --timeout 30m -- ./import.sh",
    "ultradian once --cwd ~/work/api --json -- bun run migrate",
  ],
  kind: "write",
  module: "schedules",
  options: [
    {
      description: "Label the job so its run is easy to spot",
      flags: "--name <label>",
    },
    {
      description: "Directory to run in, defaulting to the current one",
      flags: "--cwd <dir>",
    },
    {
      description: "Kill the command after this long (such as 30m)",
      flags: "--timeout <duration>",
    },
  ],
  optionsSchema: onceOptions,
  outputSchema: onceRecordSchema,
  path: ["once"],
  render(data, context) {
    return [
      `${context.ui.success(context.ui.symbols.success)} Queued ${context.ui.command(
        data.job
      )}`,
      `${context.ui.muted("job")}      ${data.job_id}`,
      `${context.ui.muted("command")}  ${data.command.join(" ")}`,
      `${context.ui.muted("cwd")}      ${data.cwd}`,
      ...(data.timeout_seconds === null
        ? []
        : [
            `${context.ui.muted("timeout")}  ${formatSeconds(data.timeout_seconds)}`,
          ]),
    ].join("\n");
  },
  async run(context) {
    const command = requireCommand(
      context.arguments.command,
      "once -- ./deploy.sh"
    );
    const store = await context.services.get(scheduleStore);
    const job = store.addOnce({
      command,
      name: createJobName(
        context.options.name ?? path.basename(command[0] ?? "job")
      ),
      timeoutMs:
        context.options.timeout === undefined
          ? null
          : parseDuration(context.options.timeout),
      workingDirectory: resolveWorkingDirectory(
        context.options.cwd,
        context.cwd
      ),
    });
    return {
      data: {
        command: [...job.command],
        created_at: iso(job.createdAt) ?? "",
        cwd: job.workingDirectory,
        job: job.name,
        job_id: job.id,
        timeout_seconds: seconds(job.timeoutMs),
      },
      ...(daemonIsLive(store.readDaemon())
        ? {}
        : {
            hint: `The daemon is not running; this job waits until it starts with '${context.app.meta.name} daemon start'.`,
          }),
    };
  },
  summary: "Run a command once under the daemon and record the outcome",
});

const runsOutput = z.object({
  cursor: z.string(),
  runs: z.array(runRecordSchema),
});

const parseCursor = (value: string): number => {
  if (!/^\d+$/u.test(value)) {
    throw new AppError({
      code: "invalid_cursor",
      exitCode: ExitCode.USAGE,
      hint: "Pass the cursor a previous 'runs' call returned, or omit --since to start from the beginning.",
      message: `"${value}" is not a runs cursor.`,
    });
  }
  return Number(value);
};

export const runsCommand = defineCommand({
  description:
    "Emits run records for other tools. This is the read surface: consumers ingest these records instead of touching the database. Every change to a run (queued, started, finished, canceled) gives it a new revision, and records come oldest change first. The output cursor is the last revision returned; hand it back with --since to receive only runs that changed after it, each in its latest state. Page with --limit. Pruned runs simply stop appearing.",
  examples: [
    "ultradian runs --json",
    "ultradian runs --since 42 --limit 500 --jsonl",
  ],
  module: "schedules",
  options: [
    {
      description: "Only runs that changed after this cursor",
      flags: "--since <cursor>",
    },
    { description: "Maximum runs to emit", flags: "--limit <count>" },
  ],
  optionsSchema: z.object({
    limit: z.coerce.number().int().positive().optional(),
    since: z.string().optional(),
  }),
  outputSchema: runsOutput,
  path: ["runs"],
  render(data, context) {
    return `${data.runs.length} run record(s). Cursor ${data.cursor} for the next call. Use ${context.ui.flag(
      "--json"
    )} or ${context.ui.flag("--jsonl")} for the records.`;
  },
  async run(context) {
    const store = await context.services.get(scheduleStore);
    const since =
      context.options.since === undefined
        ? 0
        : parseCursor(context.options.since);
    const runs = store.exportRuns({
      since,
      ...(context.options.limit === undefined
        ? {}
        : { limit: context.options.limit }),
    });
    return {
      data: {
        cursor: String(runs.at(-1)?.revision ?? since),
        runs: runs.map(toRunRecord),
      },
    };
  },
  summary: "Emit run records for other tools",
});

const listOutput = z.array(
  z.object({
    last_finished_at: z.string().nullable(),
    last_status: runStatusSchema.nullable(),
    schedule: scheduleRecordSchema,
    total_runs: z.number(),
  })
);

export const listCommand = defineCommand({
  aliases: ["ls"],
  examples: ["ultradian list", "ultradian list --json"],
  module: "schedules",
  optionsSchema: z.object({}),
  outputSchema: listOutput,
  path: ["list"],
  render(data, context) {
    if (data.length === 0) {
      return [
        context.ui.muted("No schedules yet."),
        `Add one with ${context.ui.command(
          `${context.app.meta.name} add <name> --cron "0 * * * *" -- <command>`
        )}.`,
      ].join("\n");
    }
    const row = (entry: (typeof data)[number]): unknown[] => [
      entry.schedule.name,
      describeTrigger(entry.schedule.trigger),
      entry.schedule.active ? "active" : context.ui.warning("paused"),
      entry.schedule.gate === null ? "" : "yes",
      entry.schedule.next_fire_at === null
        ? ""
        : relative(Date.parse(entry.schedule.next_fire_at)),
      entry.last_status === null
        ? context.ui.muted("never")
        : `${statusSymbol(entry.last_status, context.ui)}${
            entry.last_finished_at === null
              ? ""
              : ` ${relative(Date.parse(entry.last_finished_at))}`
          }`,
      entry.total_runs === 0 ? context.ui.muted("0") : entry.total_runs,
    ];
    const headers = [
      "NAME",
      "TRIGGER",
      "STATUS",
      "GATE",
      "NEXT",
      "LAST RUN",
      "RUNS",
    ];
    const groups = [
      ...new Set(
        data
          .map((entry) => entry.schedule.group)
          .filter((group): group is string => group !== null)
      ),
    ].toSorted();
    if (groups.length === 0) {
      return context.ui.table(headers, data.map(row));
    }
    const ungrouped = data.filter((entry) => entry.schedule.group === null);
    const sections = [
      ...(ungrouped.length === 0 ? [] : [{ rows: ungrouped.map(row) }]),
      ...groups.map((group) => ({
        rows: data.filter((entry) => entry.schedule.group === group).map(row),
        title: group,
      })),
    ];
    return context.ui.sectionedTable(headers, sections);
  },
  async run(context) {
    const store = await context.services.get(scheduleStore);
    const data = store.listSchedules().map((schedule) => {
      const last = store.lastRun(schedule.name);
      return {
        last_finished_at: iso(last?.finishedAt ?? null),
        last_status: last?.status ?? null,
        schedule: toScheduleRecord(schedule),
        total_runs: store.countRuns(schedule.name),
      };
    });
    return { data };
  },
  summary: "List schedules and their latest run",
});

// Statuses that mean the fire did not do what it was asked to.
const unsuccessful = new Set<Run["status"]>([
  "failed",
  "gate_failed",
  "timed_out",
  "canceled",
  "interrupted",
]);

export const runCommand = defineCommand({
  arguments: [
    { description: "Schedule name or id", name: "name", required: true },
  ],
  description:
    "Fires the schedule now through the same gate and runner the daemon uses. In the foreground it waits for the result and exits nonzero unless the run succeeded or the gate closed cleanly. With --detach it queues the fire for the daemon and returns the queued run straight away. Either way a schedule with a run already in flight is refused with run_in_flight.",
  examples: [
    "ultradian run sentry-check",
    "ultradian run backup --json",
    "ultradian run sentry-check --detach --json",
  ],
  kind: "write",
  module: "schedules",
  options: [
    {
      description: "Queue the fire for the daemon and return its run id",
      flags: "--detach",
    },
  ],
  optionsSchema: z.object({ detach: z.boolean().default(false) }),
  outputSchema: runRecordSchema,
  path: ["run"],
  render(data, context) {
    return [
      `${statusSymbol(data.status, context.ui)} ${context.ui.command(
        data.schedule
      )} ${data.run_id}`,
      ...(data.executor === null
        ? []
        : [`${context.ui.muted("executor")} ${data.executor}`]),
      ...(data.log_pointer === null
        ? []
        : [`${context.ui.muted("log")}      ${data.log_pointer}`]),
    ].join("\n");
  },
  async run(context) {
    const store = await context.services.get(scheduleStore);
    const schedule = store.requireSchedule(requireName(context.arguments.name));
    const begun = store.beginRun({
      queued: context.options.detach,
      schedule,
      trigger: "manual",
    });
    if (begun === undefined) {
      throw new AppError({
        code: "run_in_flight",
        exitCode: ExitCode.TEMPFAIL,
        hint: `Wait for it with '${context.app.meta.name} status', or stop it with '${context.app.meta.name} cancel <run_id>'.`,
        message: `"${schedule.name}" already has a run in flight.`,
      });
    }
    if (context.options.detach) {
      return {
        data: toRunRecord(begun),
        ...(daemonIsLive(store.readDaemon())
          ? {}
          : {
              hint: `The daemon is not running; this run waits until it starts with '${context.app.meta.name} daemon start'.`,
            }),
      };
    }
    const run = await executeFire({
      run: begun,
      schedule,
      signal: context.signal,
      store,
    });
    return {
      data: toRunRecord(run),
      ...(unsuccessful.has(run.status) ? { exitCode: ExitCode.ERROR } : {}),
    };
  },
  summary: "Trigger a schedule now, in the foreground or queued for the daemon",
});

export const cancelCommand = defineCommand({
  arguments: [{ description: "Run id", name: "run_id", required: true }],
  description:
    "Finishes a queued or running run as canceled. The process that owns the fire, the daemon or a foreground 'run', notices within a second and terminates the run's process group: SIGTERM, then SIGKILL after ten seconds. A queued run simply never starts.",
  examples: [
    "ultradian cancel run_abc123",
    "ultradian cancel run_abc123 --json",
  ],
  kind: "write",
  module: "schedules",
  optionsSchema: z.object({}),
  outputSchema: runRecordSchema,
  path: ["cancel"],
  render(data, context) {
    return `${context.ui.success(context.ui.symbols.success)} Canceled ${context.ui.command(
      data.schedule
    )} ${data.run_id}`;
  },
  async run(context) {
    const store = await context.services.get(scheduleStore);
    return {
      data: toRunRecord(store.cancelRun(String(context.arguments.run_id))),
    };
  },
  summary: "Cancel a queued or running run",
});

const statusOutput = z.object({
  active_runs: z.array(runRecordSchema),
  daemon: z.object({
    heartbeat_at: z.string().nullable(),
    live: z.boolean(),
    pid: z.number().nullable(),
    started_at: z.string().nullable(),
    version: z.string().nullable(),
  }),
  schedules: z.object({
    active: z.number(),
    paused: z.number(),
    total: z.number(),
  }),
});

export const statusCommand = defineCommand({
  examples: ["ultradian status", "ultradian status --json"],
  module: "schedules",
  optionsSchema: z.object({}),
  outputSchema: statusOutput,
  path: ["status"],
  render(data, context) {
    const daemonLine = data.daemon.live
      ? `${context.ui.success(context.ui.symbols.active)} daemon running (pid ${
          data.daemon.pid
        }, since ${
          data.daemon.started_at === null
            ? "?"
            : relative(Date.parse(data.daemon.started_at))
        })`
      : `${context.ui.warning(context.ui.symbols.warning)} daemon not running (start it with '${context.app.meta.name} daemon start')`;
    const lines = [
      daemonLine,
      `${context.ui.muted("schedules")} ${data.schedules.active} active, ${data.schedules.paused} paused`,
    ];
    if (data.active_runs.length > 0) {
      lines.push(
        context.ui.heading("Active runs"),
        ...data.active_runs.map(
          (run) =>
            `  ${run.schedule} ${run.run_id} ${context.ui.muted(
              `started ${relative(Date.parse(run.started_at))}`
            )}`
        )
      );
    }
    return lines.join("\n");
  },
  async run(context) {
    const store = await context.services.get(scheduleStore);
    const daemon = store.readDaemon();
    const live = daemonIsLive(daemon);
    const schedules = store.listSchedules();
    return {
      data: {
        active_runs: [...store.activeRuns(), ...store.queuedRuns()].map(
          toRunRecord
        ),
        daemon: {
          heartbeat_at: iso(daemon?.heartbeatAt ?? null),
          live,
          pid: daemon?.pid ?? null,
          started_at: iso(daemon?.startedAt ?? null),
          version: daemon?.version ?? null,
        },
        schedules: {
          active: schedules.filter((s) => s.status === "active").length,
          paused: schedules.filter((s) => s.status === "paused").length,
          total: schedules.length,
        },
      },
    };
  },
  summary: "Show daemon health and active runs",
});

const logsOptions = z.object({
  limit: z.coerce.number().int().min(1).max(200).default(10),
  run: z.string().optional(),
});

const logsOutput = z.object({
  log: z.object({ content: z.string(), run_id: z.string() }).nullable(),
  runs: z.array(runRecordSchema),
  total_runs: z.number(),
});

export const logsCommand = defineCommand({
  arguments: [
    { description: "Schedule name or id", name: "name", required: false },
  ],
  examples: [
    "ultradian logs",
    "ultradian logs sentry-check",
    "ultradian logs sentry-check --run run_abc123 --json",
  ],
  module: "schedules",
  options: [
    {
      defaultValue: 10,
      description: "Maximum runs to show",
      flags: "--limit <count>",
    },
    {
      description: "Include the captured log for one run id",
      flags: "--run <id>",
    },
  ],
  optionsSchema: logsOptions,
  outputSchema: logsOutput,
  path: ["logs"],
  render(data, context) {
    if (data.log !== null) {
      return data.log.content;
    }
    if (data.runs.length === 0) {
      return context.ui.muted("No runs recorded yet.");
    }
    return [
      context.ui.table(
        ["RUN", "SCHEDULE", "STATUS", "TRIGGER", "STARTED", "EXECUTOR"],
        data.runs.map((run) => [
          run.run_id,
          run.schedule,
          statusSymbol(run.status, context.ui),
          run.trigger,
          relative(Date.parse(run.started_at)),
          run.executor ?? "",
        ])
      ),
      "",
      context.ui.muted(
        `Showing ${data.runs.length} of ${data.total_runs} recorded runs. Read one run's output with '${context.app.meta.name} logs <name> --run <id>'.`
      ),
    ].join("\n");
  },
  async run(context) {
    const store = await context.services.get(scheduleStore);
    const nameValue = context.arguments.name;
    const scheduleName =
      nameValue === undefined
        ? undefined
        : store.requireSchedule(requireName(nameValue)).name;
    if (context.options.run !== undefined) {
      const run = store.getRun(context.options.run);
      if (run === undefined) {
        throw new AppError({
          code: "run_not_found",
          exitCode: ExitCode.ERROR,
          message: `No run with id "${context.options.run}".`,
        });
      }
      const content =
        run.logPointer === null
          ? ""
          : await Bun.file(run.logPointer)
              .text()
              .catch(() => "");
      return {
        data: {
          log: { content, run_id: run.id },
          runs: [toRunRecord(run)],
          total_runs: store.countRuns(run.scheduleName),
        },
      };
    }
    const runs = store.listRuns({
      limit: context.options.limit,
      ...(scheduleName === undefined ? {} : { scheduleName }),
    });
    return {
      data: {
        log: null,
        runs: runs.map(toRunRecord),
        total_runs: store.countRuns(scheduleName),
      },
    };
  },
  summary: "Show recent runs and their captured output",
});

const pruneOptions = z.object({
  olderThan: z.string(),
  yes: z.boolean().default(false),
});

export const pruneCommand = defineCommand({
  arguments: [
    {
      description: "Limit pruning to one schedule",
      name: "name",
      required: false,
    },
  ],
  description:
    "Deletes finished runs that started before the cutoff, along with their captured log files. Queued and running runs and the schedules themselves are untouched. The daemon also prunes once a day by itself, keeping ULTRADIAN_RETENTION (default 30d, 'off' to keep everything).",
  examples: [
    "ultradian prune --older-than 30d --yes",
    "ultradian prune sentry-check --older-than 7d --yes",
  ],
  kind: "write",
  module: "schedules",
  options: [
    {
      description: "Delete runs older than this (such as 7d or 12h)",
      flags: "--older-than <duration>",
    },
    { description: "Confirm without prompting", flags: "-y, --yes" },
  ],
  optionsSchema: pruneOptions,
  outputSchema: z.object({
    freed_bytes: z.number(),
    removed_runs: z.number(),
  }),
  path: ["prune"],
  render(data, context) {
    if (data.removed_runs === 0) {
      return context.ui.muted("Nothing old enough to prune.");
    }
    const mb = (data.freed_bytes / 1_000_000).toFixed(1);
    return `${context.ui.success(context.ui.symbols.success)} Pruned ${data.removed_runs} run(s), freed ${mb}MB of logs`;
  },
  async run(context) {
    const nameValue = context.arguments.name;
    const scheduleName =
      nameValue === undefined ? undefined : requireName(nameValue);
    const cutoff = Date.now() - parseDuration(context.options.olderThan);
    const confirmed =
      context.options.yes ||
      (context.interactive
        ? await confirm(
            `Delete ${
              scheduleName === undefined ? "all" : `${scheduleName}'s`
            } runs older than ${context.options.olderThan}?`,
            context.signal
          )
        : false);
    if (!confirmed) {
      throw new AppError({
        code: context.interactive ? "action_canceled" : "action_required",
        exitCode: context.interactive ? ExitCode.ERROR : ExitCode.USAGE,
        hint: "Confirm with '--yes'.",
        message: context.interactive
          ? "Nothing was pruned."
          : "This command needs explicit confirmation in non-interactive mode.",
      });
    }
    const store = await context.services.get(scheduleStore);
    const result = store.pruneHistory(cutoff, scheduleName);
    return {
      data: { freed_bytes: result.freedBytes, removed_runs: result.removed },
    };
  },
  summary: "Delete old runs and their log files",
});

const setOptions = z.object({
  catchUp: z.string().optional(),
  cron: z.string().optional(),
  cwd: z.string().optional(),
  every: z.string().optional(),
  gate: z.union([z.string(), z.literal(false)]).optional(),
  gateMode: z.enum(["output", "exit"]).optional(),
  group: z.union([z.string(), z.literal(false)]).optional(),
  manual: z.boolean().optional(),
  timeout: z.union([z.string(), z.literal(false)]).optional(),
  tz: z.string().optional(),
});

// The new trigger, if any flag asks for one. --tz alone re-zones the
// existing cron trigger; 'local' returns it to the machine's time.
const triggerPatch = (
  options: z.infer<typeof setOptions>,
  existing: Schedule["trigger"]
): Schedule["trigger"] | undefined => {
  const tz = options.tz === "local" ? undefined : options.tz;
  if (options.manual === true) {
    if (options.cron !== undefined || options.every !== undefined) {
      throw new AppError({
        code: "conflicting_triggers",
        exitCode: ExitCode.USAGE,
        message: "Use one of --cron, --every, or --manual.",
      });
    }
    return parseTrigger({ tz });
  }
  if (options.cron !== undefined || options.every !== undefined) {
    return parseTrigger({ cron: options.cron, every: options.every, tz });
  }
  if (options.tz !== undefined) {
    if (existing.kind !== "cron") {
      return parseTrigger({ tz: options.tz });
    }
    return parseTrigger({ cron: existing.expression, tz });
  }
  return undefined;
};

export const setCommand = defineCommand({
  arguments: [
    { description: "Schedule name or id", name: "name", required: true },
    {
      description: "New command to run, after --",
      name: "command",
      required: false,
      variadic: true,
    },
  ],
  description:
    "Changes only what you pass; everything else stays as it is. Changing the trigger or its time zone recomputes the next fire from now. Everything after -- replaces the command.",
  examples: [
    "ultradian set sentry-check --every 5m",
    'ultradian set nightly-backup --cron "0 3 * * *" --tz Europe/London',
    "ultradian set sentry-check --timeout 1h --catch-up 2h --group loops",
    "ultradian set sentry-check --no-gate --cwd ~/work/api",
    "ultradian set sentry-check --json -- claude -p 'Investigate the issues on stdin'",
  ],
  kind: "write",
  module: "schedules",
  options: [
    {
      description: "New cron expression trigger",
      flags: "--cron <expression>",
    },
    {
      description: "New interval trigger such as 30s, 15m, 2h, or 1d",
      flags: "--every <duration>",
    },
    { description: "Make it a manual-only schedule", flags: "--manual" },
    {
      description:
        "Time zone for the cron trigger, or 'local' for the machine's",
      flags: "--tz <zone>",
    },
    { description: "New gate command", flags: "--gate <command>" },
    { description: "Remove the gate", flags: "--no-gate" },
    {
      choices: ["output", "exit"],
      description: "When the gate opens: 'output' or 'exit'",
      flags: "--gate-mode <mode>",
    },
    {
      description: "New timeout such as 30m",
      flags: "--timeout <duration>",
    },
    { description: "Remove the timeout", flags: "--no-timeout" },
    {
      description: "New catch-up window such as 2h, or 0",
      flags: "--catch-up <duration>",
    },
    { description: "New group", flags: "--group <name>" },
    { description: "Remove it from its group", flags: "--no-group" },
    { description: "New working directory", flags: "--cwd <dir>" },
  ],
  optionsSchema: setOptions,
  outputSchema: scheduleRecordSchema,
  path: ["set"],
  render(data, context) {
    return [
      `${context.ui.success(context.ui.symbols.success)} Updated ${context.ui.command(
        data.name
      )}`,
      `${context.ui.muted("trigger")}  ${describeTrigger(data.trigger)}`,
      ...(data.next_fire_at === null
        ? []
        : [
            `${context.ui.muted("next")}     ${relative(
              Date.parse(data.next_fire_at)
            )}`,
          ]),
    ].join("\n");
  },
  async run(context) {
    const reference = requireName(context.arguments.name);
    const { options } = context;
    const commandValue = z
      .array(z.string())
      .safeParse(context.arguments.command);
    const command =
      commandValue.success && commandValue.data.length > 0
        ? commandValue.data
        : undefined;
    const store = await context.services.get(scheduleStore);
    const existing = store.requireSchedule(reference);
    const patch: Parameters<typeof store.updateSchedule>[1] = {};
    const trigger = triggerPatch(options, existing.trigger);
    if (trigger !== undefined) {
      patch.trigger = trigger;
    }
    if (options.gate !== undefined) {
      patch.gate = options.gate === false ? null : options.gate;
    }
    if (options.gateMode !== undefined) {
      patch.gateMode = options.gateMode;
    }
    if (options.timeout !== undefined) {
      patch.timeoutMs =
        options.timeout === false ? null : parseDuration(options.timeout);
    }
    if (options.catchUp !== undefined) {
      patch.catchUpMs = parseCatchUp(options.catchUp);
    }
    if (options.group !== undefined) {
      patch.group = options.group === false ? null : requireName(options.group);
    }
    if (options.cwd !== undefined) {
      patch.workingDirectory = resolveWorkingDirectory(
        options.cwd,
        context.cwd
      );
    }
    if (command !== undefined) {
      patch.command = command;
    }
    if (Object.keys(patch).length === 0) {
      throw new AppError({
        code: "nothing_to_set",
        exitCode: ExitCode.USAGE,
        hint: "Pass at least one flag, or a new command after --. See 'describe set'.",
        message: "Nothing to change.",
      });
    }
    return { data: toScheduleRecord(store.updateSchedule(existing.id, patch)) };
  },
  summary: "Change any part of a schedule in place",
});

const toggleOptions = z.object({ group: z.string().optional() });

const toggleCommand = (
  action: "pause" | "resume"
): ReturnType<typeof defineCommand> =>
  defineCommand({
    arguments: [
      { description: "Schedule name or id", name: "name", required: false },
    ],
    examples: [
      `ultradian ${action} sentry-check`,
      `ultradian ${action} --group checks`,
    ],
    kind: "write",
    module: "schedules",
    options: [
      {
        description: `${action === "pause" ? "Pause" : "Resume"} every schedule in a group`,
        flags: "--group <name>",
      },
    ],
    optionsSchema: toggleOptions,
    outputSchema: z.array(scheduleRecordSchema),
    path: [action],
    render(data, context) {
      return data
        .map((record) => {
          const suffix =
            action === "resume" && record.next_fire_at !== null
              ? ` (next ${relative(Date.parse(record.next_fire_at))})`
              : "";
          return `${context.ui.success(context.ui.symbols.success)} ${
            record.name
          } ${record.active ? "active" : "paused"}${suffix}`;
        })
        .join("\n");
    },
    async run(context) {
      const store = await context.services.get(scheduleStore);
      const status = action === "pause" ? "paused" : "active";
      const { group } = context.options;
      if (group !== undefined && context.arguments.name !== undefined) {
        throw new AppError({
          code: "conflicting_targets",
          exitCode: ExitCode.USAGE,
          message: "Give a schedule name or --group, one at a time.",
        });
      }
      if (group !== undefined) {
        return {
          data: store
            .setGroupStatus(requireName(group), status)
            .map(toScheduleRecord),
        };
      }
      const schedule = store.setScheduleStatus(
        requireName(context.arguments.name),
        status
      );
      return { data: [toScheduleRecord(schedule)] };
    },
    summary:
      action === "pause"
        ? "Stop triggering a schedule or group without removing it"
        : "Start triggering a paused schedule or group again",
  });

export const pauseCommand = toggleCommand("pause");
export const resumeCommand = toggleCommand("resume");

const rmOptions = z.object({ yes: z.boolean().default(false) });

export const rmCommand = defineCommand({
  arguments: [
    { description: "Schedule name or id", name: "name", required: true },
  ],
  examples: ["ultradian rm sentry-check --yes"],
  kind: "write",
  module: "schedules",
  options: [{ description: "Confirm without prompting", flags: "-y, --yes" }],
  optionsSchema: rmOptions,
  outputSchema: z.object({ id: z.string(), removed: z.string() }),
  path: ["rm"],
  render(data, context) {
    return `${context.ui.success(context.ui.symbols.success)} Removed ${context.ui.command(
      data.removed
    )} (run history kept)`;
  },
  async run(context) {
    const store = await context.services.get(scheduleStore);
    const { id, name } = store.requireSchedule(
      requireName(context.arguments.name)
    );
    const confirmed =
      context.options.yes ||
      (context.interactive
        ? await confirm(`Remove the schedule "${name}"?`, context.signal)
        : false);
    if (!confirmed) {
      throw new AppError({
        code: context.interactive ? "action_canceled" : "action_required",
        exitCode: context.interactive ? ExitCode.ERROR : ExitCode.USAGE,
        hint: "Confirm with '--yes'.",
        message: context.interactive
          ? "The schedule was not removed."
          : "This command needs explicit confirmation in non-interactive mode.",
      });
    }
    store.removeSchedule(id);
    return { data: { id, removed: name } };
  },
  summary: "Remove a schedule, keeping its run history",
});

const daemonInfoSchema = z.object({
  heartbeat_at: z.string(),
  pid: z.number(),
  started_at: z.string(),
  version: z.string(),
});

const toDaemonRecord = (
  info: DaemonInfo
): z.infer<typeof daemonInfoSchema> => ({
  heartbeat_at: iso(info.heartbeatAt) ?? "",
  pid: info.pid,
  started_at: iso(info.startedAt) ?? "",
  version: info.version,
});

export const daemonStartCommand = defineCommand({
  examples: ["ultradian daemon start"],
  kind: "write",
  module: "schedules",
  optionsSchema: z.object({}),
  outputSchema: daemonInfoSchema,
  path: ["daemon", "start"],
  render(data, context) {
    return `${context.ui.success(context.ui.symbols.active)} daemon running (pid ${data.pid})`;
  },
  async run(context) {
    const store = await context.services.get(scheduleStore);
    return { data: toDaemonRecord(await startDaemon(store)) };
  },
  summary: "Start the scheduling daemon in the background",
});

export const daemonStopCommand = defineCommand({
  examples: ["ultradian daemon stop"],
  kind: "write",
  module: "schedules",
  optionsSchema: z.object({}),
  outputSchema: z.object({ pid: z.number().nullable(), stopped: z.boolean() }),
  path: ["daemon", "stop"],
  render(data, context) {
    return data.stopped
      ? `${context.ui.success(context.ui.symbols.success)} daemon stopped (pid ${data.pid})`
      : context.ui.muted("The daemon was not running.");
  },
  async run(context) {
    const store = await context.services.get(scheduleStore);
    return { data: await stopDaemon(store) };
  },
  summary: "Stop the scheduling daemon",
});

const serviceSchema = z.object({
  environment: z.record(z.string(), z.string()),
  path: z.string(),
  platform: z.enum(["launchd", "systemd"]),
  program: z.array(z.string()),
});

const daemonInstallOutput = z.union([
  serviceSchema.extend({ content: z.string(), mode: z.literal("plan") }),
  serviceSchema.extend({
    daemon: daemonInfoSchema,
    mode: z.literal("applied"),
  }),
]);

export const daemonInstallCommand = defineCommand({
  description:
    "Registers the daemon with launchd (macOS) or systemd (Linux) as a user service, so it starts at login and restarts after crashes. The service runs this binary at the path it was invoked from, so install a stable copy first with 'self install' and run this from it. PATH in the service defaults to your login shell's, so actions like claude or codex resolve as they do in a terminal; ULTRADIAN_HOME and any ULTRADIAN_RETENTION are recorded too. --dry-run prints the service file without writing it. A clean 'daemon stop' stays stopped until the next login or 'daemon restart'.",
  examples: [
    "ultradian daemon install --dry-run",
    "~/.ultradian/bin/udian daemon install --json",
    'ultradian daemon install --path "/opt/homebrew/bin:/usr/bin:/bin"',
  ],
  kind: "write",
  module: "schedules",
  options: [
    {
      description: "PATH for the service, instead of the login shell's",
      flags: "--path <path>",
    },
    {
      description: "Print the service file without installing it",
      flags: "--dry-run",
    },
  ],
  optionsSchema: z.object({
    dryRun: z.boolean().default(false),
    path: z.string().optional(),
  }),
  outputSchema: daemonInstallOutput,
  path: ["daemon", "install"],
  render(data, context) {
    if (data.mode === "plan") {
      return [
        `${context.ui.info(context.ui.symbols.pending)} ${context.ui.heading(
          "Dry run"
        )} ${context.ui.muted(data.path)}`,
        data.content.trimEnd(),
      ].join("\n");
    }
    return [
      `${context.ui.success(context.ui.symbols.active)} daemon supervised by ${data.platform} (pid ${data.daemon.pid})`,
      `${context.ui.muted("service")}  ${data.path}`,
    ].join("\n");
  },
  async run(context) {
    const store = await context.services.get(scheduleStore);
    const service = planDaemonService(store, {
      path: context.options.path,
      retention: context.env.ULTRADIAN_RETENTION,
    });
    const record = {
      environment: service.environment,
      path: service.path,
      platform: service.platform,
      program: [...service.program],
    };
    if (context.options.dryRun) {
      return {
        data: { ...record, content: service.content, mode: "plan" as const },
      };
    }
    const info = await installDaemon(store, service);
    return {
      data: {
        ...record,
        daemon: toDaemonRecord(info),
        mode: "applied" as const,
      },
    };
  },
  summary: "Install the daemon as a login service",
});

export const daemonRestartCommand = defineCommand({
  description:
    "Stops the daemon and starts a fresh one, through launchd or systemd when it is installed as a service. Runs in flight are interrupted. After 'self install' replaces the binary, this is what moves the daemon onto the new version.",
  examples: ["ultradian daemon restart", "ultradian daemon restart --json"],
  kind: "write",
  module: "schedules",
  optionsSchema: z.object({}),
  outputSchema: daemonInfoSchema,
  path: ["daemon", "restart"],
  render(data, context) {
    return `${context.ui.success(context.ui.symbols.active)} daemon running (pid ${data.pid}, ${data.version})`;
  },
  async run(context) {
    const store = await context.services.get(scheduleStore);
    return { data: toDaemonRecord(await restartDaemon(store)) };
  },
  summary: "Restart the daemon, picking up a replaced binary",
});

export const daemonUninstallCommand = defineCommand({
  examples: ["ultradian daemon uninstall"],
  kind: "write",
  module: "schedules",
  optionsSchema: z.object({}),
  outputSchema: z.object({
    path: z.string(),
    platform: z.enum(["launchd", "systemd"]),
  }),
  path: ["daemon", "uninstall"],
  render(data, context) {
    return `${context.ui.success(context.ui.symbols.success)} daemon service removed (${data.platform})`;
  },
  async run(context) {
    const store = await context.services.get(scheduleStore);
    return { data: await uninstallDaemon(store) };
  },
  summary: "Remove the daemon login service and stop the daemon",
});

export const selfInstallCommand = defineCommand({
  description:
    "Copies this compiled binary to a stable path, atomically: it writes a temporary file beside the target and renames it into place, so nothing ever runs a half-written binary. Upgrading is running the new binary's 'self install' and then 'daemon restart'; a service installed from the stable path keeps pointing at it.",
  examples: [
    "./ultradian self install",
    "./ultradian self install --to ~/.ultradian/bin/udian --json",
  ],
  kind: "write",
  module: "schedules",
  options: [
    {
      description:
        "Where to install, defaulting to bin/udian in ULTRADIAN_HOME",
      flags: "--to <path>",
    },
  ],
  optionsSchema: z.object({ to: z.string().optional() }),
  outputSchema: z.object({
    path: z.string(),
    replaced: z.boolean(),
    version: z.string(),
  }),
  path: ["self", "install"],
  render(data, context) {
    return `${context.ui.success(context.ui.symbols.success)} ${
      data.replaced ? "Replaced" : "Installed"
    } ${context.ui.command(data.path)} (${data.version})`;
  },
  run(context) {
    const target =
      context.options.to ?? path.join(resolveHome(context.env), "bin", "udian");
    const { replaced } = installSelf(target);
    return {
      data: {
        path: path.resolve(target),
        replaced,
        version: context.app.meta.version,
      },
      ...(replaced
        ? {
            hint: `A running daemon stays on the old binary until '${context.app.meta.name} daemon restart'.`,
          }
        : {}),
    };
  },
  summary: "Install this binary at a stable path",
});

export const daemonRunCommand = defineCommand({
  description:
    "Runs the daemon loop in the foreground until interrupted. 'daemon start' launches this in the background; running it directly suits supervisors such as launchd or systemd. It writes daemon.log in ULTRADIAN_HOME, rotated at 5MB with three old files kept, and prunes run history older than ULTRADIAN_RETENTION (default 30d, 'off' to keep everything) once a day.",
  examples: ["ultradian daemon run"],
  kind: "write",
  module: "schedules",
  optionsSchema: z.object({}),
  outputSchema: z.object({ stopped: z.literal(true) }),
  path: ["daemon", "run"],
  render(_, context) {
    return context.ui.muted("daemon stopped");
  },
  async run(context) {
    const store = await context.services.get(scheduleStore);
    // The daemon loop is a lifecycle boundary: it writes its own rotated
    // log, mirrored to stderr when someone is watching it in a terminal.
    await runDaemonLoop({
      log: openDaemonLog(store.home, process.stderr.isTTY),
      retentionMs: parseRetention(context.env.ULTRADIAN_RETENTION),
      signal: context.signal,
      store,
      version: context.app.meta.version,
    });
    return { data: { stopped: true as const } };
  },
  summary: "Run the scheduling daemon in the foreground",
});
