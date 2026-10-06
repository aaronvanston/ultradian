#!/usr/bin/env bash
# The systemd side of tests/service/launchctl-calls.sh: records every
# systemctl and loginctl call `daemon install|restart|uninstall` and
# `doctor` make, in order, for two builds, and diffs them. With
# OUT=<folder> it also writes each build's transcript there, which is how
# the Linux goldens in tests/service/golden/ were recorded from 0.2.1.
#
#   TS_BIN=legacy/dist/ultradian RS_BIN=target/debug/ultradian tests/service/systemctl-calls.sh
#
# Linux only. Each scenario runs in a throwaway HOME and ULTRADIAN_HOME,
# with stubs first on PATH: systemctl plays the user manager by starting
# and stopping the build's own daemon in that home, loginctl answers the
# linger question from STUB_LINGER, and launchctl fails outright. Every
# daemon a stub starts is stopped by pid at the end. Nothing here reaches
# the real systemd or the real ~/.config/systemd.
set -euo pipefail

[ "$(uname -s)" = Linux ] || { echo "systemd scenarios need Linux; skipping"; exit 0; }
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
: "${TS_BIN:?Set TS_BIN}" "${RS_BIN:?Set RS_BIN}"
case "$TS_BIN" in /*) ;; *) TS_BIN="$root/$TS_BIN" ;; esac
case "$RS_BIN" in /*) ;; *) RS_BIN="$root/$RS_BIN" ;; esac

system_tmp="$(cd "${TMPDIR:-/tmp}" && pwd -P)"
work="$(mktemp -d "$system_tmp/ultradian-systemctl.XXXXXX")"
work="$(cd "$work" && pwd -P)"
case "$work" in "$system_tmp"/*) ;; *) echo "refusing: $work" >&2; exit 2 ;; esac
real_unit="$HOME/.config/systemd/user/ultradian.service"
fingerprint() { if [ -e "$real_unit" ]; then ls -l "$real_unit"; sha256sum "$real_unit"; else echo absent; fi; }
real_before="$(fingerprint)"

started_pids="$work/started-pids"
: >"$started_pids"
cleanup() {
  while read -r pid; do [ -n "$pid" ] && kill -TERM "$pid" 2>/dev/null || true; done <"$started_pids"
  sleep 0.5
  rm -rf "$work"
}
trap cleanup EXIT

guard="$work/guard"
mkdir -p "$guard"
cp "$root/tests/service/systemctl-stub" "$guard/systemctl"
cp "$root/tests/service/loginctl-stub" "$guard/loginctl"
printf '#!/bin/sh\necho "launchctl $*" >>"$STUB_LOG"\nexit 97\n' >"$guard/launchctl"
chmod +x "$guard"/*
for tool in systemctl loginctl; do
  resolved="$(PATH="$guard:/usr/bin:/bin" command -v "$tool")"
  [ "$resolved" = "$guard/$tool" ] || { echo "refusing: $tool resolves to $resolved" >&2; exit 2; }
done

failures=0
# scenario <name> <fail-list> <linger> <commands...>: one fresh home per build.
scenario() {
  local name="$1" fail="$2" linger="$3"
  shift 3
  for build in ts rs; do
    local bin="$TS_BIN"
    [ "$build" = rs ] && bin="$RS_BIN"
    local home="$work/$name-$build"
    mkdir -p "$home/user" "$home/work"
    local out="$work/$name-$build.txt"
    : >"$out"
    for command in "$@"; do
      : >"$home/calls"
      local code=0
      # shellcheck disable=SC2086
      (cd "$home/work" && env -i HOME="$home/user" ULTRADIAN_HOME="$home/state" \
        PATH="$guard:/usr/bin:/bin" TZ=UTC STUB_LOG="$home/calls" STUB_FAIL="$fail" \
        STUB_LINGER="$linger" STUB_DAEMON="$bin" STUB_PIDS="$started_pids" STUB_HOME="$home/user" \
        "$bin" $command </dev/null >"$home/stdout" 2>"$home/stderr") || code=$?
      {
        echo "\$ $command -> exit $code $(grep -o '"code": "[^"]*"' "$home/stderr" | head -1)"
        grep -o '"details": "[^"]*"' "$home/stderr" || true
        if [ "${command%% *}" = doctor ]; then
          grep -E '"(name|status|detail|fix)"' "$home/stdout" | sed "s#$home#<HOME>#g; s/Bun [0-9.]*/<RUNTIME>/; s/Rust build of ultradian [0-9.]*/<RUNTIME>/; s/\"Bun runtime\"/\"Runtime\"/" || true
        fi
        sed 's/^/  /' "$home/calls"
      } >>"$out"
    done
    local unit="$home/user/.config/systemd/user/ultradian.service"
    if [ -e "$unit" ]; then
      echo "unit left behind:" >>"$out"
      sed "s#$bin#<BIN>#g; s#$home#<HOME>#g" "$unit" >>"$out"
    fi
    (env -i HOME="$home/user" ULTRADIAN_HOME="$home/state" PATH="$guard:/usr/bin:/bin" \
      STUB_LOG=/dev/null "$bin" daemon stop --json >/dev/null 2>&1) || true
    if [ -n "${OUT:-}" ]; then
      mkdir -p "$OUT"
      { echo "== $name"; cat "$out"; } >>"$OUT/systemd-calls-$build.txt"
    fi
  done
  if diff -u "$work/$name-ts.txt" "$work/$name-rs.txt"; then
    echo "same  $name"
    sed 's/^/      /' "$work/$name-rs.txt"
  else
    echo "DIFF  $name"
    failures=$((failures + 1))
  fi
}

install="daemon install --path /usr/bin:/bin --json"
scenario lifecycle "" yes \
  "$install" "$install" "daemon restart --json" "daemon uninstall --json" \
  "daemon restart --json" "daemon stop --json"
scenario restart-refused restart yes "$install" "daemon restart --json" "daemon uninstall --json"
scenario enable-refused enable yes "$install" "daemon uninstall --json"
scenario reload-refused daemon-reload yes "$install" "daemon uninstall --json"
# Leaves the unit in place so its bytes are compared, and asks the doctor
# about lingering each way.
scenario linger-on "" yes "$install" "doctor --json"
scenario linger-off "" no "$install" "doctor --json"
scenario linger-unknown "" fail "$install" "doctor --json"
scenario not-installed "" yes "doctor --json" "daemon uninstall --json"

if [ "$real_before" != "$(fingerprint)" ]; then
  echo "FAIL: the real systemd unit changed" >&2
  failures=$((failures + 1))
fi
if [ -f "$root/tests/service/golden/systemd-calls.txt" ] && [ -n "${OUT:-}" ]; then
  diff -u "$root/tests/service/golden/systemd-calls.txt" "$OUT/systemd-calls-rs.txt" \
    && echo "same  as the recorded 0.2.1 transcript" || failures=$((failures + 1))
fi
[ "$failures" -eq 0 ] && echo "ok    systemctl call sequences match" || { echo "FAIL  $failures check(s)"; exit 1; }
