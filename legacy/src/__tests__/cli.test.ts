import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import os from "node:os";
import path from "node:path";

import { z } from "zod";

import packageJson from "../../package.json" with { type: "json" };

const root = path.resolve(import.meta.dir, "../..");
const packageVersion = packageJson.version;

const requireBunExecutable = (): string => {
  const executable = Bun.which("bun");
  if (executable === null) {
    throw new Error(
      "The Bun executable is required for CLI integration tests."
    );
  }
  return executable;
};

const bunExecutable = requireBunExecutable();

const homes: string[] = [];

const temporaryHome = (): string => {
  const home = mkdtempSync(path.join(os.tmpdir(), "ultradian-cli-"));
  homes.push(home);
  return home;
};

afterEach(() => {
  for (const home of homes.splice(0)) {
    rmSync(home, { force: true, recursive: true });
  }
});

const runCli = async (
  args: readonly string[],
  home: string,
  envOverrides: Readonly<Record<string, string>> = {}
): Promise<{ exitCode: number; stderr: string; stdout: string }> => {
  const child = Bun.spawn([bunExecutable, "src/index.ts", ...args], {
    cwd: root,
    env: {
      ...process.env,
      CI: "1",
      NO_COLOR: "1",
      ULTRADIAN_HOME: home,
      ...envOverrides,
    },
    stderr: "pipe",
    stdin: "ignore",
    stdout: "pipe",
  });
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ]);
  return { exitCode, stderr, stdout };
};

const parseJson = (value: string): unknown => JSON.parse(value);

const runEnvelopeSchema = z.object({
  data: z.object({
    executor: z.string().nullable(),
    log_pointer: z.string().nullable(),
    run_id: z.string(),
    schedule: z.string(),
    status: z.string(),
    trigger: z.enum(["scheduled", "manual", "once"]),
  }),
  ok: z.literal(true),
});

const runsEnvelopeSchema = z.object({
  data: z.object({
    cursor: z.string(),
    runs: z.array(
      z.object({
        cwd: z.string().nullable(),
        log_pointer: z.string().nullable(),
        machine_id: z.string(),
        run_id: z.string(),
        schedule: z.string(),
        schedule_id: z.string(),
        started_at: z.string(),
        status: z.string(),
        trigger: z.string(),
      })
    ),
  }),
  ok: z.literal(true),
});

const logsEnvelopeSchema = z.object({
  data: z.object({
    log: z.object({ content: z.string(), run_id: z.string() }).nullable(),
    runs: z.array(
      z.object({
        log_pointer: z.string().nullable(),
        run_id: z.string(),
        schedule: z.string(),
        status: z.string(),
        trigger: z.string(),
      })
    ),
    total_runs: z.number(),
  }),
  ok: z.literal(true),
});

const actionErrorSchema = z.object({
  error: z.object({ code: z.string(), hint: z.string() }),
});

