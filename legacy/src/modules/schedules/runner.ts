import { once } from "node:events";
import { closeSync, mkdirSync, openSync, writeSync } from "node:fs";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";

import type { Run, Schedule, ScheduleStore } from "./store.ts";

// How long a process group gets between SIGTERM and SIGKILL.
export const KILL_GRACE_MS = 10_000;

const stamp = (): string => new Date().toISOString();

const signalGroup = (pgid: number, signal: NodeJS.Signals | 0): boolean => {
  try {
    process.kill(-pgid, signal);
    return true;
  } catch (error) {
    return error instanceof Error && "code" in error && error.code === "EPERM";
  }
};

export const groupIsAlive = (pgid: number): boolean => signalGroup(pgid, 0);

// SIGTERM the whole process group, then SIGKILL whatever is still alive
// after the grace. Every gate and action leads its own group, so this
// reaches the grandchildren a shell script leaves behind, which is what
// lets their output pipes close.
export const terminateGroup = async (
  pgid: number,
  graceMs = KILL_GRACE_MS
): Promise<void> => {
  if (!signalGroup(pgid, "SIGTERM")) {
    return;
  }
  const deadline = Date.now() + graceMs;
  while (Date.now() < deadline) {
    if (!groupIsAlive(pgid)) {
      return;
    }
    // oxlint-disable-next-line no-await-in-loop -- polling for the group to exit.
    await delay(100);
  }
  signalGroup(pgid, "SIGKILL");
};

// How often the owner of a fire checks the store for a cancellation.
const CANCEL_POLL_MS = 1000;

export type StopReason = "timeout" | "canceled" | "shutdown";

const reasonOf = (signal: AbortSignal): StopReason => {
  const reason: unknown = signal.reason;
  if (reason === "timeout" || reason === "canceled") {
    return reason;
  }
  return "shutdown";
};

// A run log keeps at most this much captured output. Past it the output is
// still drained, so the process never blocks, but it is dropped.
export const RUN_LOG_CAP_BYTES = 10_000_000;

class RunLog {
  readonly #fd: number;
  readonly #encoder = new TextEncoder();
  #captured = 0;
  #truncated = false;

  constructor(file: string) {
    this.#fd = openSync(file, "a", 0o600);
  }

