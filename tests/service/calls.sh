#!/usr/bin/env bash
# Records every service-manager call `daemon install|restart|uninstall|stop`
# and `doctor` make, in order, and diffs the transcript against the one
# checked in for this platform: launchd.txt on macOS, systemd.txt on Linux.
# These are the calls that touch real machines, so they are compared
# exactly. The transcripts were recorded from 0.2.1.
#
#   BIN=target/debug/ultradian tests/service/calls.sh           # check
#   BIN=target/debug/ultradian tests/service/calls.sh --print   # print it
#
# Each scenario runs in a throwaway HOME and ULTRADIAN_HOME. Stubs come
# first on PATH and never run the real tools: launchctl or systemctl
# records its arguments, fails the subcommands STUB_FAIL names, and plays
# the supervisor by starting or stopping this build's daemon in that
# throwaway home; loginctl answers the linger question from STUB_LINGER;
# the other platform's tool fails outright. Every daemon a scenario starts
# is stopped by pid at the end, and the real service file is checked
# untouched before and after.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
print=0
[ "${1:-}" = --print ] && print=1
: "${BIN:?Set BIN to the ultradian binary}"
case "$BIN" in /*) ;; *) BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")" ;; esac

case "$(uname -s)" in
  Darwin)
    platform=launchd
    real_file="$HOME/Library/LaunchAgents/com.ultradian.daemon.plist"
    service_file="Library/LaunchAgents/com.ultradian.daemon.plist"
    ;;
  Linux)
    platform=systemd
    real_file="$HOME/.config/systemd/user/ultradian.service"
    service_file=".config/systemd/user/ultradian.service"
    ;;
  *) echo "no service manager to check on $(uname -s)" >&2; exit 2 ;;
esac

fingerprint() { if [ -e "$real_file" ]; then ls -l "$real_file"; cksum <"$real_file"; else echo absent; fi; }
real_before="$(fingerprint)"

system_tmp="$(cd "${TMPDIR:-/tmp}" && pwd -P)"
work="$(mktemp -d "$system_tmp/ultradian-service.XXXXXX")"
work="$(cd "$work" && pwd -P)"
case "$work" in "$system_tmp"/*) ;; *) echo "refusing: $work" >&2; exit 2 ;; esac

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
fail_outright() { printf '#!/bin/sh\necho "%s $*" >>"$STUB_LOG"\nexit 97\n' "$1" >"$guard/$1"; }
if [ "$platform" = launchd ]; then
  cp "$here/launchctl-stub" "$guard/launchctl"
  fail_outright systemctl
  fail_outright loginctl
else
  cp "$here/systemctl-stub" "$guard/systemctl"
  cp "$here/loginctl-stub" "$guard/loginctl"
  fail_outright launchctl
fi
chmod +x "$guard"/*
for tool in launchctl systemctl loginctl; do
  resolved="$(PATH="$guard:/usr/bin:/bin" command -v "$tool")"
  [ "$resolved" = "$guard/$tool" ] || { echo "refusing: $tool resolves to $resolved" >&2; exit 2; }
done

transcript="$work/transcript.txt"
: >"$transcript"

# scenario <name> <fail-list> <linger> <commands...>: one fresh home each.
scenario() {
  local name="$1" fail="$2" linger="$3"
  shift 3
  local home="$work/$name"
  mkdir -p "$home/user" "$home/work"
  echo "== $name" >>"$transcript"
  for command in "$@"; do
    : >"$home/calls"
    local code=0
    # shellcheck disable=SC2086
    (cd "$home/work" && env -i HOME="$home/user" ULTRADIAN_HOME="$home/state" \
      PATH="$guard:/usr/bin:/bin" TZ=UTC STUB_LOG="$home/calls" STUB_FAIL="$fail" \
      STUB_LINGER="$linger" STUB_DAEMON="$BIN" STUB_PIDS="$started_pids" STUB_HOME="$home/user" \
      "$BIN" $command </dev/null >"$home/stdout" 2>"$home/stderr") || code=$?
    {
      echo "\$ $command -> exit $code $(grep -o '"code": "[^"]*"' "$home/stderr" | head -1 || true)"
      grep -o '"details": "[^"]*"' "$home/stderr" || true
      if [ "${command%% *}" = doctor ]; then
        grep -o '{[^{}]*"name":"Login service"[^{}]*}' "$home/stdout" || true
      fi
      sed 's/^/  /' "$home/calls"
    } >>"$transcript"
  done
  if [ -e "$home/user/$service_file" ]; then
    echo "left behind:" >>"$transcript"
    sed "s#$BIN#<BIN>#g; s#$home#<HOME>#g" "$home/user/$service_file" >>"$transcript"
  fi
  (env -i HOME="$home/user" ULTRADIAN_HOME="$home/state" PATH="$guard:/usr/bin:/bin" \
    STUB_LOG=/dev/null "$BIN" daemon stop --json >/dev/null 2>&1) || true
}

# install gets a fixed --path, so the login shell's PATH stays out of it.
install="daemon install --path /usr/bin:/bin --json"
doctor="doctor --json --compact"
if [ "$platform" = launchd ]; then
  scenario lifecycle "" "" \
    "$install" "$install" "daemon restart --json" \
    "daemon uninstall --json" "daemon restart --json" "daemon stop --json"
  scenario kickstart-refused kickstart "" "$install" "daemon restart --json" "daemon uninstall --json"
  scenario bootstrap-refused bootstrap "" "$install" "daemon uninstall --json"
  scenario installed "" "" "$install"
else
  scenario lifecycle "" yes \
    "$install" "$install" "daemon restart --json" \
    "daemon uninstall --json" "daemon restart --json" "daemon stop --json"
  scenario restart-refused restart yes "$install" "daemon restart --json" "daemon uninstall --json"
  scenario enable-refused enable yes "$install" "daemon uninstall --json"
  scenario reload-refused daemon-reload yes "$install" "daemon uninstall --json"
  scenario linger-on "" yes "$install" "$doctor"
  scenario linger-off "" no "$install" "$doctor"
  scenario linger-unknown "" fail "$install" "$doctor"
  scenario not-installed "" yes "$doctor" "daemon uninstall --json"
fi

failures=0
if [ "$real_before" != "$(fingerprint)" ]; then
  echo "FAIL: the real service file changed" >&2
  failures=$((failures + 1))
fi
if [ "$print" -eq 1 ]; then
  cat "$transcript"
elif diff -u "$here/$platform.txt" "$transcript"; then
  echo "ok    $platform calls match"
else
  failures=$((failures + 1))
fi
[ "$failures" -eq 0 ] || exit 1
