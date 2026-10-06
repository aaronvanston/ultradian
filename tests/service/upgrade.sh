#!/usr/bin/env bash
# Arbor's in-place upgrade, with a 0.2.1 daemon still running a fire:
# install the new binary, then `daemon install --json` and
# `daemon restart --json` under `set -e`, as Arbor's install script does.
#
#   TS_BIN=legacy/dist/ultradian RS_BIN=target/debug/ultradian tests/service/upgrade.sh
#
# 0.2.1's daemon finishes shutting down at once but its process lingers
# about 15 s on a leftover timer when a run was in flight, so the new
# build's stop must wait it out rather than fail with daemon_stop_timeout.
# macOS only (launchd). Everything runs in a throwaway HOME and
# ULTRADIAN_HOME with the recording launchctl stub first on PATH; every
# daemon started here is stopped by pid; the real LaunchAgents plist is
# checked untouched.
set -euo pipefail

[ "$(uname -s)" = Darwin ] || { echo "launchd upgrade needs macOS; skipping"; exit 0; }
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
: "${TS_BIN:?Set TS_BIN to the 0.2.1 build}" "${RS_BIN:?Set RS_BIN to the Rust build}"
case "$TS_BIN" in /*) ;; *) TS_BIN="$root/$TS_BIN" ;; esac
case "$RS_BIN" in /*) ;; *) RS_BIN="$root/$RS_BIN" ;; esac

system_tmp="$(cd "${TMPDIR:-/tmp}" && pwd -P)"
work="$(mktemp -d "$system_tmp/ultradian-upgrade.XXXXXX")"
work="$(cd "$work" && pwd -P)"
case "$work" in "$system_tmp"/*) ;; *) echo "refusing: $work" >&2; exit 2 ;; esac
real_plist="$HOME/Library/LaunchAgents/com.ultradian.daemon.plist"
fingerprint() { if [ -e "$real_plist" ]; then ls -l "$real_plist"; shasum "$real_plist"; else echo absent; fi; }
real_before="$(fingerprint)"

started="$work/started-pids"
: >"$started"
cleanup() {
  while read -r pid; do [ -n "$pid" ] && kill -TERM "$pid" 2>/dev/null || true; done <"$started"
  sleep 0.5
  rm -rf "$work"
}
trap cleanup EXIT

guard="$work/guard"
mkdir -p "$guard" "$work/user" "$work/work" "$work/state/bin"
cp "$root/tests/service/launchctl-stub" "$guard/launchctl"
for tool in systemctl loginctl; do
  printf '#!/bin/sh\necho "%s $*" >>"%s/forbidden.log"\nexit 97\n' "$tool" "$work" >"$guard/$tool"
done
chmod +x "$guard"/*
[ "$(PATH="$guard:/usr/bin:/bin" command -v launchctl)" = "$guard/launchctl" ] || { echo "refusing: stub not first" >&2; exit 2; }

# The installed binary Arbor runs, at the stable path; the stub supervises
# whatever is there now.
udian="$work/state/bin/udian"
cp "$TS_BIN" "$udian"
chmod 755 "$udian"
run() {
  (cd "$work/work" && env -i HOME="$work/user" ULTRADIAN_HOME="$work/state" \
    PATH="$guard:/usr/bin:/bin" TZ=UTC STUB_LOG="$work/calls" STUB_FAIL="" \
    STUB_DAEMON="$udian" STUB_PIDS="$started" STUB_HOME="$work/user" "$udian" "$@" </dev/null)
}

# 0.2.1 installed and running, with a fire in flight.
run daemon install --path /usr/bin:/bin --json >/dev/null
run add slow --yes --json -- /bin/sh -c 'sleep 60' >/dev/null
run_id="$(run run slow --detach --json | sed -n 's/.*"run_id": "\([^"]*\)".*/\1/p')"
for _ in $(seq 1 50); do
  run status --json | grep -q '"status": "running"' && break
  sleep 0.2
done
old_pid="$(run status --json | sed -n 's/.*"pid": \([0-9]*\),.*/\1/p' | tail -n 1)"
echo "0.2.1 daemon pid $old_pid running $run_id"

# The upgrade, exactly as Arbor's script runs it.
cp "$RS_BIN" "$udian.new" && chmod 755 "$udian.new" && mv "$udian.new" "$udian"
: >"$work/calls"
set +e
started_at=$(date +%s)
run daemon install --path /usr/bin:/bin --json >"$work/install.json" 2>"$work/install.err"
install_exit=$?
run daemon restart --json >"$work/restart.json" 2>"$work/restart.err"
restart_exit=$?
set -e
echo "install exit $install_exit, restart exit $restart_exit, $(( $(date +%s) - started_at ))s"
sed 's/^/  /' "$work/calls"

failures=0
[ "$install_exit" -eq 0 ] || { echo "FAIL install:"; cat "$work/install.err"; failures=$((failures + 1)); }
[ "$restart_exit" -eq 0 ] || { echo "FAIL restart:"; cat "$work/restart.err"; failures=$((failures + 1)); }
grep -q '"version": "0.3.0"' "$work/restart.json" || { echo "FAIL: not on the new build"; failures=$((failures + 1)); }
if kill -0 "$old_pid" 2>/dev/null; then echo "FAIL: the 0.2.1 daemon is still running"; failures=$((failures + 1)); fi
status="$(run runs --json | grep -A12 "\"run_id\": \"$run_id\"" | sed -n 's/.*"status": "\([^"]*\)".*/\1/p')"
echo "the in-flight run ended as $status"
[ "$status" = interrupted ] || { echo "FAIL: expected interrupted"; failures=$((failures + 1)); }
run daemon stop --json >/dev/null || true
[ -s "$work/forbidden.log" ] && { echo "FAIL: systemctl or loginctl was called"; failures=$((failures + 1)); }
[ "$real_before" = "$(fingerprint)" ] || { echo "FAIL: the real LaunchAgents plist changed"; failures=$((failures + 1)); }
[ "$failures" -eq 0 ] && echo "ok    upgrade from a lingering 0.2.1 daemon" || exit 1