describe("CLI integration", () => {
  test("renders concise root help without ANSI", async () => {
    const home = temporaryHome();
    const result = await runCli(["--help"], home);
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toContain("ultradian");
    expect(result.stdout).toContain("add");
    expect(result.stdout).toContain("daemon");
    expect(result.stdout).not.toContain("\u001B[");
  });

  test("emits a machine-readable command catalog", async () => {
    const home = temporaryHome();
    const result = await runCli(["schema", "--json"], home);
    expect(result.exitCode).toBe(0);
    const envelope = z
      .object({
        data: z.object({
          commands: z.array(z.object({ path: z.array(z.string()) })),
          schemaVersion: z.literal(2),
        }),
        ok: z.literal(true),
      })
      .parse(parseJson(result.stdout));
    const paths = envelope.data.commands.map((command) =>
      command.path.join(" ")
    );
    for (const expected of ["add", "run", "logs", "daemon start"]) {
      expect(paths).toContain(expected);
    }
  });

  test("requires explicit confirmation for headless add", async () => {
    const home = temporaryHome();
    const result = await runCli(
      ["add", "tick", "--every", "5s", "--json", "--", "echo", "ok"],
      home
    );
    expect(result.exitCode).toBe(2);
    const error = actionErrorSchema.parse(parseJson(result.stderr));
    expect(error.error.code).toBe("action_required");
    expect(error.error.hint).toContain("--yes");
  });

  test("returns a dry-run plan without saving the schedule", async () => {
    const home = temporaryHome();
    const plan = await runCli(
      ["add", "tick", "--every", "5s", "--dry-run", "--json", "--", "echo"],
      home
    );
    expect(plan.exitCode).toBe(0);
    const envelope = z
      .object({
        data: z.object({
          mode: z.literal("plan"),
          schedule: z.object({ name: z.literal("tick") }),
        }),
      })
      .parse(parseJson(plan.stdout));
    expect(envelope.data.mode).toBe("plan");
    const list = await runCli(["list", "--json"], home);
    expect(parseJson(list.stdout)).toMatchObject({ data: [], ok: true });
  });

  test("runs a gated schedule through every gate outcome in both modes", async () => {
    const home = temporaryHome();

    const add = async (name: string, gate: string): Promise<void> => {
      const result = await runCli(
        ["add", name, "--yes", "--gate", gate, "--", "cat"],
        home
      );
      expect(result.exitCode).toBe(0);
    };
    await add("open", "echo gate-context");
    await add("quiet", "true");
    await add("broken", "exit 3");

    const open = await runCli(["run", "open", "--json"], home);
    expect(open.exitCode).toBe(0);
    const openRun = runEnvelopeSchema.parse(parseJson(open.stdout));
    expect(openRun.data.status).toBe("succeeded");
    expect(openRun.data.executor).toBe("cat");

    const quiet = await runCli(["run", "quiet", "--json"], home);
    const quietRun = runEnvelopeSchema.parse(parseJson(quiet.stdout));
    expect(quietRun.data.status).toBe("clean");
    expect(quietRun.data.executor).toBeNull();

    const broken = await runCli(["run", "broken", "--json"], home);
    expect(broken.exitCode).toBe(1);
    const brokenRun = runEnvelopeSchema.parse(parseJson(broken.stdout));
    expect(brokenRun.data.status).toBe("gate_failed");

    // In exit mode a silent exit 0 opens the gate all the same.
    const byExit = await runCli(
      [
        "add",
        "by-exit",
        "--yes",
        "--gate",
        "true",
        "--gate-mode",
        "exit",
        "--",
        "cat",
      ],
      home
    );
    expect(byExit.exitCode).toBe(0);
    const exitResult = await runCli(["run", "by-exit", "--json"], home);
    const exitRun = runEnvelopeSchema.parse(parseJson(exitResult.stdout));
    expect(exitRun.data.status).toBe("succeeded");
    expect(exitRun.data.executor).toBe("cat");

    // The gate's stdout must reach the action: cat echoes it into the log.
    const logs = await runCli(
      ["logs", "open", "--run", openRun.data.run_id, "--json"],
      home
    );
    const logEnvelope = logsEnvelopeSchema.parse(parseJson(logs.stdout));
    const content = logEnvelope.data.log?.content ?? "";
    expect(content).toContain("gate open");
    expect(content.split("gate-context").length).toBeGreaterThan(2);
  });

  test("keeps every run in day-partitioned logs and prunes on demand", async () => {
    const home = temporaryHome();
    await runCli(["add", "tick", "--yes", "--", "echo", "ok"], home);
    for (let index = 0; index < 3; index += 1) {
      // oxlint-disable-next-line no-await-in-loop -- sequential runs keep the record order deterministic.
      await runCli(["run", "tick"], home);
    }
    const logs = await runCli(["logs", "tick", "--json"], home);
    const envelope = logsEnvelopeSchema.parse(parseJson(logs.stdout));
    expect(envelope.data.total_runs).toBe(3);
    expect(envelope.data.runs).toHaveLength(3);
    const day = new Date().toISOString().slice(0, 10);
    expect(envelope.data.runs[0]?.log_pointer).toContain(
      path.join("logs", "tick", day)
    );

    const guarded = await runCli(
      ["prune", "--older-than", "1s", "--json"],
      home
    );
    expect(guarded.exitCode).toBe(2);

    await Bun.sleep(1500);
    const pruned = await runCli(
      ["prune", "--older-than", "1s", "--yes", "--json"],
      home
    );
    expect(parseJson(pruned.stdout)).toMatchObject({
      data: { removed_runs: 3 },
    });
    const after = await runCli(["logs", "tick", "--json"], home);
    expect(
      logsEnvelopeSchema.parse(parseJson(after.stdout)).data.total_runs
    ).toBe(0);
  }, 30_000);

  test("emits run records with a cursor for incremental consumers", async () => {
    const home = temporaryHome();
    const added = await runCli(
      ["add", "tick", "--every", "1h", "--yes", "--", "echo", "hello"],
      home
    );
    expect(added.exitCode).toBe(0);
    const first = await runCli(["run", "tick", "--json"], home);
    expect(first.exitCode).toBe(0);

    const exported = await runCli(["runs", "--json"], home);
    expect(exported.exitCode).toBe(0);
    const envelope = runsEnvelopeSchema.parse(parseJson(exported.stdout));
    expect(envelope.data.runs).toHaveLength(1);
    expect(envelope.data.runs[0]?.status).toBe("succeeded");
    expect(envelope.data.runs[0]?.cwd).not.toBeNull();

    const caughtUp = await runCli(
      ["runs", "--since", envelope.data.cursor, "--json"],
      home
    );
    const empty = runsEnvelopeSchema.parse(parseJson(caughtUp.stdout));
    expect(empty.data.runs).toHaveLength(0);
    expect(empty.data.cursor).toBe(envelope.data.cursor);

    await runCli(["run", "tick"], home);
    const next = await runCli(
      ["runs", "--since", envelope.data.cursor, "--json"],
      home
    );
    expect(
      runsEnvelopeSchema.parse(parseJson(next.stdout)).data.runs
    ).toHaveLength(1);

    const invalid = await runCli(
      ["runs", "--since", "yesterday", "--json"],
      home
    );
    expect(invalid.exitCode).toBe(2);
  });

  test("pause, resume, and rm manage the schedule lifecycle", async () => {
    const home = temporaryHome();
    await runCli(
      ["add", "tick", "--every", "5s", "--yes", "--", "echo", "ok"],
      home
    );
    const paused = await runCli(["pause", "tick", "--json"], home);
    expect(parseJson(paused.stdout)).toMatchObject({
      data: [{ active: false }],
    });
    const resumed = await runCli(["resume", "tick", "--json"], home);
    expect(parseJson(resumed.stdout)).toMatchObject({
      data: [{ active: true }],
    });
    const removed = await runCli(["rm", "tick", "--yes", "--json"], home);
    expect(parseJson(removed.stdout)).toMatchObject({
      data: { removed: "tick" },
    });
    const list = await runCli(["list", "--json"], home);
    expect(parseJson(list.stdout)).toMatchObject({ data: [] });
  });

  test("registers a one-shot job headlessly and keeps schedules clean", async () => {
    const home = temporaryHome();
    const registered = await runCli(
      ["once", "--name", "import", "--json", "--", "echo", "ok"],
      home
    );
    expect(registered.exitCode).toBe(0);
    const envelope = z
      .object({
        data: z.object({
          command: z.array(z.string()),
          cwd: z.string(),
          job: z.string(),
          job_id: z.string(),
        }),
        hint: z.string(),
        ok: z.literal(true),
      })
      .parse(parseJson(registered.stdout));
    expect(envelope.data.job).toContain("once-import-");
    expect(envelope.data.command).toEqual(["echo", "ok"]);
    // Registering with the daemon down keeps the job; it says so.
    expect(envelope.hint).toContain("daemon is not running");

    const list = await runCli(["list", "--json"], home);
    expect(parseJson(list.stdout)).toMatchObject({ data: [] });

    const refusals: readonly (readonly string[])[] = [
      ["rm", envelope.data.job, "--yes"],
      ["pause", envelope.data.job],
      ["set", envelope.data.job, "--every", "5m"],
    ];
    for (const argv of refusals) {
      // oxlint-disable-next-line no-await-in-loop -- sequential CLI runs against one database.
      const refused = await runCli([...argv, "--json"], home);
      expect(refused.exitCode).toBe(1);
      expect(
        actionErrorSchema.parse(parseJson(refused.stderr)).error.code
      ).toBe("schedule_not_found");
    }
  });

  test("set changes the trigger in place and reschedules", async () => {
    const home = temporaryHome();
    await runCli(
      ["add", "tick", "--every", "5s", "--yes", "--", "echo", "ok"],
      home
    );
    const updated = await runCli(
      ["set", "tick", "--every", "1h", "--json"],
      home
    );
    expect(updated.exitCode).toBe(0);
    const record = z
      .object({
        data: z.object({
          next_fire_at: z.string(),
          trigger: z.object({
            kind: z.literal("every"),
            seconds: z.literal(3600),
          }),
        }),
      })
      .parse(parseJson(updated.stdout));
    const millisUntilFire = Date.parse(record.data.next_fire_at) - Date.now();
    expect(millisUntilFire).toBeGreaterThan(3_000_000);
    const empty = await runCli(["set", "tick", "--json"], home);
    expect(empty.exitCode).toBe(2);
  });

  test("set reaches a schedule by id and can clear fields and replace the command", async () => {
    const home = temporaryHome();
    const added = await runCli(
      [
        "add",
        "edit-me",
        "--gate",
        "echo go",
        "--timeout",
        "5m",
        "--yes",
        "--json",
        "--",
        "echo",
        "old",
      ],
      home
    );
    const { id } = z
      .object({ data: z.object({ schedule: z.object({ id: z.string() }) }) })
      .parse(parseJson(added.stdout)).data.schedule;
    const updated = await runCli(
      [
        "set",
        id,
        "--no-gate",
        "--no-timeout",
        "--json",
        "--",
        "printf",
        "%s",
        "new",
      ],
      home
    );
    expect(updated.exitCode).toBe(0);
    expect(parseJson(updated.stdout)).toMatchObject({
      data: {
        command: ["printf", "%s", "new"],
        gate: null,
        id,
        name: "edit-me",
        timeout_seconds: null,
      },
    });
  });

  test("a group pauses and resumes as one unit", async () => {
    const home = temporaryHome();
    for (const name of ["loop-a", "loop-b"]) {
      // oxlint-disable-next-line no-await-in-loop -- concurrent adds against a fresh database race its creation.
      const added = await runCli(
        ["add", name, "--group", "loops", "--yes", "--", "echo", "ok"],
        home
      );
      expect(added.exitCode).toBe(0);
    }
    const paused = await runCli(["pause", "--group", "loops", "--json"], home);
    const envelope = z
      .object({
        data: z.array(
          z.object({ active: z.boolean(), group: z.string().nullable() })
        ),
      })
      .parse(parseJson(paused.stdout));
    expect(envelope.data).toHaveLength(2);
    expect(envelope.data.every((record) => !record.active)).toBe(true);
    expect(envelope.data.every((record) => record.group === "loops")).toBe(
      true
    );
    const resumed = await runCli(
      ["resume", "--group", "loops", "--json"],
      home
    );
    expect(
      z
        .object({ data: z.array(z.object({ active: z.boolean() })) })
        .parse(parseJson(resumed.stdout))
        .data.every((record) => record.active)
    ).toBe(true);
  });
});

