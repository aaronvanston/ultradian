#!/usr/bin/env bash
# Records every launchctl call `daemon install|restart|uninstall` makes, in
# order, for two builds, and diffs them. This is the behavior that touches
# real machines, so it is compared exactly.
#
#   TS_BIN=legacy/dist/ultradian RS_BIN=target/debug/ultradian tests/service/launchctl-calls.sh
#
# macOS only (launchd). Each scenario runs in a throwaway HOME and
# ULTRADIAN_HOME, with a PATH whose launchctl is a stub: it records its
# arguments (the uid masked), fails the subcommands STUB_FAIL names, and
# plays the supervisor by starting or stopping the build's own daemon in
# that throwaway home, so commands that wait for a heartbeat finish.
# systemctl and loginctl stubs fail outright. Every daemon a scenario
# starts is stopped by pid at its end. The real ~/Library/LaunchAgents is
# checked untouched before and after.
set -euo pipefail

[ "$(uname -s)" = Darwin ] || { echo "launchd scenarios need macOS; skipping"; exit 0; }
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
: "${TS_BIN:?Set TS_BIN}" "${RS_BIN:?Set RS_BIN}"
case "$TS_BIN" in /*) ;; *) TS_BIN="$root/$TS_BIN" ;; esac
case "$RS_BIN" in /*) ;; *) RS_BIN="$root/$RS_BIN" ;; esac

system_tmp="$(cd "${TMPDIR:-/tmp}" && pwd -P)"
work="$(mktemp -d "$system_tmp/ultradian-launchctl.XXXXXX")"
work="$(cd "$work" && pwd -P)"
case "$work" in "$system_tmp"/*) ;; *) echo "refusing: $work" >&2; exit 2 ;; esac

real_plist="$HOME/Library/LaunchAgents/com.ultradian.daemon.plist"
fingerprint() { if [ -e "$real_plist" ]; then ls -l "$real_plist"; shasum "$real_plist"; else echo absent; fi; }
real_before="$(fingerprint)"

started_pids="$work/started-pids"
: >"$started_pids"
cleanup() {
  # Stop only daemons this script's stubs started, by their recorded pids.
  while read -r pid; do
    [ -n "$pid" ] && kill -TERM "$pid" 2>/dev/null || true
  done <"$started_pids"
  sleep 0.5
  rm -rf "$work"
}
trap cleanup EXIT

guard="$work/guard"
mkdir -p "$guard"
cat >"$guard/launchctl" <<'STUB'
#!/bin/sh
uid="$(id -u)"
printf 'launchctl' >>"$STUB_LOG"
for argument in "$@"; do
  printf ' %s' "$(printf '%s' "$argument" | sed "s#gui/$uid#gui/<UID>#; s#$STUB_HOME#<HOME>#")" >>"$STUB_LOG"
done
printf '\n' >>"$STUB_LOG"
case ",$STUB_FAIL," in *",$1,"*) echo "stub: $1 refused" >&2; exit 5 ;; esac
stop_daemon() { "$STUB_DAEMON" daemon stop --json >/dev/null 2>&1 || true; }
start_daemon() {
  "$STUB_DAEMON" daemon run >>"$ULTRADIAN_HOME/daemon.out.log" 2>&1 </dev/null &
  echo $! >>"$STUB_PIDS"
}
case "$1" in
  bootstrap) start_daemon ;;
  bootout) stop_daemon ;;
  kickstart) stop_daemon; start_daemon ;;
esac
exit 0
STUB
for tool in systemctl loginctl; do
  printf '#!/bin/sh\necho "%s $*" >>"$STUB_LOG"\nexit 97\n' "$tool" >"$guard/$tool"
done
chmod +x "$guard"/*

# scenario <name> <fail-list> <commands...>: one fresh home per build.
scenario() {
  local name="$1" fail="$2"
  shift 2
  for build in ts rs; do
    local bin="$TS_BIN"
    [ "$build" = rs ] && bin="$RS_BIN"
    local home="$work/$name-$build"
    mkdir -p "$home/user" "$home/work"
    local out="$work/$name-$build.txt"
    : >"$out"
    local resolved
    resolved="$(PATH="$guard:/usr/bin:/bin" command -v launchctl)"
    [ "$resolved" = "$guard/launchctl" ] || { echo "refusing: launchctl resolves to $resolved" >&2; exit 2; }
    for command in "$@"; do
      : >"$home/calls"
      local code=0
      # shellcheck disable=SC2086
      (cd "$home/work" && env -i HOME="$home/user" ULTRADIAN_HOME="$home/state" \
        PATH="$guard:/usr/bin:/bin" TZ=UTC STUB_LOG="$home/calls" STUB_FAIL="$fail" \
        STUB_DAEMON="$bin" STUB_PIDS="$started_pids" STUB_HOME="$home/user" \
        "$bin" $command </dev/null >"$home/stdout" 2>"$home/stderr") || code=$?
      {
        echo "\$ $command -> exit $code $(grep -o '"code": "[^"]*"' "$home/stderr" | head -1)"
        grep -o '"details": "[^"]*"' "$home/stderr" || true
        sed 's/^/  /' "$home/calls"
      } >>"$out"
    done
    local plist="$home/user/Library/LaunchAgents/com.ultradian.daemon.plist"
    if [ -e "$plist" ]; then
      echo "plist left behind:" >>"$out"
      sed "s#$bin#<BIN>#g; s#$home#<HOME>#g" "$plist" >>"$out"
    fi
    # Whatever is still running in this home stops now.
    (env -i HOME="$home/user" ULTRADIAN_HOME="$home/state" PATH="$guard:/usr/bin:/bin" \
      "$bin" daemon stop --json >/dev/null 2>&1) || true
  done
  if diff -u "$work/$name-ts.txt" "$work/$name-rs.txt"; then
    echo "same  $name"
    sed 's/^/      /' "$work/$name-rs.txt"
  else
    echo "DIFF  $name"
    failures=$((failures + 1))
  fi
}

failures=0
# install gets a fixed --path, so the login shell's PATH stays out of it.
run_all() {
  scenario lifecycle "" \
    "daemon install --path /usr/bin:/bin --json" "daemon install --path /usr/bin:/bin --json" "daemon restart --json" \
    "daemon uninstall --json" "daemon restart --json" "daemon stop --json"
  scenario kickstart-refused kickstart \
    "daemon install --path /usr/bin:/bin --json" "daemon restart --json" "daemon uninstall --json"
  scenario bootstrap-refused bootstrap "daemon install --path /usr/bin:/bin --json" "daemon uninstall --json"
  # Leaves the plist in place so its bytes are compared too.
  scenario installed "" "daemon install --path /usr/bin:/bin --json"
}
run_all

real_after="$(fingerprint)"
if [ "$real_before" != "$real_after" ]; then
  echo "FAIL: the real LaunchAgents plist changed" >&2
  failures=$((failures + 1))
fi
[ "$failures" -eq 0 ] && echo "ok    launchctl call sequences match" || { echo "FAIL  $failures scenario(s)"; exit 1; }
