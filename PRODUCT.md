# Product

## Purpose

Ultradian runs gated schedules and workflows for invoking AI. A schedule pairs a trigger, an optional gate command, and an action command. The gate runs first on every fire and decides whether the action runs at all, so a loop that mostly has nothing to do costs a script run instead of an agent session.

Success means an engineer can put a recurring loop on a machine in one command, leave it running under a local daemon, and later answer two questions from the record: is the loop healthy, and what did it actually do.

## Users

The primary user owns the machine the loops run on. They write the gate scripts, choose the action commands (often `claude -p`, `codex exec`, or a custom harness), and read the history when a loop misbehaves.

The second user is the coding agent driving the CLI on the owner's behalf. It discovers the surface through `schema --json` and `describe`, then works in `--json`, branching on exit codes and stable error codes. Every prompt has a flag route, so an agent never needs a TTY.

## The gated loop

A gate speaks through its exit code and stdout:

- exit 0 with empty stdout: clean pass, the action never runs;
- exit 0 with stdout: the gate opens, and that stdout is piped to the action's stdin as context; and
- nonzero exit: gate failure, recorded, the action never runs.

A run's status carries the outcome: `queued` (waiting for the daemon), `running`, `clean` (the gate closed), `succeeded` or `failed` (the action ran), `gate_failed`, `timed_out` (the action outlived the schedule's `--timeout` and its process group was killed), `canceled` (stopped with `cancel`), `skipped` (the previous run was still in flight), `missed` (a fire passed while the daemon was down and outside the schedule's `--catch-up` window), and `interrupted` (the owning process died mid-run).

## In this version

- **Schedules.** Cron expressions in any time zone, fixed intervals, and manual triggers, each with a working directory, an optional timeout and a catch-up window, stored in a local SQLite database under `~/.ultradian`.
- **Daemon.** A single background process claims due schedules, fires them concurrently, writes a heartbeat, and recovers cleanly after a crash or restart. It can run under launchd or systemd with the login shell's `PATH`.
- **Gates.** First-class, with the contract above enforced by the runner, or opening on exit 0 alone with `--gate-mode exit`.
- **Run control.** Timeouts and `cancel` end a run's whole process group; `run --detach` queues a fire for the daemon.
- **Records.** Every run captures status, gate and action exit codes, timing, executor, machine, and a size-capped log holding the captured output. `runs --since` pages through every change for other tools. Old runs are pruned after 30 days unless configured otherwise.
- **CLI.** `add`, `set`, `once`, `list`, `run`, `cancel`, `status`, `logs`, `runs`, `pause`, `resume`, `rm`, `prune`, `daemon start|stop|restart|install|uninstall|run` and `self install`, each speaking human text, `--json`, and `--jsonl`.

## Deliberately out of scope for now

- remote machines and any form of fleet control;
- automatic placement or load balancing of schedules;
- credential storage and rotation for the commands being run;
- workflow graphs, fan-out, and dependencies between schedules; and
- a graphical or full-screen interface.

The run record is the integration surface in the meantime: anything that wants to build on Ultradian consumes records through `--json` and `--jsonl`.

## Personality

Exact, calm, and helpful. Concise during successful work, specific when something fails. Color supports hierarchy without turning routine output into decoration.

## Anti-patterns

- prompts in CI, pipes, JSON, or agent runs;
- prose-only failures that force callers to parse English;
- a daemon that silently loses runs across a restart;
- decorative banners or animation that delay the task; and
- credentials exposed in logs, errors, or generated commands.
