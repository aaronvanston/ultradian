#!/usr/bin/env bash
# Replays the 0.2.1 contract against any ultradian binary and diffs it.
#
#   BIN=/path/to/ultradian tests/contract/run.sh            # check every scenario
#   BIN=... tests/contract/run.sh arbor errors              # check some
#   BIN=... tests/contract/run.sh --record                  # rewrite golden/ (0.2.1 only)
#   BIN=... KEEP=1 tests/contract/run.sh                    # keep the actual output
#
# Each scenario in scenarios/ runs in its own throwaway HOME and
# ULTRADIAN_HOME under mktemp, with a scrubbed environment (env -i), TZ=UTC,
# stdin from /dev/null and cwd in the scenario's temp work folder. Each step
# becomes one file: the masked argv, the exit code, stdout and stderr, all
# passed through normalize.pl. Steps marked "full" must match the golden
# file byte for byte; "json" steps match argv, exit, stdout and the JSON
# envelope on stderr (not human text printed before it); "exit" steps
# (human output, which may differ slightly in the rewrite) only need the
# same argv and exit code.
#
# Intentional 0.3.0 changes live in overrides/<scenario>/<step>.txt. They
# replace the golden file unless the binary under test is the TypeScript
# build (its version --json still has a "bun" field).
#
# Safety: a guard folder first on PATH holds launchctl and systemctl stubs
# that only record that they were called and fail. No scenario installs a
# service; the run fails if any stub was reached anyway. HOME never points at
# a real home, and the run refuses to start if the temp root isn't under
# the system temp directory.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
record=0
scenarios=()
for argument in "$@"; do
  case "$argument" in
    --record) record=1 ;;
    -*) echo "unknown flag $argument" >&2; exit 2 ;;
    *) scenarios+=("$argument") ;;
  esac
