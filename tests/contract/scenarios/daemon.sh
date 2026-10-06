# A daemon started with `daemon restart` in a throwaway home (no service
# file there, so it starts directly and never reaches a service manager),
# firing real gates and actions the way Arbor's automations do.
mkdir -p "$WORK/a"
printf 'echo "gate context"\n' >"$WORK/a/gate-open.sh"
printf 'exit 0\n' >"$WORK/a/gate-clean.sh"
printf 'echo nope >&2\nexit 3\n' >"$WORK/a/gate-fail.sh"
printf 'echo "run $ULTRADIAN_SCHEDULE"\ncat\n' >"$WORK/a/run.sh"
printf 'echo failing\nexit 4\n' >"$WORK/a/fail.sh"
printf 'sleep 30\n' >"$WORK/a/slow.sh"

step status-before full -- status --json
step add-open full -- add d-open --every 1h --gate "sh \"$WORK/a/gate-open.sh\"" --cwd "$WORK/a" --yes --json -- /bin/sh "$WORK/a/run.sh"
step add-clean full -- add d-clean --every 1h --gate "sh \"$WORK/a/gate-clean.sh\"" --cwd "$WORK/a" --yes --json -- /bin/sh "$WORK/a/run.sh"
step add-exit-mode full -- add d-exit --every 1h --gate "sh \"$WORK/a/gate-clean.sh\"" --gate-mode exit --cwd "$WORK/a" --yes --json -- /bin/sh "$WORK/a/run.sh"
step add-gate-fail full -- add d-gate-fail --every 1h --gate "sh \"$WORK/a/gate-fail.sh\"" --gate-mode exit --cwd "$WORK/a" --yes --json -- /bin/sh "$WORK/a/run.sh"
step add-fail full -- add d-fail --every 1h --cwd "$WORK/a" --yes --json -- /bin/sh "$WORK/a/fail.sh"
step add-slow full -- add d-slow --every 1h --cwd "$WORK/a" --yes --json -- /bin/sh "$WORK/a/slow.sh"
step add-timeout full -- add d-timeout --every 1h --timeout 1s --cwd "$WORK/a" --yes --json -- /bin/sh "$WORK/a/slow.sh"
step daemon-restart full -- daemon restart --json
step status-live full -- status --json
for schedule in d-open d-clean d-exit d-gate-fail d-fail; do
  step "run-detach-$schedule" full -- run "$schedule" --detach --json
  wait_idle
done
step run-detach-timeout full -- run d-timeout --detach --json
wait_idle
step run-detach-slow full -- run d-slow --detach --json
slow_run="$(last_field run_id)"
wait_status running
step cancel-running full -- cancel "$slow_run" --json
wait_idle
step runs full -- runs --limit 500 --json
step list full -- list --json
step logs-open full -- logs d-open --json
step daemon-stop full -- daemon stop --json
step status-stopped full -- status --json
step daemon-stop-again full -- daemon stop --json
