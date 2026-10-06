# os: darwin
# The launchd plist `daemon install` would write, without writing it. A
# fixed --path keeps the login shell's PATH out of the recording.
step install-dry-run full -- daemon install --dry-run --path "/opt/tools/bin:/usr/bin:/bin" --json
step install-dry-run-human exit -- daemon install --dry-run --path "/usr/bin:/bin"
