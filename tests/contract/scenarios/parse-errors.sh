# Command lines Commander refuses before any command runs, plus the flags
# it answers itself (--version, --help). Every refusal exits 2; with --json
# anywhere on the line, even after --, stderr ends with an invalid_usage
# envelope after Commander's own lines. "json" steps compare only that
# envelope from stderr, because Commander prints a group's whole help
# before it.
step unknown-command full -- bogus --json
step unknown-command-human full -- bogus
step unknown-command-suggest-two full -- lst --json
step unknown-command-empty full -- "" --json
step help-is-not-a-command full -- help --json
step unknown-option full -- list --bogus --json
step unknown-option-human full -- list --bogus
step unknown-option-suggest full -- add nogate --every 5m --no-gate --yes --json -- echo hi
step unknown-option-suggest-global full -- list --jsno
step unknown-option-before-command full -- --bogus list --json
step unknown-short-option full -- list -x --json
step unknown-short-option-root full -- -x list --json
step equals-on-flag full -- --json=1 list
step equals-on-unknown full -- list --bogus=1 --json
step missing-option-value full -- runs --json --limit
step missing-option-value-human full -- runs --limit
step missing-argument full -- add --json
step missing-argument-cancel full -- cancel --json
step missing-argument-describe full -- describe --json
step missing-command full -- add x --every 5m --yes --json
step unknown-option-beats-missing-argument full -- add --bogus --json
step bad-choice full -- add x --gate-mode maybe --json -- echo
step bad-choice-equals full -- add x --gate-mode=maybe --json -- echo
step bad-color full -- list --color sometimes --json
step bad-color-equals full -- --color=sometimes list --json
step too-many-arguments full -- rm a b --yes --json
step too-many-arguments-none full -- list extra --json
step too-many-arguments-version full -- version extra more --json
step dashdash-before-command full -- -- version --json
step dashdash-trailing full -- version --json --
step json-after-dashdash full -- version -- --json
step json-and-jsonl full -- --json --jsonl version
step json-and-jsonl-list full -- list --jsonl --json
step group-without-subcommand json -- daemon --json
step group-without-subcommand-human exit -- daemon
step group-unknown-subcommand json -- daemon bogus --json
step group-leaf-unknown-option json -- daemon start --bogus --json
step self-without-subcommand json -- self --json
step version-flag-anywhere full -- list --version --json
step version-flag-in-leaf full -- add x --every 5m -V
step version-flag-combined full -- -qV
step version-flag-after-dashdash full -- version --json -- --version
step help-flag-root exit -- --help --json
step help-flag-leaf exit -- list --help --json
step help-flag-short exit -- add -h
step help-flag-group exit -- daemon --help
step help-flag-beats-errors exit -- add --bogus --help
