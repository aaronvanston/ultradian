---
name: ultradian
description: Use when operating the ultradian CLI to schedule gated loops, add or inspect schedules, trigger runs, read run records and logs, or diagnose the local daemon.
---

# Ultradian skill

Ultradian runs gated schedules on the local machine. A schedule pairs a trigger (cron, interval, or manual), an optional gate command, and an action command. A local daemon fires due schedules and records every run.

Both `ultradian` and `udian` are the same binary. Use `udian` when typing by hand; the examples below spell it out in full.

## Use this skill when

- adding, pausing, resuming, or removing a schedule;
- triggering a schedule now and waiting for the result;
- checking daemon health or what is currently running;
- reading run history and captured output; or
- diagnosing a local install that is behaving oddly.

Do not use this file as a substitute for command discovery. The descriptor catalog is the source of truth.

## Discover commands

Start with the cheapest relevant surface:

```bash
ultradian schema --json          # every command, argument, flag, and output schema
ultradian describe add           # one command's contract
ultradian --help
```

Use `schema --json` when selecting a command programmatically. Use `describe` when the path is known and the arguments, flags, examples, or write classification need inspection.

## Prefer headless execution

When an agent invokes the CLI:

- pass `--json` for one bounded result;
- pass `--jsonl` for versioned event records;
- pass `--non-interactive` when prompts must be impossible;
- set `NO_COLOR=1` when capturing human text;
- read stdout as data and stderr as diagnostics; and
- branch on exit status and the stable error code, never on message text.

Never drive prompts with keystrokes. Every prompt has a flag route.

## Add a schedule

`add` is a write. Preview it first, then apply with explicit confirmation:

```bash
ultradian add sentry-check --cron "0 * * * *" \
  --gate "bun check-sentry.ts" \
  --dry-run --json \
  -- claude -p "Investigate the Sentry issues on stdin and open a fix PR"
```

```bash
ultradian add sentry-check --cron "0 * * * *" \
  --gate "bun check-sentry.ts" \
  --yes --json \
  -- claude -p "Investigate the Sentry issues on stdin and open a fix PR"
```

