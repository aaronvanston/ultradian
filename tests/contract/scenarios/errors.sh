# Domain errors: the envelope on stderr and the exit code.
mkdir -p "$WORK/d"
step add-base full -- add base --every 5m --yes --json -- echo hi
step add-duplicate full -- add base --every 5m --yes --json -- echo hi
step add-unconfirmed full -- add other --every 5m --json -- echo hi
step add-bad-name full -- add "bad name" --every 5m --yes --json -- echo hi
step add-bad-name-dot full -- add .hidden --every 5m --yes --json -- echo hi
step add-bad-group full -- add g1 --every 5m --group "a b" --yes --json -- echo hi
step add-bad-cron full -- add c1 --cron "not a cron" --yes --json -- echo hi
step add-bad-cron-range full -- add c2 --cron "61 * * * *" --yes --json -- echo hi
step add-bad-tz full -- add c3 --cron "0 9 * * *" --tz Mars/Olympus --yes --json -- echo hi
step add-tz-without-cron full -- add c4 --every 5m --tz UTC --yes --json -- echo hi
step add-cron-and-every full -- add c5 --cron "0 9 * * *" --every 5m --yes --json -- echo hi
step add-duration-no-unit full -- add d1 --every 10 --yes --json -- echo hi
step add-duration-bad-unit full -- add d2 --every 5x --yes --json -- echo hi
step add-duration-zero full -- add d3 --every 0m --yes --json -- echo hi
step add-timeout-zero full -- add d4 --every 5m --timeout 0s --yes --json -- echo hi
step add-catch-up-bad full -- add d5 --every 5m --catch-up soon --yes --json -- echo hi
step add-catch-up-0s full -- add d6 --every 5m --catch-up 0s --yes --json -- echo hi
step add-catch-up-0h full -- add d7 --every 5m --catch-up 0h --yes --json -- echo hi
step add-missing-cwd full -- add d8 --every 5m --cwd "$WORK/nope" --yes --json -- echo hi
step add-cwd-relative full -- add d9 --every 5m --cwd d --yes --json -- echo hi
step add-dry-run full -- add plan --cron "0 2 * * *" --tz UTC --dry-run --json -- ./backup.sh
step add-manual full -- add hand --yes --json -- echo hi
step add-tz-lowercase full -- add lower --cron "0 9 * * *" --tz australia/sydney --yes --json -- echo hi
step add-tz-utc-alias full -- add etc --cron "0 9 * * *" --tz Etc/UTC --yes --json -- echo hi
step add-six-field full -- add secs --cron "*/30 * * * * *" --tz UTC --yes --json -- echo hi
step set-missing full -- set nobody --every 5m --json
step set-nothing full -- set base --json
step set-manual-and-every full -- set base --manual --every 5m --json
step set-tz-on-every full -- set base --tz UTC --json
step pause-missing full -- pause nobody --json
step pause-group-missing full -- pause --group nobody --json
step pause-both full -- pause base --group arbor --json
step pause-neither full -- pause --json
step run-missing full -- run nobody --detach --json
step cancel-missing full -- cancel run_nope --json
step runs-bad-cursor full -- runs --since abc --json
step runs-negative-cursor full -- runs --since -1 --json
step runs-limit-zero full -- runs --limit 0 --json
step runs-limit-word full -- runs --limit many --json
step runs-limit-fraction full -- runs --limit 1.5 --json
step logs-missing-run full -- logs --run run_nope --json
step human-error exit -- rm nobody --yes
step human-compact-error full -- rm nobody --yes --json --compact
step jsonl-error full -- rm nobody --yes --jsonl
step completion-bad-shell full -- completion tcsh --json
step describe-missing full -- describe nothing here --json