  write(chunk: Uint8Array): void {
    if (this.#truncated) {
      return;
    }
    const room = RUN_LOG_CAP_BYTES - this.#captured;
    if (chunk.byteLength <= room) {
      writeSync(this.#fd, chunk);
      this.#captured += chunk.byteLength;
      return;
    }
    writeSync(this.#fd, chunk.subarray(0, room));
    this.#captured = RUN_LOG_CAP_BYTES;
    this.#truncated = true;
    this.line(
      `\n# output truncated at ${RUN_LOG_CAP_BYTES} bytes; the rest was discarded`
    );
  }

  // Marker lines always land, past the cap too, so every log still says
  // how its run ended.
  line(text: string): void {
    writeSync(this.#fd, this.#encoder.encode(`${text}\n`));
  }

  close(): void {
    closeSync(this.#fd);
  }
}

const pump = async (
  stream: ReadableStream<Uint8Array>,
  log: RunLog,
  collect: boolean
): Promise<string> => {
  const decoder = new TextDecoder();
  let text = "";
  for await (const chunk of stream) {
    log.write(chunk);
    if (collect) {
      text += decoder.decode(chunk, { stream: true });
    }
  }
  if (collect) {
    text += decoder.decode();
  }
  return text;
};

interface PhaseResult {
  exit: number | null;
  stdout: string;
  stopped: StopReason | null;
}

// Resolves with the stop reason once the fire is stopped, or with null when
// the wait itself is called off because the phase finished first.
const waitForStop = async (
  stop: AbortSignal,
  calledOff: AbortSignal
): Promise<StopReason | null> => {
  if (stop.aborted) {
    return reasonOf(stop);
  }
  try {
    await once(stop, "abort", { signal: calledOff });
    return reasonOf(stop);
  } catch {
    return null;
  }
};

// Runs one process as the leader of a new process group and waits for the
// whole group: when the leader exits, anything it left behind is terminated
// so the run never hangs on a grandchild holding its pipes open, and when
// the fire is stopped the group is terminated mid-flight.
const runPhase = async (options: {
  argv: readonly string[];
  cwd: string;
  environment: Record<string, string | undefined>;
  stdin: "ignore" | Uint8Array;
  log: RunLog;
  stop: AbortSignal;
  onSpawn: (pgid: number) => void;
}): Promise<PhaseResult> => {
  const { argv, cwd, environment, log, onSpawn, stdin, stop } = options;
  const child = Bun.spawn([...argv], {
    cwd,
    detached: true,
    env: environment,
    stderr: "pipe",
    stdin,
    stdout: "pipe",
  });
  onSpawn(child.pid);
  const output = Promise.all([
    pump(child.stdout, log, true),
    pump(child.stderr, log, false),
  ]);
  const settled = new AbortController();
  const exited = async (): Promise<null> => {
    await child.exited;
    return null;
  };
  const first = await Promise.race([
    exited(),
    waitForStop(stop, settled.signal),
  ]);
  settled.abort();
  if (first === null) {
    if (groupIsAlive(child.pid)) {
      log.line(`# terminating processes left behind ${stamp()}`);
      await terminateGroup(child.pid);
    }
  } else {
    await terminateGroup(child.pid);
  }
  const exit = await child.exited;
  const [stdout] = await output;
  return { exit, stdout, stopped: first };
};

// Runs one fire of a schedule, from either the daemon or a manual trigger.
// Gate contract in 'output' mode: exit 0 with empty stdout closes the gate
// (clean pass); exit 0 with stdout opens it. In 'exit' mode exit 0 alone
// opens it. Either way the gate's stdout becomes the action's stdin, and a
// nonzero exit is a gate failure and the action never runs. The schedule's
// timeout bounds the whole fire, gate and action together.
export const executeFire = async (options: {
  store: ScheduleStore;
  schedule: Schedule;
  run: Run;
  signal?: AbortSignal;
}): Promise<Run> => {
  const { run, schedule, signal, store } = options;
  const { trigger } = run;
  const runId = run.id;
  const logPointer =
    run.logPointer ??
    path.join(store.home, "logs", schedule.name, `${runId}.log`);
  mkdirSync(path.dirname(logPointer), { mode: 0o700, recursive: true });
  const log = new RunLog(logPointer);
  const settle = (result: Parameters<ScheduleStore["finishRun"]>[1]): Run => {
    store.finishRun(runId, result);
    return store.getRun(runId) ?? run;
  };

  log.line(`# ${schedule.name} ${runId}`);
  log.line(`# started ${stamp()} trigger=${trigger}`);

  const stop = new AbortController();
  const timer =
    schedule.timeoutMs === null
      ? undefined
      : setTimeout(() => {
          stop.abort("timeout" satisfies StopReason);
        }, schedule.timeoutMs);
  // The store is the only channel in: 'cancel' finishes the row, and the
  // owner of the fire, daemon or foreground CLI, notices here.
  const cancelWatch = setInterval(() => {
    if (store.getRun(runId)?.status !== "running") {
      stop.abort("canceled" satisfies StopReason);
    }
  }, CANCEL_POLL_MS);
  const onShutdown = (): void => {
    stop.abort("shutdown" satisfies StopReason);
  };
  if (signal?.aborted === true) {
    onShutdown();
  } else {
    signal?.addEventListener("abort", onShutdown, { once: true });
  }
  const stoppedStatus = (reason: StopReason): Run["status"] => {
    if (reason === "timeout") {
      log.line(`# timed out after ${schedule.timeoutMs}ms ${stamp()}`);
      return "timed_out";
    }
    if (reason === "canceled") {
      log.line(`# canceled ${stamp()}`);
      return "canceled";
    }
    log.line(`# interrupted ${stamp()}`);
    return "interrupted";
  };

  const environment = {
    ...process.env,
    ULTRADIAN_RUN_ID: runId,
    ULTRADIAN_SCHEDULE: schedule.name,
  };
  const onSpawn = (pgid: number): void => {
    store.setRunProcessGroup(runId, pgid);
  };

  let gateExit: number | null = null;
  let context = "";
  try {
    if (schedule.gate !== null) {
      log.line(`# gate: ${schedule.gate}`);
      const gate = await runPhase({
        argv: ["/bin/sh", "-c", schedule.gate],
        cwd: schedule.workingDirectory,
        environment,
        log,
        onSpawn,
        stdin: "ignore",
        stop: stop.signal,
      });
      gateExit = gate.stopped === null ? gate.exit : null;
      if (gate.stopped !== null) {
        return settle({ gateExit, status: stoppedStatus(gate.stopped) });
      }
      if (gate.exit !== 0) {
        log.line(`# gate failed exit=${gate.exit} ${stamp()}`);
        return settle({ gateExit, status: "gate_failed" });
      }
      if (schedule.gateMode === "output" && gate.stdout.trim() === "") {
        log.line(`# gate clean ${stamp()}`);
        return settle({ gateExit, status: "clean" });
      }
      context = gate.stdout;
      log.line(`# gate open (${context.length} bytes of context) ${stamp()}`);
    }
    if (stop.signal.aborted) {
      return settle({
        gateExit,
        status: stoppedStatus(reasonOf(stop.signal)),
      });
    }

    const executor = path.basename(schedule.command[0] ?? "command");
    store.setRunExecutor(runId, executor);
    log.line(`# action: ${schedule.command.join(" ")}`);
    const action = await runPhase({
      argv: schedule.command,
      cwd: schedule.workingDirectory,
      environment: { ...environment, ULTRADIAN_SESSION_ID: runId },
      log,
      onSpawn,
      stdin:
        schedule.gate === null ? "ignore" : new TextEncoder().encode(context),
      stop: stop.signal,
    });
    if (action.stopped !== null) {
      return settle({ gateExit, status: stoppedStatus(action.stopped) });
    }
    log.line(`# finished exit=${action.exit} ${stamp()}`);
    return settle({
      actionExit: action.exit,
      gateExit,
      status: action.exit === 0 ? "succeeded" : "failed",
    });
  } catch (error) {
    log.line(
      `# error ${error instanceof Error ? error.message : String(error)}`
    );
    return settle({ gateExit, status: "failed" });
  } finally {
    clearTimeout(timer);
    clearInterval(cancelWatch);
    signal?.removeEventListener("abort", onShutdown);
    log.close();
  }
};