Everything after `--` is the action command. Triggers are `--cron <expression>` (with `--tz <zone>` for a time zone other than the machine's), `--every <duration>` (`30s`, `15m`, `2h`, `1d`), or neither for a manual-only schedule. The gate and command run in `--cwd <dir>`, defaulting to the directory you add it from. `--timeout` kills a run that outlives it, `--catch-up` lets a fire missed while the daemon was down still run once within that window, and `--gate-mode exit` opens the gate on exit 0 alone, with or without output.

Change a schedule in place with `set`, passing only what changes; everything after `--` replaces the command:

```bash
ultradian set sentry-check --every 30m --json
ultradian set sentry-check --no-gate --json -- ./triage.sh
```

Do not add `--yes` merely to make an error disappear. Confirm the name, trigger, gate, and command first. Without `--yes` in a non-interactive context the command fails with `action_required` and a retry hint.

## Run something once

`once` hands a single command to the daemon and returns immediately with the job's identity. Use it for work that should run under supervision and leave a record, without becoming a schedule.

```bash
ultradian once --json -- ./deploy.sh
ultradian once --name import --cwd ~/work/api --timeout 30m --json -- bun run migrate
```

It takes no confirmation flag: the command was given explicitly, so it registers headlessly the way `run` fires headlessly. `--cwd` defaults to the current directory. There is no gate.

The job runs once, as soon as the daemon can take it, and its outcome is an ordinary run: same statuses, same captured log, same shape in `runs`, with `trigger` reading `once`. To find it, match the printed `job_id` against `schedule_id` in the run records:

```bash
ultradian runs --json    # the job's run appears here once the daemon fires it
ultradian logs --run <run_id> --json
```

Registering while the daemon is stopped is safe. The job waits and runs when the daemon returns, and `once` says so in its hint.

## The gate contract

The gate runs first on every fire and speaks through its exit code and stdout:

- exit 0 with empty stdout: clean pass, the action never runs, the run is recorded as `clean`;
- exit 0 with stdout: the gate opens, and that stdout is piped to the action's stdin as context; and
- nonzero exit: the run is recorded as `gate_failed` and the action never runs.

So a gate script has one job: decide, and when the answer is yes, describe why on stdout. Keep it cheap and deterministic.

Run statuses are `queued` (waiting for the daemon), `running`, `clean` (the gate closed), `succeeded` or `failed` (the action ran), `gate_failed`, `timed_out` (the action outlived the schedule's `--timeout` and its process group was killed), `canceled` (stopped with `cancel`), `skipped` (the previous run was still in flight), `missed` (a fire passed while the daemon was down and outside the schedule's `--catch-up` window), and `interrupted` (the owning process died mid-run). Triggers are `scheduled`, `manual`, and `once`.

Inside a gate or action, the runner sets `ULTRADIAN_RUN_ID` and `ULTRADIAN_SCHEDULE`. Tag any artifacts the action produces with the run id so they can be traced back to the fire that created them. The action also gets `ULTRADIAN_SESSION_ID`, which is the run id again, not an agent session.

## Tie a run to its agent session

Each run also hands the action `ULTRADIAN_AGENT_SESSION_ID`, a fresh lowercase UUID, and `ULTRADIAN_AGENT_SESSION_FILE`, a private file path. The run records one of them as `agent_session_id`, so tools reading runs can open the session a run started. Write every agent schedule so its run records its session.

The action runs as argv, not through a shell: a `"$VAR"` typed after `--` is expanded by your own shell when you run `add`, before the schedule exists. Wrap the action in `sh -c '...'` with single quotes so the variable expands at each run.

An agent that takes a session id, such as Claude Code, gets the UUID:

```bash
ultradian add triage --cron "0 9 * * 1-5" --yes --json \
  -- sh -c 'claude -p --session-id "$ULTRADIAN_AGENT_SESSION_ID" "Triage the new issues"'
```

An agent that picks its own id, such as `codex exec`, reports it by writing the id as the first line of the file. `codex exec --json` opens its event stream with `{"type":"thread.started","thread_id":"…"}`; this keeps the stream in the run log through stderr, writes the thread id to the file, and keeps codex's exit status with `pipefail`:

```bash
ultradian add review --cron "0 * * * *" --yes --json \
  -- bash -o pipefail -c 'codex exec --json "Review open PRs" | tee /dev/stderr | jq -r "select(.type == \"thread.started\") | .thread_id" > "$ULTRADIAN_AGENT_SESSION_FILE"'
```

The rule the run follows:

1. a valid id written to `$ULTRADIAN_AGENT_SESSION_FILE` wins: its first line, trimmed, 1 to 128 characters of letters, digits, `.`, `_`, `:` or `-`; anything else is ignored;
2. otherwise the UUID, when the command line contains `ULTRADIAN_AGENT_SESSION_ID`;
3. otherwise `agent_session_id` is null.

A wrapper script that passes the UUID on without naming it in the command line should report it: `printf '%s\n' "$ULTRADIAN_AGENT_SESSION_ID" > "$ULTRADIAN_AGENT_SESSION_FILE"`. Never try to recover an id from the action's output afterwards; Ultradian doesn't.

## Inspect loops

```bash
ultradian list --json                          # schedules, next fire, last outcome
ultradian status --json                        # daemon health and active runs
ultradian logs --json                          # recent runs across all schedules
ultradian logs sentry-check --limit 20 --json  # one schedule's history
ultradian logs sentry-check --run <run_id> --json   # one run, with captured output
```

Every run record carries `agent_session_id`, a string or null.

Reads work with the daemon stopped: the CLI and the daemon share the SQLite store, so history stays readable either way. `list` and `logs` are the two commands worth reaching for when asked how a loop has been going.

## Trigger and control

```bash
ultradian run sentry-check --json            # fire now in the foreground and wait
ultradian run sentry-check --detach --json   # queue it for the daemon and return the run id
ultradian cancel <run_id> --json             # stop a queued or running run
ultradian pause sentry-check
ultradian resume sentry-check
ultradian rm sentry-check --yes
```

`run` uses the same gate and runner the daemon uses, so it is the right way to test a schedule before leaving it to fire on its own. A schedule with a run already in flight refuses another with `run_in_flight`. `cancel` and timeouts end the run's whole process group. `rm` keeps the run history.

Tools that follow runs read `ultradian runs --since <cursor> --json`: every change to a run gives it a new revision, records come oldest change first, and the returned `cursor` goes back in as `--since` to get only what changed after it.

## The daemon

```bash
ultradian daemon start     # background
ultradian daemon stop
ultradian daemon restart   # through launchd or systemd when installed there
ultradian daemon install --dry-run --json   # preview the login service
ultradian daemon run       # foreground, for launchd or systemd
```

To keep the daemon across logins and crashes, copy a release build to a stable path with `self install` (it lands in `~/.ultradian/bin/udian`), then run `daemon install` from there. The service gets the login shell's `PATH` so actions like `claude` resolve; `--path` sets it explicitly. Upgrading is `self install` from the newer build, then `daemon restart`. Do not install or restart the service on someone's machine without being asked.

Only one daemon runs at a time; a second start fails with `daemon_already_running`. On start it recovers: runs whose owning process is gone are marked `interrupted` and their process groups ended, and each schedule that missed fires while it was down fires once if its due fire is no later than its `--catch-up` window, or records one `missed` run and skips forward otherwise, never a burst. Schedules added while the daemon is stopped stay dormant until it starts, and `add` says so in its hint.

## Configuration

- All environment variables use the `ULTRADIAN_` prefix.
- `ULTRADIAN_HOME` relocates all state. It defaults to `~/.ultradian` and holds `ultradian.db`, `daemon.log`, and `logs/<schedule>/<YYYY-MM-DD>/<run_id>.log`, readable only by the owner.
- `ULTRADIAN_RETENTION` is how long the daemon keeps run history (default `30d`, `off` to keep everything). Each run's log is capped at 10MB.
- A local `.env` works during development. Compiled binaries deliberately do not auto-load it.

Point `ULTRADIAN_HOME` at a scratch directory when experimenting, so test schedules stay out of the real database.

## Handle failures

For an expected error:

1. read the exit status;
2. parse the structured error code in JSON mode;
3. inspect `hint` and `details`;
4. correct the input or configuration; and
5. retry the exact intended command.

Common codes: `schedule_not_found`, `schedule_exists`, `invalid_schedule_name`, `invalid_cron`, `invalid_timezone`, `timezone_requires_cron`, `invalid_duration`, `invalid_working_directory`, `conflicting_triggers`, `command_required`, `nothing_to_set`, `action_required`, `run_not_found`, `run_finished`, `run_in_flight`, `invalid_cursor`, `group_not_found`, `daemon_already_running`, `daemon_start_timeout`, `daemon_stop_timeout`, `daemon_install_failed`, `not_a_compiled_binary`, and `database_too_new`.

Useful diagnostics:

```bash
ultradian doctor --json    # data directory writability and daemon state
ultradian version --json
```

`doctor` is offline and fast. Do not echo environment values or credentials while reporting diagnostics.
