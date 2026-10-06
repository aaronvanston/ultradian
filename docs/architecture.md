# Architecture

Ultradian is one Rust binary with no runtime dependencies: SQLite is compiled in, and zone data is embedded.

## Modules

```text
src/main.rs           argv -> cli::run -> exit code
src/cli/mod.rs        builds the command tree from the catalog, dispatches, and is the
                      only writer to stdout and stderr
src/cli/commander.rs  the argv parser: options anywhere before --, --opt=value, --no-*,
                      variadic commands, Commander's error wording
src/cli/options.rs    reading parsed option values and their invalid_options errors
src/cli/schedules.rs  add, once, list, set, pause, resume, rm, run, runs, cancel,
                      status, logs, prune, and their human renderings
src/cli/daemon.rs     daemon start|stop|restart|install|uninstall|run, self install
src/cli/doctor.rs     doctor and completion
src/cli/system.rs     schema and describe;  src/cli/version.rs  version
src/catalog.rs        the command catalog (catalog.json): help, schema, describe,
                      completions and the parser tree all read it
src/output.rs         envelopes, ISO times, JSON and JSONL;  src/errors.rs  AppError, exit codes
src/style.rs          semantic color and symbols, with ASCII fallbacks
src/store/            the SQLite store: queries (mod.rs), ordered migrations (schema.rs),
                      the 0.1 upgrade (legacy.rs)
src/triggers.rs       triggers, durations, zone names;  src/cron.rs  cron next fires
src/runner.rs         one fire: gate, action, process groups, timeout, cancel, log
src/daemon/           the tick loop (run_loop.rs), start/stop and the supervisors
                      (control.rs), the plist, unit and service PATH (service.rs)
src/ids.rs            run, schedule and job ids
```

## Execution lifecycle

1. Preflight scans argv for the presentation flags, so even a parse error is reported in the mode and color asked for.
2. The parser reads argv against the tree built from the catalog. Every parse error exits 2; with `--json` it also prints an `invalid_usage` envelope.
3. The command runs with a context holding the parsed arguments, options, working directory and styling. It returns a `Done` (the data, plus text for people) or an `AppError` (code, message, hint, exit code).
4. `cli::run` renders exactly one outcome: the human text, or one envelope for `--json` and `--jsonl`. Errors go to stderr.
5. `main` exits with the code. A closed stdout pipe ends the program quietly with exit 0.

## Human and headless behavior

Interactive execution requires human output, a terminal on stdin and stderr, no CI marker, and no explicit non-interactive flag. Every prompt is a small y/N question with a complete flag-based route. A write that needs confirmation fails with `action_required` and a copy-pasteable retry hint when interaction is unavailable.

Handlers return data. JSON and JSONL serialize the same values the human renderers format. The theme exposes semantic roles instead of raw colors, and status wording and symbols remain when color is disabled.

## The schedules module

### The store is the bus

`src/store/` owns a SQLite database at `~/.ultradian/ultradian.db`, relocatable with `ULTRADIAN_HOME`. It holds `schedules`, `runs`, a single-row `daemon` table, and a `counters` table that gives every change to a run the next revision number, which is what `runs --since` pages by. Its schema version lives in SQLite's `user_version` and moves forward through ordered migrations; a database newer than the binary is refused with `database_too_new`, and one written by 0.1.x, before versions, is carried into the current shape in place with its schedules, run history and daemon lock. WAL journaling and a busy timeout let the CLI and the daemon hold it at the same time. The folder is `0700` and the database and logs `0600`.

The store is the only channel between them. There is no socket, no RPC, and no running server to talk to. `add` inserts a row and the daemon picks it up on its next tick; `list`, `logs`, and `status` read rows and work exactly the same with the daemon stopped. A stopped daemon costs future fires, and it costs nothing else.

### Triggers

