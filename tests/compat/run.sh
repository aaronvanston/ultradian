#!/usr/bin/env bash
# Database compatibility between 0.2.1 (TypeScript, run with bun from
# legacy/) and the Rust build, in both directions:
#
#   1. a database 0.2.1 wrote reads the same through Rust,
#   2. a database Rust wrote opens and reads the same through 0.2.1 (the
#      rollback guarantee: Rust never moves user_version past 1),
#   3. the two builds take turns writing one database,
#   4. a 0.1 database (user_version 0) upgrades to the same rows under each,
#   5. a fresh database has the same schema, version, journal mode and
#      file modes whichever build created it.
#
#   RS_BIN=target/debug/ultradian tests/compat/run.sh
#
# TS_BIN overrides how 0.2.1 runs (default: bun legacy/src/index.ts). Every
# invocation gets a throwaway HOME and ULTRADIAN_HOME under mktemp and a
# PATH whose launchctl, systemctl and loginctl only record the call and
# fail; the run fails if any was reached. Nothing here starts a daemon.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
: "${RS_BIN:?Set RS_BIN to the Rust ultradian binary}"
case "$RS_BIN" in /*) ;; *) RS_BIN="$root/$RS_BIN" ;; esac
bun_path="$(command -v bun)" || { echo "bun is required to run 0.2.1" >&2; exit 2; }
if [ -n "${TS_BIN:-}" ]; then
  ts=("$TS_BIN")
else
  ts=("$bun_path" "$root/legacy/src/index.ts")
fi

system_tmp="$(cd "${TMPDIR:-/tmp}" && pwd -P)"
work="$(mktemp -d "$system_tmp/ultradian-compat.XXXXXX")"
work="$(cd "$work" && pwd -P)"
case "$work" in "$system_tmp"/*) ;; *) echo "refusing: $work" >&2; exit 2 ;; esac
trap 'rm -rf "$work"' EXIT

guard="$work/guard"
mkdir -p "$guard"
for tool in launchctl systemctl loginctl; do
  printf '#!/bin/sh\necho "%s $*" >>"%s/guard-calls.log"\nexit 97\n' "$tool" "$work" >"$guard/$tool"
  chmod +x "$guard/$tool"
done

failures=0
fail() {
  echo "FAIL  $*" >&2
  failures=$((failures + 1))
}

# in_home <home> <ts|rs> args... : one invocation, stdout only.
in_home() {
  local home="$1" which="$2"
  shift 2
  mkdir -p "$home/user" "$home/work"
  local program=("$RS_BIN")
  [ "$which" = ts ] && program=("${ts[@]}")
  (
    cd "$home/work"
    env -i HOME="$home/user" ULTRADIAN_HOME="$home/state" \
      PATH="$guard:/usr/bin:/bin:$(dirname "$bun_path")" TZ=UTC LANG=C NO_COLOR=1 \
      "${program[@]}" "$@" </dev/null
  )
}

# sql <db> <query>: rows as JSON, read through bun:sqlite. (Opening read-only
# makes bun exit silently on a WAL database.)
sql() {
  "$bun_path" -e '
    const { Database } = require("bun:sqlite");
    const db = new Database(process.argv[1]);
    console.log(JSON.stringify(db.query(process.argv[2]).all(), null, 1));
  ' "$1" "$2"
}

# The same read commands through both builds must print the same bytes.
compare_reads() {
  local home="$1" label="$2" name="$3"
  local reads=(
    "list --json"
    "runs --json"
    "runs --since 2 --limit 2 --json"
    "runs --limit 500 --json --compact"
    "status --json"
    "logs --json"
    "logs $name --limit 5 --json"
    "pause --group nobody --json"
  )
  for read in "${reads[@]}"; do
    # shellcheck disable=SC2086
    local from_ts from_rs
    from_ts="$(in_home "$home" ts $read 2>&1 || true)"
    from_rs="$(in_home "$home" rs $read 2>&1 || true)"
    if [ "$from_ts" != "$from_rs" ]; then
      fail "$label: '$read' differs"
      diff <(printf '%s\n' "$from_ts") <(printf '%s\n' "$from_rs") | head -20 >&2  || true
    fi
  done
  local version
  version="$(sql "$home/state/ultradian.db" "PRAGMA user_version" || true)"
  case "$version" in *'"user_version": 1'*) ;; *) fail "$label: user_version is $version" ;; esac
}

# The writes Arbor makes, by one build or alternating between them.
write_history() {
  local home="$1" first="$2" second="$3"
  mkdir -p "$home/work/automation"
  in_home "$home" "$first" add tick --cron "0 9 * * 1-5" --tz australia/sydney --timeout 6h \
    --catch-up 30m --gate 'sh "gate.sh"' --gate-mode exit --group arbor --cwd automation --yes --json -- /bin/sh run.sh >/dev/null
  in_home "$home" "$second" add hourly --every 1h --catch-up 0 --group arbor --yes --json -- echo 'quote " and ü' >/dev/null
  in_home "$home" "$first" add hand --yes --json -- true >/dev/null
  in_home "$home" "$second" set tick --every 15m --no-gate --timeout 2h --json >/dev/null
  in_home "$home" "$first" set hourly --tz Europe/Berlin --cron "30 2 * * *" --group loops --json >/dev/null
  in_home "$home" "$second" pause hand --json >/dev/null
  local run_id
  run_id="$(in_home "$home" "$first" run tick --detach --json | sed -n 's/.*"run_id": "\([^"]*\)".*/\1/p')"
  in_home "$home" "$second" run hourly --detach --json >/dev/null
  in_home "$home" "$second" cancel "$run_id" --json >/dev/null
  in_home "$home" "$first" run tick --detach --json >/dev/null
  in_home "$home" "$first" once --name import --json -- ./import.sh >/dev/null
  in_home "$home" "$second" rm hand --yes --json >/dev/null
}