describe("daemon service", () => {
  test("install --dry-run renders the service file and writes nothing", async () => {
    const home = temporaryHome();
    const fakeHome = temporaryHome();
    const result = await runCli(
      [
        "daemon",
        "install",
        "--dry-run",
        "--path",
        "/opt/tools/bin:/usr/bin",
        "--json",
      ],
      home,
      { HOME: fakeHome }
    );
    expect(result.exitCode).toBe(0);
    const plan = z
      .object({
        data: z.object({
          content: z.string(),
          environment: z.record(z.string(), z.string()),
          mode: z.literal("plan"),
          path: z.string(),
          program: z.array(z.string()),
        }),
      })
      .parse(parseJson(result.stdout)).data;
    expect(plan.environment).toMatchObject({
      PATH: "/opt/tools/bin:/usr/bin",
      ULTRADIAN_HOME: home,
    });
    expect(plan.path.startsWith(fakeHome)).toBe(true);
    expect(plan.program.slice(-2)).toEqual(["daemon", "run"]);
    expect(plan.content).toContain("/opt/tools/bin:/usr/bin");
    expect(await Bun.file(plan.path).exists()).toBe(false);
  });

  test("self install refuses to copy anything but a compiled binary", async () => {
    const home = temporaryHome();
    const result = await runCli(["self", "install", "--json"], home);
    expect(result.exitCode).toBe(2);
    expect(parseJson(result.stderr)).toMatchObject({
      error: { code: "not_a_compiled_binary" },
    });
  });
});

