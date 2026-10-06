# Arbor's install script probes `version --json` under `set -e` and reads
# data.version, so it must exit 0 and keep that key.
step version-json full -- version --json
step version-json-compact full -- version --json --compact
step version-jsonl full -- version --jsonl
step version-flag full -- --version
step version-short-flag full -- -V
step version-human exit -- version
step root-help exit --
step root-help-flag exit -- --help
step command-help exit -- add --help
step version-unknown-option full -- version --bogus --json
