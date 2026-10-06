# How Commander reads a command line that parses: options anywhere before
# --, --opt=value, --no-* flags and the variadic command after --. Global
# options are taken out first wherever they sit, so '--gate --json' gives
# --gate the next word instead.
step setup full -- add argv --every 5m --yes --json -- echo hi
step setup-short full -- add short --every 5m --yes --json -- echo hi
step global-before-command full -- --json list
step equals-form full -- add eq --every=5m --cwd=. --gate-mode=exit --timeout=1h --yes --json -- echo hi
step options-after-name full -- add after --yes --json --every 10m -- echo hi
step options-before-name full -- add --every 10m --yes --json before -- echo hi
step flags-after-dashdash full -- add argv2 --every 5m --yes --json -- echo --json --every -y -- tail
step value-looks-like-flag full -- add flagval --every 5m --gate --json --yes -- echo hi
step value-takes-next-flag full -- add flagval --every 5m --gate --dry-run --yes --json -- echo hi
step dashdash-without-unknown-option full -- --json add dd --yes -- echo --dry-run
step preflight-json-after-dashdash exit -- add pf --every 5m --yes -- echo --json
step preflight-json-error-after-dashdash full -- add argv --every 5m --yes -- echo --json
step command-without-dashdash full -- add nodash --every 5m --yes --json echo hi
step yes-short full -- add short2 --every 5m -y --json -- echo hi
step repeated-option full -- add twice --every 5m --every 10m --yes --json -- echo hi
step set-command full -- set argv --json -- printf "%s\n" one two
step set-no-color full -- set argv --no-color --every 1h --json
step quiet full -- set argv --every 2h --json -q
step non-interactive full -- rm short --non-interactive --json
step no-input full -- rm short --no-input --json
step compact-success full -- rm short --yes --json --compact
step jsonl-success full -- list --jsonl
step list-after full -- list --json
