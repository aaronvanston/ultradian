# Arbor sends --catch-up 0m for automations with no grace period
# (grace_minutes = 0). 0.2.1 took only a bare 0 and refused the rest with
# invalid_duration; 0.3.0 takes zero in any unit (overrides/ holds those
# answers). Kept in its own scenario so the change doesn't shift ids in
# the others.
mkdir -p "$WORK/automation"
step bare-zero full -- add z0 --every 1h --timeout 6h --catch-up 0 --group arbor --cwd "$WORK/automation" --yes --json -- /bin/sh "$WORK/automation/run.sh"
step zero-minutes full -- add z1 --every 1h --timeout 6h --catch-up 0m --group arbor --cwd "$WORK/automation" --yes --json -- /bin/sh "$WORK/automation/run.sh"
step zero-seconds full -- add z2 --every 1h --catch-up 0s --yes --json -- echo hi
step zero-hours full -- add z3 --every 1h --catch-up 0h --yes --json -- echo hi
step zero-days full -- add z4 --every 1h --catch-up 0d --yes --json -- echo hi
step set-zero-minutes full -- set z0 --catch-up 0m --json
step every-zero-still-refused full -- add z5 --every 0m --yes --json -- echo hi
step timeout-zero-still-refused full -- add z6 --every 1h --timeout 0s --yes --json -- echo hi
step older-than-zero-still-refused full -- prune --older-than 0d --yes --json
