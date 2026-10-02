# Architecture

## Core seam

```text
feature module
  ├─ command descriptors ──> parser registration
  │                       ├─> human help and examples
  │                       ├─> schema / describe
  │                       ├─> Markdown command reference
  │                       └─> shell completions
  ├─ service providers ───> lazy typed per-run service registry
  └─ doctor checks ───────> fast offline diagnostics

handler ──> typed outcome ──> human renderer | JSON | JSONL
```

Commander is an implementation detail behind the descriptor binder. Feature code never imports it. The grammar can therefore remain mature while the catalog, services, docs, and future protocol surfaces stay framework-independent.

## Layers

### Engine

`src/engine/` owns command registration, parsing, lifecycle, signals, lazy services, help, presentation, errors, machine output, catalog generation, and completion generation.

It has no feature or product knowledge.

### Modules

`src/modules/` contains statically imported feature slices:

- `schedules`: schedules, gates, runs, the store, and the daemon; and
- `system`: version, doctor, schema, describe, and completion.

Static imports keep standalone Bun builds deterministic.

### Services

Commands request services through typed tokens. Providers construct lazily inside one CLI run, may depend on other providers, and may register disposal callbacks.

This gives commands a small execution context without globals, an application container, or eager startup work. Every command that touches state resolves the same `schedule-store` token, so the database path, schema creation, and connection disposal cannot drift between commands.

### Presentation

Handlers return data. Human renderers turn validated data into terminal text. JSON and JSONL bypass human layout and serialize the same validated values.

The theme exposes semantic roles instead of raw colors. Status wording and symbols remain present when color is disabled.

## Execution lifecycle

1. Preflight scans universal presentation flags so usage errors respect machine and color modes.
2. The descriptor catalog is validated for duplicate paths and aliases.
3. Commander binds the catalog and parses argv.
4. One abort controller listens for `SIGINT` and `SIGTERM`.
5. The selected handler resolves only the services it uses.
6. Options, inputs, and outputs pass through boundary schemas.
7. The engine renders exactly one outcome.
8. Services dispose in reverse construction order.
9. The entry point sets `process.exitCode`; command code never exits directly.

## Human and headless behavior

Interactive execution requires human output, TTY input and diagnostics, no CI marker, and no explicit non-interactive flag. Every prompt has a complete flag-based route. A write that needs confirmation fails with `action_required` and a copy-pasteable retry hint when interaction is unavailable.

Clack is loaded lazily for all prompts. Prompts are presentation adapters; commands retain complete flag-based routes.

## The schedules module

### The store is the bus

`src/modules/schedules/store.ts` owns a `bun:sqlite` database at `~/.ultradian/ultradian.db`, relocatable with `ULTRADIAN_HOME`. It holds `schedules`, `runs`, a single-row `daemon` table, and a `counters` table that gives every change to a run the next revision number, which is what `runs --since` pages by. Its schema version lives in SQLite's `user_version` and moves forward through ordered migrations; a database newer than the binary is refused with `database_too_new`, and one written by 0.1.x, before versions, is carried into the current shape in place with its schedules, run history and daemon lock. WAL journaling and a busy timeout let the CLI and the daemon hold it at the same time. The folder is `0700` and the database and logs `0600`.

The store is the only channel between them. There is no socket, no RPC, and no running server to talk to. `add` inserts a row and the daemon picks it up on its next tick; `list`, `logs`, and `status` read rows and work exactly the same with the daemon stopped. A stopped daemon costs future fires, and it costs nothing else.

### Triggers

`triggers.ts` parses the three trigger kinds and computes the next fire. [croner](https://github.com/hexagon/croner) parses cron expressions and answers `nextRun`; intervals parse durations such as `30s`, `15m`, `2h`, and `1d`; manual schedules have no next fire and only move through `run`.

### One-shot jobs

`once` registers a job and returns immediately. The job is a row in the same `schedules` table, marked `kind = 'once'`, with a manual trigger and a next fire of right now. That is the whole mechanism: the daemon's existing claim query picks it up on the next tick, and claiming a manual trigger advances it to no next fire, so it runs exactly once. A job registered with the daemon stopped keeps its past due time, because recovery only skips missed fires forward for real schedules, so it runs when the daemon returns.

Every schedule-facing read filters on `kind = 'schedule'`, so a one-shot never appears in `list` and `rm`, `set`, `pause`, and `resume` cannot reach it. It has no gate by design. The daemon deletes the row once the fire finishes: the job is consumed by its run, and the run record is what survives. Recovery sweeps the rows a crashed daemon fired but never removed.

The run it produces is an ordinary run: the same statuses, the same day-partitioned log, the same shape in `runs`, with `trigger` reading `once` and `schedule_id` carrying the job id that `once` printed.

### The runner and gate semantics

`runner.ts` executes one fire, from the daemon or from `run`, through the same path. It creates the run row first, so a fire is visible as `running` while it happens, then:

1. The gate, when the schedule has one, runs under `/bin/sh -c` with stdin ignored. Its stdout is captured and also written to the run log.
2. A nonzero gate exit finishes the run as `gate_failed`, and the action never runs.
3. A zero exit with empty stdout finishes the run as `clean`, and the action never runs, unless the schedule's gate mode is `exit`, where exit 0 alone opens the gate.
4. A zero exit with stdout opens the gate. The runner records the executor (the basename of the action's first argument) and spawns the action with that stdout as its stdin.
5. The action's exit code decides `succeeded` or `failed`.

Both processes inherit the environment plus `ULTRADIAN_RUN_ID` and `ULTRADIAN_SCHEDULE`; the action also receives `ULTRADIAN_SESSION_ID`. The schedule's recorded working directory is the cwd for both.

Each process starts in its own process group, whose id is stored on the run. A timeout, a `cancel` or a daemon shutdown sends the whole group `SIGTERM`, then `SIGKILL` ten seconds later, so children an action started in the background end with it. A run's log keeps its first 10MB of output and then a truncation marker; the finish marker is always written.

### Daemon lifecycle

`daemon.ts` runs a one-second tick loop. Each tick writes a heartbeat row, starts runs queued by `run --detach`, claims every schedule whose `next_fire_at` has passed (advancing it in the same transaction so a claim happens once), and fires the claimed schedules concurrently. A tick that throws is logged and the loop carries on. Once a day it prunes runs and logs older than `ULTRADIAN_RETENTION` (default 30 days). A schedule whose previous run is still in flight records a `skipped` run, so the gap stays visible in its history.

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
- runtime filesystem scanning for commands in compiled executables.

Feature modules are imported statically so Bun can compile one deterministic executable. Lazy service construction provides most startup benefits without a dynamic command loader.