describe("end-to-end through the daemon", () => {
  test("starts, fires a scheduled run, reports status, and stops", async () => {
    const home = temporaryHome();
    await runCli(
      ["add", "pulse", "--every", "1s", "--yes", "--", "echo", "beat"],
      home
    );
    const started = await runCli(["daemon", "start", "--json"], home);
    expect(started.exitCode).toBe(0);
    try {
      const deadline = Date.now() + 15_000;
      let scheduledRun: string | undefined;
      while (scheduledRun === undefined && Date.now() < deadline) {
        // oxlint-disable-next-line no-await-in-loop -- polling the CLI until the daemon fires.
        const logs = await runCli(["logs", "pulse", "--json"], home);
        const envelope = logsEnvelopeSchema.parse(parseJson(logs.stdout));
        scheduledRun = envelope.data.runs.find(
          (run) => run.trigger === "scheduled" && run.status === "succeeded"
        )?.run_id;
        if (scheduledRun === undefined) {
          // oxlint-disable-next-line no-await-in-loop -- polling the CLI until the daemon fires.
          await Bun.sleep(250);
        }
      }
      expect(scheduledRun).toBeDefined();

      const status = await runCli(["status", "--json"], home);
      expect(parseJson(status.stdout)).toMatchObject({
        data: { daemon: { live: true, version: packageVersion } },
      });
    } finally {
      const stopped = await runCli(["daemon", "stop", "--json"], home);
      expect(stopped.exitCode).toBe(0);
    }
    const after = await runCli(["status", "--json"], home);
    expect(parseJson(after.stdout)).toMatchObject({
      data: { daemon: { live: false } },
    });
  }, 30_000);

  test("picks up one-shot jobs and queued fires and records them as ordinary runs", async () => {
    const home = temporaryHome();
    const jobEnvelopeSchema = z.object({
      data: z.object({ job: z.string(), job_id: z.string() }),
    });
    const register = async (
      argv: readonly string[]
    ): Promise<{ job: string; job_id: string }> => {
      const result = await runCli(["once", "--json", ...argv], home);
      expect(result.exitCode).toBe(0);
      return jobEnvelopeSchema.parse(parseJson(result.stdout)).data;
    };
    const settled = async (
      jobId: string
    ): Promise<{
      log_pointer: string | null;
      status: string;
      trigger: string;
    }> => {
      const deadline = Date.now() + 20_000;
      while (Date.now() < deadline) {
        // oxlint-disable-next-line no-await-in-loop -- polling the CLI until the daemon finishes the job.
        const exported = await runCli(["runs", "--json"], home);
        const record = runsEnvelopeSchema
          .parse(parseJson(exported.stdout))
          .data.runs.find(
            (run) => run.schedule_id === jobId && run.status !== "running"
          );
        if (record !== undefined) {
          expect(record.cwd).toBe(root);
          return record;
        }
        // oxlint-disable-next-line no-await-in-loop -- polling the CLI until the daemon finishes the job.
        await Bun.sleep(250);
      }
      throw new Error(`No finished run for job ${jobId}.`);
    };

    const added = await runCli(
      ["add", "manual", "--yes", "--", "sleep", "2"],
      home
    );
    expect(added.exitCode).toBe(0);
    const detached = await runCli(
      ["run", "manual", "--detach", "--json"],
      home
    );
    expect(detached.exitCode).toBe(0);
    const queued = z
      .object({
        data: z.object({ run_id: z.string(), status: z.literal("queued") }),
        hint: z.string(),
      })
      .parse(parseJson(detached.stdout));
    expect(queued.hint).toContain("daemon is not running");
    const overlapping = await runCli(["run", "manual", "--json"], home);
    expect(overlapping.exitCode).toBe(75);
    expect(
      z
        .object({ error: z.object({ code: z.string() }) })
        .parse(parseJson(overlapping.stderr)).error.code
    ).toBe("run_in_flight");

    const started = await runCli(["daemon", "start", "--json"], home);
    expect(started.exitCode).toBe(0);
    try {
      const quick = await register(["--", "echo", "hello"]);
      const slow = await register(["--timeout", "1s", "--", "sleep", "30"]);

      const quickRun = await settled(quick.job_id);
      expect(quickRun.status).toBe("succeeded");
      expect(quickRun.trigger).toBe("once");
      const day = new Date().toISOString().slice(0, 10);
      expect(quickRun.log_pointer).toContain(path.join("logs", quick.job, day));
      expect(await Bun.file(quickRun.log_pointer ?? "").text()).toContain(
        "hello"
      );

      const slowRun = await settled(slow.job_id);
      expect(slowRun.status).toBe("timed_out");

      const deadline = Date.now() + 15_000;
      let manualStatus = "queued";
      while (manualStatus !== "succeeded" && Date.now() < deadline) {
        // oxlint-disable-next-line no-await-in-loop -- polling the CLI until the daemon finishes the queued fire.
        const logs = await runCli(["logs", "manual", "--json"], home);
        manualStatus =
          logsEnvelopeSchema
            .parse(parseJson(logs.stdout))
            .data.runs.find((run) => run.run_id === queued.data.run_id)
            ?.status ?? "missing";
        // oxlint-disable-next-line no-await-in-loop -- polling the CLI until the daemon finishes the queued fire.
        await Bun.sleep(250);
      }
      expect(manualStatus).toBe("succeeded");

      // Spent jobs leave the schedules table exactly as they found it.
      const list = await runCli(["list", "--json"], home);
      expect(
        z
          .object({
            data: z.array(
              z.object({ schedule: z.object({ name: z.string() }) })
            ),
          })
          .parse(parseJson(list.stdout))
          .data.map((entry) => entry.schedule.name)
      ).toEqual(["manual"]);
    } finally {
      await runCli(["daemon", "stop", "--json"], home);
    }
  }, 45_000);
});

