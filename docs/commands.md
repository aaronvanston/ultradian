# ultradian command reference

The commands of ultradian 0.3.0. This page is kept in step with `src/catalog.json` by hand; `ultradian schema --json` and `ultradian describe <command>` print the same catalog from the binary.

Gated schedules and workflows for invoking AI

## `ultradian add <name> <command...>`

Add a schedule with an optional gate

Creates a schedule. With --gate, the gate command runs first on every fire: exit 0 with no stdout records a clean pass, exit 0 with stdout opens the gate and the stdout is piped to the command's stdin, and a nonzero exit records a gate failure.

### Options

- `--cron <expression>`: Cron expression trigger.
- `--tz <zone>`: IANA time zone for the cron expression, defaulting to the machine's.
- `--every <duration>`: Interval trigger such as 30s, 15m, 2h, or 1d.
- `--gate <command>`: Gate command that decides whether the command runs.
- `--gate-mode <mode>`: When the gate opens: 'output' on exit 0 with stdout, 'exit' on exit 0 alone. Default: `output`.
- `--timeout <duration>`: Kill the gate or command after this long (such as 30m).
- `--catch-up <duration>`: Fire once after downtime if no later than this (such as 2h); 0 records a missed run instead.
- `--cwd <dir>`: Directory the gate and command run in, defaulting to the current one.
- `--group <name>`: Group this schedule with others under one heading.
- `--dry-run`: Show the schedule without saving it.
- `-y, --yes`: Confirm without prompting.

### Examples

```bash
ultradian add nightly-backup --cron "0 2 * * *" -- ./backup.sh
ultradian add sentry-check --cron "0 * * * *" --gate "bun check-sentry.ts" -- claude -p "Investigate the issues on stdin"
ultradian add heartbeat --every 30s -- echo ok
ultradian add oneshot -- ./task.sh
```

## `ultradian cancel <run_id>`

Cancel a queued or running run

Finishes a queued or running run as canceled. The process that owns the fire, the daemon or a foreground 'run', notices within a second and terminates the run's process group: SIGTERM, then SIGKILL after ten seconds. A queued run simply never starts.

### Examples

```bash
ultradian cancel run_abc123
ultradian cancel run_abc123 --json
```

## `ultradian completion <shell>`

Generate a shell completion script

### Examples

```bash
ultradian completion zsh > ~/.zfunc/_ultradian
ultradian completion fish > ~/.config/fish/completions/ultradian.fish
```

## `ultradian daemon install`

Install the daemon as a login service

Registers the daemon with launchd (macOS) or systemd (Linux) as a user service, so it starts at login and restarts after crashes. The service runs this binary at the path it was invoked from, so install a stable copy first with 'self install' and run this from it. PATH in the service defaults to your login shell's, so actions like claude or codex resolve as they do in a terminal; ULTRADIAN_HOME and any ULTRADIAN_RETENTION are recorded too. --dry-run prints the service file without writing it. A clean 'daemon stop' stays stopped until the next login or 'daemon restart'.

### Options

- `--path <path>`: PATH for the service, instead of the login shell's.
- `--dry-run`: Print the service file without installing it.

### Examples

```bash
ultradian daemon install --dry-run
~/.ultradian/bin/udian daemon install --json
ultradian daemon install --path "/opt/homebrew/bin:/usr/bin:/bin"
```

## `ultradian daemon restart`

Restart the daemon, picking up a replaced binary

Stops the daemon and starts a fresh one, through launchd or systemd when it is installed as a service. Runs in flight are interrupted. After 'self install' replaces the binary, this is what moves the daemon onto the new version.

### Examples

```bash
ultradian daemon restart
ultradian daemon restart --json
```

## `ultradian daemon run`

Run the scheduling daemon in the foreground

Runs the daemon loop in the foreground until interrupted. 'daemon start' launches this in the background; running it directly suits supervisors such as launchd or systemd. It writes daemon.log in ULTRADIAN_HOME, rotated at 5MB with three old files kept, and prunes run history older than ULTRADIAN_RETENTION (default 30d, 'off' to keep everything) once a day.

### Examples

```bash
ultradian daemon run
```

## `ultradian daemon start`

Start the scheduling daemon in the background

### Examples

```bash
ultradian daemon start
```

## `ultradian daemon stop`

Stop the scheduling daemon

### Examples

```bash
ultradian daemon stop
```

## `ultradian daemon uninstall`

Remove the daemon login service and stop the daemon

### Examples

```bash
ultradian daemon uninstall
```

## `ultradian describe <command...>`

Describe one command and its contract

### Examples

```bash
ultradian describe add
ultradian describe daemon start --json
```

## `ultradian doctor`

Check the local install and configuration

Runs fast, offline checks. It does not contact the configured service.

### Examples

```bash
ultradian doctor
ultradian doctor --json
```

## `ultradian list`

List schedules and their latest run

### Examples

```bash
ultradian list
ultradian list --json
```

## `ultradian logs [name]`

Show recent runs and their captured output

### Options

- `--limit <count>`: Maximum runs to show. Default: `10`.
- `--run <id>`: Include the captured log for one run id.

### Examples

```bash
ultradian logs
ultradian logs sentry-check
ultradian logs sentry-check --run run_abc123 --json
```

## `ultradian once <command...>`

Run a command once under the daemon and record the outcome