done
if [ "${#scenarios[@]}" -eq 0 ]; then
  for file in "$here"/scenarios/*.sh; do
    name="$(basename "$file" .sh)"
    scenarios+=("$name")
  done
fi

: "${BIN:?Set BIN to the ultradian binary under test}"
case "$BIN" in /*) ;; *) BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")" ;; esac
[ -x "$BIN" ] || { echo "BIN $BIN is not executable" >&2; exit 2; }

os="$(uname -s | tr '[:upper:]' '[:lower:]')"
system_tmp="$(cd "${TMPDIR:-/tmp}" && pwd -P)"
run_root="$(mktemp -d "$system_tmp/ultradian-contract.XXXXXX")"
run_root_real="$(cd "$run_root" && pwd -P)"
case "$run_root_real" in
  "$system_tmp"/*) ;;
  *) echo "refusing: $run_root_real is outside $system_tmp" >&2; exit 2 ;;
esac

export CONTRACT_BIN="$BIN"
export CONTRACT_HOST="$(hostname)"

# Every invocation, including the probes below, runs in a throwaway home.
probe_home="$run_root_real/probe"
mkdir -p "$probe_home/home" "$probe_home/state"
probe() {
  env -i HOME="$probe_home/home" ULTRADIAN_HOME="$probe_home/state" \
    PATH=/usr/bin:/bin TZ=UTC "$BIN" "$@" </dev/null 2>/dev/null
}
export CONTRACT_VERSION="$(probe --version)"
use_overrides=1
if probe version --json | grep -q '"bun"'; then
  use_overrides=0
fi
if [ "$record" -eq 1 ] && [ "$use_overrides" -eq 1 ]; then
  echo "refusing to record golden files from a binary that isn't the 0.2.1 TypeScript build" >&2
  exit 2
fi

# The step currently being written, and the scenario's folders.
step_number=0
scenario=""
scenario_root=""
out_dir=""
WORK=""
LAST_STDOUT=""
failures=0

# Runs the binary inside the scenario's sandbox. Used by step and by
# unrecorded helpers (waits, cleanup).
sandboxed() {
  (
    cd "$WORK"
    env -i \
      HOME="$scenario_root/home" \
      ULTRADIAN_HOME="$scenario_root/state" \
      PATH="$scenario_root/guard:/usr/bin:/bin" \
      TZ=UTC LANG=C NO_COLOR=1 \
      "$BIN" "$@" </dev/null
  )
}

# Shell-quotes one word the same way on every bash, unlike printf %q.
quote() {
  case "$1" in
    '') printf "''" ;;
    *[!A-Za-z0-9_./:=@%+,-]*) printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")" ;;
    *) printf '%s' "$1" ;;
  esac
}

# step <slug> <full|exit> -- <args...>
step() {
  local slug="$1" compare="$2"
  shift 2
  [ "$1" = "--" ] && shift
  step_number=$((step_number + 1))
  local id
  id="$(printf '%02d-%s' "$step_number" "$slug")"
  local stdout_file="$scenario_root/raw/$id.out" stderr_file="$scenario_root/raw/$id.err"
  local code=0
  sandboxed "$@" >"$stdout_file" 2>"$stderr_file" || code=$?
  LAST_STDOUT="$(cat "$stdout_file")"
  local state="$scenario_root/ids.tsv"
  {
    printf '# args:'
    local word
    for word in "$@"; do printf ' %s' "$(quote "$word")"; done
    printf '\n# compare: %s\n--- exit\n%s\n--- stdout\n' "$compare" "$code"
    cat "$stdout_file"
    printf '%s\n' '--- stderr'
    cat "$stderr_file"
  } | CONTRACT_TMP="$scenario_root" CONTRACT_TMP_REAL="$scenario_root" \
    perl "$here/normalize.pl" "$state" >"$out_dir/$id.txt"
}

# The comparable part of a step file. "full": all of it. "json": argv,
# exit, stdout, and from stderr only the JSON envelope (from the first line
# that is exactly "{"). "exit": argv and exit code only.
comparable() {
  local file="$1"
  case "$(sed -n 's/^# compare: //p' "$file")" in
    exit) sed -n '1,/^--- stdout$/p' "$file" ;;
    json)
      sed -n '1,/^--- stderr$/p' "$file"
      sed -n '/^--- stderr$/,$p' "$file" | sed -n '/^{$/,$p'
      ;;
    *) cat "$file" ;;
  esac
}

# Polls until no run is queued or running (the daemon finished them).
wait_idle() {
  local deadline=$((SECONDS + ${1:-40}))
  local status
  while [ $SECONDS -lt $deadline ]; do
    # A binary whose status fails can't be waited on; the steps that
    # follow will show the difference.
    status="$(sandboxed status --json 2>/dev/null)" || return 0
    if printf '%s' "$status" | grep -q '"active_runs": \[\]'; then
      return 0
    fi
    sleep 0.3
  done
  # Not fatal: the steps that follow show what state the runs were in.
  echo "  timed out waiting for runs to finish in $scenario" >&2
}

# Polls until a run with this status is in flight.
wait_status() {
  local status="$1" deadline=$((SECONDS + ${2:-20}))
  local output
  while [ $SECONDS -lt $deadline ]; do
    output="$(sandboxed status --json 2>/dev/null)" || return 0
    if printf '%s' "$output" | grep -q "\"status\": \"$status\""; then
      return 0
    fi
    sleep 0.2
  done
  echo "  timed out waiting for a $status run in $scenario" >&2
}

# Pulls a string field out of the last step's stdout.
last_field() {
  printf '%s' "$LAST_STDOUT" | { grep -o "\"$1\": \"[^\"]*\"" || true; } | head -n 1 | sed 's/.*: "\(.*\)"/\1/'
}

# Stops any daemon the scenario left behind, by the pid in its own lock row.
cleanup_scenario() {
  [ -n "$scenario_root" ] || return 0
  [ -d "$scenario_root/state" ] || return 0
  sandboxed daemon stop --json >/dev/null 2>&1 || true
}
trap cleanup_scenario EXIT

for name in "${scenarios[@]}"; do
  file="$here/scenarios/$name.sh"
  [ -f "$file" ] || { echo "no scenario $name" >&2; exit 2; }
  only_os="$(sed -n 's/^# os: //p' "$file")"
  if [ -n "$only_os" ] && [ "$only_os" != "$os" ]; then
    echo "skip  $name (needs $only_os)"
    continue
  fi
  scenario="$name"
  scenario_root="$run_root_real/$name"
  WORK="$scenario_root/work"
  out_dir="$scenario_root/actual"
  mkdir -p "$scenario_root/home" "$scenario_root/state" "$scenario_root/guard" \
    "$scenario_root/raw" "$WORK" "$out_dir"
  for tool in launchctl systemctl loginctl; do
    cat >"$scenario_root/guard/$tool" <<EOF
#!/bin/sh
echo "$tool \$*" >>"$scenario_root/guard-calls.log"
exit 97
EOF
    chmod +x "$scenario_root/guard/$tool"
  done
  step_number=0
  # shellcheck source=/dev/null
  source "$file"
  name="$scenario"
  cleanup_scenario
  if [ -s "$scenario_root/guard-calls.log" ]; then
    echo "FAIL  $name reached a service manager:" >&2
    cat "$scenario_root/guard-calls.log" >&2
    failures=$((failures + 1))
  fi

  golden="$here/golden/$name"
  if [ "$record" -eq 1 ]; then
    rm -rf "$golden"
    mkdir -p "$golden"
    cp "$out_dir"/*.txt "$golden"/
    echo "rec   $name ($step_number steps)"
    continue
  fi
  scenario_failures=0
  for actual in "$out_dir"/*.txt; do
    step_file="$(basename "$actual")"
    expected="$golden/$step_file"
    override="$here/overrides/$name/$step_file"
    if [ "$use_overrides" -eq 1 ] && [ -f "$override" ]; then
      expected="$override"
    fi
    if [ ! -f "$expected" ]; then
      echo "  $name/$step_file: no golden file" >&2
      scenario_failures=$((scenario_failures + 1))
      continue
    fi
    if ! diff -u --label "expected $name/$step_file" --label "actual" \
      <(comparable "$expected") <(comparable "$actual") >&2; then
      scenario_failures=$((scenario_failures + 1))
    fi
  done
  for expected in "$golden"/*.txt; do
    [ -f "$out_dir/$(basename "$expected")" ] || {
      echo "  $name/$(basename "$expected"): step never ran" >&2
      scenario_failures=$((scenario_failures + 1))
    }
  done
  if [ "$scenario_failures" -eq 0 ]; then
    echo "ok    $name ($step_number steps)"
  else
    echo "FAIL  $name ($scenario_failures step(s) differ)"
    failures=$((failures + scenario_failures))
  fi
done
scenario_root=""

if [ "${KEEP:-0}" = "1" ]; then
  echo "kept $run_root_real"
else
  rm -rf "$run_root_real"
fi
[ "$failures" -eq 0 ]
