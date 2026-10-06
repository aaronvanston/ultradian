# Commands Arbor doesn't call, recorded for the rewrite's own regression net.
mkdir -p "$WORK/m"
step once full -- once --name import --timeout 30m --cwd "$WORK/m" --json -- ./import.sh
step once-default-name full -- once --json -- /usr/bin/true
step once-missing-command full -- once --json
step add full -- add m1 --every 5m --yes --json -- echo hi
step run-foreground full -- run m1 --json
step logs full -- logs --json
step logs-run full -- logs m1 --limit 5 --json
step logs-limit-too-big full -- logs --limit 500 --json
step prune-unconfirmed full -- prune --older-than 1d --json
step prune full -- prune --older-than 1d --yes --json
step prune-missing-duration full -- prune --yes --json
step doctor exit -- doctor --json
step completion-zsh exit -- completion zsh
step list-human exit -- list
step status-human exit -- status