echo "1. 0.2.1 writes, Rust reads"
home="$work/ts-wrote"
write_history "$home" ts ts
compare_reads "$home" "0.2.1-written" tick

echo "2. Rust writes, 0.2.1 reads"
home="$work/rs-wrote"
write_history "$home" rs rs
compare_reads "$home" "Rust-written" tick

echo "3. the builds take turns writing one database"
home="$work/mixed"
write_history "$home" rs ts
compare_reads "$home" "alternately written" tick

echo "4. a 0.1 database upgrades to the same rows under each"
legacy_seed="$work/legacy-seed.db"
"$bun_path" -e '
  const { Database } = require("bun:sqlite");
  const db = new Database(process.argv[1], { create: true });
  db.run(`CREATE TABLE schedules (
    id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE,
    trigger_kind TEXT NOT NULL, trigger_value TEXT, gate TEXT,
    command TEXT NOT NULL, working_directory TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT (\x27active\x27), created_at INTEGER NOT NULL,
    next_fire_at INTEGER, timeout_ms INTEGER, schedule_group TEXT)`);
  db.run(`CREATE TABLE runs (
    id TEXT PRIMARY KEY, schedule_id TEXT NOT NULL,
    schedule_name TEXT NOT NULL, machine_id TEXT NOT NULL, executor TEXT,
    trigger TEXT NOT NULL, status TEXT NOT NULL, gate_exit INTEGER,
    action_exit INTEGER, started_at INTEGER NOT NULL, finished_at INTEGER,
    log_pointer TEXT, owner_pid INTEGER NOT NULL)`);
  db.run("CREATE INDEX runs_by_schedule ON runs (schedule_name, started_at DESC)");
  db.run(`CREATE TABLE daemon (id INTEGER PRIMARY KEY CHECK (id = 1),
    pid INTEGER NOT NULL, started_at INTEGER NOT NULL, heartbeat_at INTEGER NOT NULL)`);
  db.run(`INSERT INTO schedules VALUES
    (\x27s1\x27, \x27tick\x27, \x27interval\x27, \x2715m\x27, NULL, \x27["true"]\x27, \x27/srv\x27, \x27active\x27, 1000, 2000, NULL, NULL),
    (\x27s2\x27, \x27nightly\x27, \x27cron\x27, \x270 2 * * *\x27, \x27true\x27, \x27["echo","ok"]\x27, \x27/srv\x27, \x27paused\x27, 1000, NULL, 60000, \x27ops\x27),
    (\x27s3\x27, \x27daily\x27, \x27interval\x27, \x271d\x27, NULL, \x27["true"]\x27, \x27/srv\x27, \x27active\x27, 1000, 3000, NULL, NULL)`);
  db.run(`INSERT INTO runs VALUES
    (\x27r2\x27, \x27s1\x27, \x27tick\x27, \x27box\x27, \x27true\x27, \x27manual\x27, \x27failed\x27, 0, 1, 3000, 3100, \x27/logs/r2.log\x27, 42),
    (\x27r1\x27, \x27s1\x27, \x27tick\x27, \x27box\x27, NULL, \x27scheduled\x27, \x27clean\x27, 0, NULL, 2000, 2100, NULL, 42)`);
  db.run("INSERT INTO daemon VALUES (1, 4242, 900, 950)");
  db.close();
' "$legacy_seed"
for which in ts rs; do
  mkdir -p "$work/legacy-$which/state"
  cp "$legacy_seed" "$work/legacy-$which/state/ultradian.db"
  in_home "$work/legacy-$which" "$which" list --json >"$work/legacy-$which/list.json"
  in_home "$work/legacy-$which" "$which" runs --json >"$work/legacy-$which/runs.json"
  for table in schedules runs counters daemon sqlite_master; do
    sql "$work/legacy-$which/state/ultradian.db" "SELECT * FROM $table ORDER BY 1, 2" \
      >"$work/legacy-$which/$table.json"
  done
done
for file in list.json runs.json schedules.json runs.json counters.json daemon.json sqlite_master.json; do
  if ! diff -u "$work/legacy-ts/$file" "$work/legacy-rs/$file" >&2; then
    fail "0.1 upgrade: $file differs"
  fi
done

echo "5. a fresh database is the same whichever build creates it"
for which in ts rs; do
  in_home "$work/fresh-$which" "$which" list --json >/dev/null
  db="$work/fresh-$which/state/ultradian.db"
  {
    sql "$db" "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY name"
    sql "$db" "PRAGMA user_version"
    sql "$db" "PRAGMA journal_mode"
    sql "$db" "SELECT * FROM counters"
    for path in "$work/fresh-$which/state" "$work/fresh-$which/state/logs" "$db"; do
      # stat differs between macOS and Linux; ls -ld's mode column doesn't.
      ls -ld "$path" | cut -c1-10
    done
  } >"$work/fresh-$which/shape.txt"
done
if ! diff -u "$work/fresh-ts/shape.txt" "$work/fresh-rs/shape.txt" >&2; then
  fail "a fresh database differs"
fi

if [ -s "$work/guard-calls.log" ]; then
  fail "a service manager was reached:"
  cat "$work/guard-calls.log" >&2
fi
if [ "$failures" -eq 0 ]; then
  echo "ok    database compatibility"
else
  echo "FAIL  $failures check(s)"
  exit 1
fi
