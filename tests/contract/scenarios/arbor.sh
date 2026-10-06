# What Arbor runs to place, change, trigger and read an automation, with no
# daemon running (src-tauri/src/usage/machine_health/automations/udian.rs).
mkdir -p "$WORK/automation"
printf 'echo context\n' >"$WORK/automation/gate.sh"
printf 'cat\n' >"$WORK/automation/run.sh"
gate="sh \"$WORK/automation/gate.sh\""

step status-fresh full -- status --json
step list-empty full -- list --json
step add-cron-tz full -- add arbor-x --cron "0 9 * * *" --tz Australia/Sydney --timeout 6h --catch-up 30m --gate "$gate" --gate-mode exit --group arbor --cwd "$WORK/automation" --yes --json -- /bin/sh "$WORK/automation/run.sh"
step add-every full -- add arbor-y --every 15m --timeout 6h --catch-up 0 --group arbor --cwd "$WORK/automation" --yes --json -- /bin/sh "$WORK/automation/run.sh"
step add-every-hours full -- add arbor-w --every 2h --timeout 6h --catch-up 90m --group arbor --cwd "$WORK/automation" --yes --json -- /bin/sh "$WORK/automation/run.sh"
# grace_minutes = 0 sends --catch-up 0m: 0.2.1 rejects it, 0.3.0 accepts it.
step add-catch-up-0m full -- add arbor-z --every 1h --timeout 6h --catch-up 0m --group arbor --cwd "$WORK/automation" --yes --json -- /bin/sh "$WORK/automation/run.sh"
step list full -- list --json
step list-alias full -- ls --json
step set-cron full -- set arbor-x --cron "30 8 * * 1-5" --tz America/New_York --timeout 2h --catch-up 1h --cwd "$WORK/automation" --no-gate --json -- /bin/sh "$WORK/automation/run.sh"
step set-every-gate full -- set arbor-y --every 1h --timeout 6h --catch-up 0 --gate "$gate" --gate-mode exit --cwd "$WORK/automation" --json -- /bin/sh "$WORK/automation/run.sh"
step set-tz-alone full -- set arbor-x --tz Europe/Berlin --json
step set-tz-local full -- set arbor-x --tz local --json
step set-no-timeout full -- set arbor-y --no-timeout --json
step set-no-group full -- set arbor-y --no-group --json
step set-group full -- set arbor-y --group loops --json
step set-manual full -- set arbor-w --manual --json
step pause full -- pause arbor-x --json
step pause-again full -- pause arbor-x --json
step resume full -- resume arbor-x --json
step pause-group full -- pause --group arbor --json
step resume-group full -- resume --group arbor --json
step run-detach full -- run arbor-x --detach --json
run_id="$(last_field run_id)"
step run-detach-in-flight full -- run arbor-x --detach --json
step run-detach-other full -- run arbor-y --detach --json
step runs-first-page full -- runs --limit 500 --json
cursor="$(last_field cursor)"
step runs-since-cursor-empty full -- runs --since "$cursor" --limit 500 --json
step cancel full -- cancel "$run_id" --json
step cancel-again full -- cancel "$run_id" --json
step runs-since-cursor-changed full -- runs --since "$cursor" --limit 500 --json
step runs-page-1 full -- runs --since 0 --limit 1 --json
cursor="$(last_field cursor)"
step runs-page-2 full -- runs --since "$cursor" --limit 1 --json
step runs-all full -- runs --json
step runs-jsonl full -- runs --since 0 --limit 500 --jsonl
step runs-compact full -- runs --since 0 --limit 500 --json --compact
step status-queued full -- status --json
step rm full -- rm arbor-y --yes --json
step rm-again full -- rm arbor-y --yes --json
step rm-unconfirmed full -- rm arbor-x --json
step list-after full -- list --json