// The records other tools build on. Changing a key here is a
// breaking change: bump SCHEMA_VERSION and say so in the release notes.
const pinnedScheduleKeys = [
  "active",
  "catch_up_seconds",
  "command",
  "created_at",
  "cwd",
  "gate",
  "gate_mode",
  "group",
  "id",
  "name",
  "next_fire_at",
  "timeout_seconds",
  "trigger",
  "updated_at",
];
const pinnedRunKeys = [
  "action_exit",
  "cwd",
  "executor",
  "finished_at",
  "gate_exit",
  "log_pointer",
  "machine_id",
  "pgid",
  "run_id",
  "schedule",
  "schedule_id",
  "started_at",
  "status",
  "trigger",
];

const jsonOf = async (
  args: readonly string[],
  home: string
): Promise<unknown> => {
  const result = await runCli(args, home);
  return parseJson(result.stdout);
};

const keysOf = (value: unknown): string[] =>
  Object.keys(z.record(z.string(), z.unknown()).parse(value)).toSorted();

describe("public JSON contract", () => {
  test("schedule and run records keep their pinned shape in output and schema", async () => {
    const home = temporaryHome();
    const added = await runCli(
      ["add", "pinned", "--cron", "0 * * * *", "--yes", "--", "echo", "ok"],
      home
    );
    expect(added.exitCode).toBe(0);
    await runCli(
      ["add", "interval", "--every", "15m", "--yes", "--", "true"],
      home
    );
    await runCli(["add", "byhand", "--yes", "--", "true"], home);
    await runCli(
      [
        "add",
        "zoned",
        "--cron",
        "0 9 * * *",
        "--tz",
        "Europe/Paris",
        "--yes",
        "--",
        "true",
      ],
      home
    );
    await runCli(["run", "pinned"], home);

    const list = z
      .object({
        data: z.array(z.object({ schedule: z.unknown() })),
        schemaVersion: z.literal(2),
      })
      .parse(await jsonOf(["list", "--json"], home));
    const records = list.data.map((entry) => entry.schedule);
    for (const record of records) {
      expect(keysOf(record)).toEqual(pinnedScheduleKeys);
    }
    const triggers = z
      .array(z.object({ name: z.string(), trigger: z.unknown() }))
      .parse(records);
    expect(
      Object.fromEntries(triggers.map((r) => [r.name, r.trigger]))
    ).toEqual({
      byhand: { kind: "manual" },
      interval: { kind: "every", seconds: 900 },
      pinned: { expression: "0 * * * *", kind: "cron", timezone: null },
      zoned: {
        expression: "0 9 * * *",
        kind: "cron",
        timezone: "Europe/Paris",
      },
    });

    const runs = z
      .object({ data: z.object({ runs: z.array(z.unknown()) }) })
      .parse(await jsonOf(["runs", "--json"], home));
    expect(runs.data.runs).toHaveLength(1);
    expect(keysOf(runs.data.runs[0])).toEqual(pinnedRunKeys);

    const propertiesSchema = z.object({
      properties: z.record(z.string(), z.unknown()),
    });
    const catalog = z
      .object({
        data: z.object({
          commands: z.array(
            z.object({ outputSchema: z.unknown(), path: z.array(z.string()) })
          ),
        }),
      })
      .parse(await jsonOf(["schema", "--json"], home));
    const outputOf = (name: string): unknown =>
      catalog.data.commands.find((command) => command.path.join(" ") === name)
        ?.outputSchema;
    const runsSchema = z
      .object({
        properties: z.object({ runs: z.object({ items: propertiesSchema }) }),
      })
      .parse(outputOf("runs"));
    expect(
      Object.keys(runsSchema.properties.runs.items.properties).toSorted()
    ).toEqual(pinnedRunKeys);
    const listSchema = z
      .object({
        items: z.object({
          properties: z.object({ schedule: propertiesSchema }),
        }),
      })
      .parse(outputOf("list"));
    expect(
      Object.keys(listSchema.items.properties.schedule.properties).toSorted()
    ).toEqual(pinnedScheduleKeys);
  });
});