Registers a one-shot job and returns its identity immediately, without waiting. The daemon runs it once, as soon as it can, and records the outcome as an ordinary run: the same statuses, the same captured log, and the same appearance in 'runs', where the job id is the record's schedule_id and the trigger reads as 'once'. A job registered while the daemon is stopped runs when the daemon returns. One-shot jobs have no gate, and they never appear in 'list'.

### Options

- `--name <label>`: Label the job so its run is easy to spot.
- `--cwd <dir>`: Directory to run in, defaulting to the current one.
- `--timeout <duration>`: Kill the command after this long (such as 30m).

### Examples

```bash
ultradian once -- ./deploy.sh
ultradian once --name import --timeout 30m -- ./import.sh
ultradian once --cwd ~/work/api --json -- bun run migrate
```

## `ultradian pause [name]`

Stop triggering a schedule or group without removing it

### Options

- `--group <name>`: Pause every schedule in a group.

### Examples

```bash
ultradian pause sentry-check
ultradian pause --group checks
```

## `ultradian prune [name]`

Delete old runs and their log files

Deletes finished runs that started before the cutoff, along with their captured log files. Queued and running runs and the schedules themselves are untouched. The daemon also prunes once a day by itself, keeping ULTRADIAN_RETENTION (default 30d, 'off' to keep everything).

### Options

- `--older-than <duration>`: Delete runs older than this (such as 7d or 12h).
- `-y, --yes`: Confirm without prompting.

### Examples

```bash
ultradian prune --older-than 30d --yes
ultradian prune sentry-check --older-than 7d --yes
```

## `ultradian resume [name]`

Start triggering a paused schedule or group again

### Options

- `--group <name>`: Resume every schedule in a group.

### Examples

```bash
ultradian resume sentry-check
ultradian resume --group checks
```

## `ultradian rm <name>`

Remove a schedule, keeping its run history

### Options

- `-y, --yes`: Confirm without prompting.

### Examples

```bash
ultradian rm sentry-check --yes
```

## `ultradian run <name>`

Trigger a schedule now, in the foreground or queued for the daemon

Fires the schedule now through the same gate and runner the daemon uses. In the foreground it waits for the result and exits nonzero unless the run succeeded or the gate closed cleanly. With --detach it queues the fire for the daemon and returns the queued run straight away. Either way a schedule with a run already in flight is refused with run_in_flight.

### Options

- `--detach`: Queue the fire for the daemon and return its run id.

### Examples

```bash
ultradian run sentry-check
ultradian run backup --json
ultradian run sentry-check --detach --json
```

## `ultradian runs`

Emit run records for other tools

Emits run records for other tools. This is the read surface: consumers ingest these records instead of touching the database. Every change to a run (queued, started, finished, canceled) gives it a new revision, and records come oldest change first. The output cursor is the last revision returned; hand it back with --since to receive only runs that changed after it, each in its latest state. Page with --limit. Pruned runs simply stop appearing.

### Options

- `--since <cursor>`: Only runs that changed after this cursor.
- `--limit <count>`: Maximum runs to emit.

### Examples

```bash
ultradian runs --json
ultradian runs --since 42 --limit 500 --jsonl
```

## `ultradian schema`

Print the machine-readable command catalog

### Examples

```bash
ultradian schema --json
```

## `ultradian self install`

Install this binary at a stable path

Copies this compiled binary to a stable path, atomically: it writes a temporary file beside the target and renames it into place, so nothing ever runs a half-written binary. Upgrading is running the new binary's 'self install' and then 'daemon restart'; a service installed from the stable path keeps pointing at it.

### Options

- `--to <path>`: Where to install, defaulting to bin/udian in ULTRADIAN_HOME.

### Examples

```bash
./ultradian self install
./ultradian self install --to ~/.ultradian/bin/udian --json
```

## `ultradian set <name> [command...]`

Change any part of a schedule in place

Changes only what you pass; everything else stays as it is. Changing the trigger or its time zone recomputes the next fire from now. Everything after -- replaces the command.

### Options

- `--cron <expression>`: New cron expression trigger.
- `--every <duration>`: New interval trigger such as 30s, 15m, 2h, or 1d.
- `--manual`: Make it a manual-only schedule.
- `--tz <zone>`: Time zone for the cron trigger, or 'local' for the machine's.
- `--gate <command>`: New gate command.
- `--no-gate`: Remove the gate.
- `--gate-mode <mode>`: When the gate opens: 'output' or 'exit'.
- `--timeout <duration>`: New timeout such as 30m.
- `--no-timeout`: Remove the timeout.
- `--catch-up <duration>`: New catch-up window such as 2h, or 0.
- `--group <name>`: New group.
- `--no-group`: Remove it from its group.
- `--cwd <dir>`: New working directory.

### Examples

```bash
ultradian set sentry-check --every 5m
ultradian set nightly-backup --cron "0 3 * * *" --tz Europe/London
ultradian set sentry-check --timeout 1h --catch-up 2h --group loops
ultradian set sentry-check --no-gate --cwd ~/work/api
ultradian set sentry-check --json -- claude -p 'Investigate the issues on stdin'
```

## `ultradian status`

Show daemon health and active runs

### Examples

```bash
ultradian status
ultradian status --json
```

## `ultradian version`

Show detailed version and runtime information

### Examples

```bash
ultradian version
ultradian version --json
```