`src/triggers.rs` parses the three trigger kinds and computes the next fire. `src/cron.rs` is a port of [croner](https://github.com/hexagon/croner) 10.0.1, which 0.2.x used, so cron expressions parse and fire the same way, quirks included; zones come from embedded tz data; intervals parse durations such as `30s`, `15m`, `2h`, and `1d`; manual schedules have no next fire and only move through `run`.

### One-shot jobs

`once` registers a job and returns immediately. The job is a row in the same `schedules` table, marked `kind = 'once'`, with a manual trigger and a next fire of right now. That is the whole mechanism: the daemon's existing claim query picks it up on the next tick, and claiming a manual trigger advances it to no next fire, so it runs exactly once. A job registered with the daemon stopped keeps its past due time, because recovery only skips missed fires forward for real schedules, so it runs when the daemon returns.

Every schedule-facing read filters on `kind = 'schedule'`, so a one-shot never appears in `list` and `rm`, `set`, `pause`, and `resume` cannot reach it. It has no gate by design. The daemon deletes the row once the fire finishes: the job is consumed by its run, and the run record is what survives. Recovery sweeps the rows a crashed daemon fired but never removed.

The run it produces is an ordinary run: the same statuses, the same day-partitioned log, the same shape in `runs`, with `trigger` reading `once` and `schedule_id` carrying the job id that `once` printed.

### The runner and gate semantics

`src/runner.rs` executes one fire, from the daemon or from `run`, through the same path. It creates the run row first, so a fire is visible as `running` while it happens, then:

1. The gate, when the schedule has one, runs under `/bin/sh -c` with stdin ignored. Its stdout is captured and also written to the run log.
2. A nonzero gate exit finishes the run as `gate_failed`, and the action never runs.
3. A zero exit with empty stdout finishes the run as `clean`, and the action never runs, unless the schedule's gate mode is `exit`, where exit 0 alone opens the gate.
4. A zero exit with stdout opens the gate. The runner records the executor (the basename of the action's first argument) and spawns the action with that stdout as its stdin.
5. The action's exit code decides `succeeded` or `failed`.

Both processes inherit the environment plus `ULTRADIAN_RUN_ID` and `ULTRADIAN_SCHEDULE`; the action also receives `ULTRADIAN_SESSION_ID`. The schedule's recorded working directory is the cwd for both.

Each process starts in its own session and process group, whose id is stored on the run. A timeout, a `cancel` or a daemon shutdown sends the whole group `SIGTERM`, then `SIGKILL` ten seconds later, so children an action started in the background end with it. A run's log keeps its first 10MB of output and then a truncation marker; the finish marker is always written.

### Daemon lifecycle

`src/daemon/run_loop.rs` runs a one-second tick loop. Each tick writes a heartbeat row, starts runs queued by `run --detach`, claims every schedule whose `next_fire_at` has passed (advancing it in the same transaction so a claim happens once), and fires the claimed schedules concurrently. A tick that throws is logged and the loop carries on. Once a day it prunes runs and logs older than `ULTRADIAN_RETENTION` (default 30 days). A schedule whose previous run is still in flight records a `skipped` run, so the gap stays visible in its history.

Liveness is the heartbeat row plus a `kill(pid, 0)` check: a daemon counts as live when its pid exists and its heartbeat is under fifteen seconds old. That single row enforces one instance. `daemon start` re-invokes this CLI as `daemon run`, detaches it, and polls until the child's own heartbeat appears before reporting success. `daemon stop` sends `SIGTERM` and waits for the row to clear. On shutdown every run still in flight has its process group terminated and is recorded as `interrupted`, so a stopped daemon never leaves work running. `daemon install` writes a launchd agent or systemd user unit that runs the binary at its current path with the login shell's `PATH`; `daemon restart` goes through that supervisor when one is installed, which is how `self install` followed by a restart upgrades in place.

Startup recovery handles whatever the last process left behind. Runs still marked `running` whose owning pid is gone become `interrupted`, and any process group they left is terminated. An active schedule whose next fire is in the past fires once if that fire is no later than its `--catch-up` window; otherwise it records a single `missed` run and skips forward to the next fire from now. Either way a machine that was asleep for a day never wakes to a burst.

### Records and logs

Each run writes one log file at `~/.ultradian/logs/<schedule>/<YYYY-MM-DD>/<run_id>.log`, holding gate and action stdout and stderr interleaved with `#` marker lines for start, gate outcome, action, and finish. The run row stores the path as `log_pointer`. The daemon's own lifecycle lines go to `~/.ultradian/daemon.log`, which it rotates at 5MB keeping three; anything it writes to stdout or stderr goes to `daemon.out.log`.

The run record is the public integration surface. It carries stable `schedule_id` and `run_id` values, the machine id, the executor, the trigger, the status, both exit codes, timestamps, and the log pointer. Other tools consume that shape through `--json` and `--jsonl`. The run id is also the correlation handle for whatever the action invokes: a harness that tags its own artifacts with `ULTRADIAN_RUN_ID` can be joined back to the fire that spawned it.

## Intentionally absent

- a daemon protocol, socket, or HTTP surface;
- schedules spanning more than one machine;
- in-process third-party plugins;
- a full-screen TUI;
- implicit telemetry;
- automatic shell configuration edits;
- an automatic update mechanism; and
- commands loaded at runtime: every command is compiled in.

## 0.3.0 notes

0.3.0 is the Rust build of the 0.2.x contract: the same commands, envelopes, records, exit codes and database. Where 0.2.1 was arguably wrong but a consumer could tell the difference, 0.3.0 kept its behavior; those places can each be changed on purpose later, with their own release note:

- The gate's stdout reaches the action as decoded UTF-8 text (invalid bytes become U+FFFD, a leading byte-order mark is dropped), not byte for byte, and the log's "bytes of context" counts UTF-16 units.
- The 10 MB run-log cap counts stdout and stderr together, so where the cut falls between them depends on timing.
- Only a run's process group is stopped, so a grandchild that starts its own session and holds the output pipes open keeps the run waiting.
- Next fires follow croner 10.0.1 exactly, including `5#` reading as `5`, a fire asked for from inside a repeated DST hour, and off-by-one numbers in some error messages.
- After a refused `kickstart`, `daemon restart` bootstraps the service again while the first copy may still be loaded.

What changed:

- `daemon stop` returns as soon as the daemon has exited; a 0.2.1 daemon with a run in flight lingered about 15 s and made `daemon stop` report `daemon_stop_timeout`. For upgrades in place, stopping a daemon that has already released its lock waits up to 30 s for its pid.
- Pids at or below zero are never treated as alive, so a bad lock row can't make `daemon stop` signal a process group.
- `--catch-up` takes zero with any unit (`0s`, `0m`, `0h`, `0d`).
- `version --json` reports `runtime: "rust"` in place of the Bun version, and `doctor`'s runtime check reads "Runtime".
- `doctor` asks the system for the user name when checking systemd lingering, instead of reading `$USER`.

