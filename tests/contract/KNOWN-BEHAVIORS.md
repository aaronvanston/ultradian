# 0.2.1 behaviors to revisit after 0.3.0

0.3.0 keeps 0.2.1's behavior wherever a consumer could tell the difference, including in places where 0.2.1 is arguably wrong. This file lists those places so they can be fixed on purpose after 0.3.0 ships, each as its own change with its own release note. It also lists the few places where 0.3.0 already does the correct thing instead.

## Kept as 0.2.1 did them

1. **The gate's output reaches the action as decoded text.** The runner decodes the gate's stdout as UTF-8 (invalid bytes become U+FFFD, a leading byte-order mark is dropped) and re-encodes it for the action's stdin, so binary gate output is not passed through byte for byte.
2. **"bytes of context" counts UTF-16 units.** The run log's `# gate open (N bytes of context)` reports the JavaScript string length of the decoded output, not bytes.
3. **Each gate and action gets its own session.** Bun's `detached` started children with `setsid`, which also detaches them from any controlling terminal; 0.3.0 does the same, not just a new process group.
4. **A process that leaves its group can stall a run.** Only the run's process group is stopped, so a grandchild that starts its own session and keeps the output pipes open keeps the run waiting.
5. **The 10 MB log cap counts stdout and stderr together.** Where the cut falls between the two streams depends on timing.
6. **Croner's quirks.** Next fires follow croner 10.0.1 exactly, including a fire asked for from inside a repeated DST hour that lands before the instant asked about, `5#` reading as `5`, and error messages with off-by-one numbers (`Invalid value for day: 31` for 32).
7. **`daemon install`, `restart` and `uninstall` address `gui/<uid>/com.ultradian.daemon` whatever HOME says.** Only the plist path follows HOME. Tests must stub `launchctl` (see `tests/service/launchctl-calls.sh`).
8. **Restart after a refused `kickstart` bootstraps a second copy while the first is still loaded.** Under a real launchd that bootstrap fails; with the stub it waits out the 20 s heartbeat wait. Both builds do the same.

## Changed in 0.3.0

1. **`daemon stop` with a run in flight.** 0.2.1's daemon finished shutting down at once but its process stayed alive for 15 s on a leftover timer, so `daemon stop` (and the stop inside `install`) reported `daemon_stop_timeout`, exit 75. 0.3.0 exits when it is done. `overrides/daemon/22-daemon-stop.txt` records the new answer.
2. **Pids at or below zero are never alive.** To `kill(2)` they name process groups, so a bad lock row in 0.2.1 could make `daemon stop` signal its own group. 0.3.0 treats them as dead and never signals pid 0 or 1.
3. **`--catch-up` takes `0s`, `0m`, `0h` and `0d` as zero** (owner decision), and `version --json` reports `runtime: "rust"` in place of `bun`. `doctor`'s "Bun runtime" check is now "Runtime", naming the Rust build.
4. **Stopping a daemon that has already released its lock waits up to 30 s** for its pid to exit, instead of 15 s. A 0.2.1 daemon that was running a fire lingers about 15 s after shutting down, so without this an in-place upgrade from 0.2.1 (`daemon install`, then `daemon restart`) could fail with `daemon_stop_timeout`. `tests/service/upgrade.sh` runs that upgrade against a real 0.2.1 daemon.

## Not recorded here

The systemd unit and the `systemctl` call sequence are Linux-only, so the golden files and `launchctl-calls.sh` cover only macOS. The unit renderer is unit-tested against 0.2.1's escaping rules, but a run on Linux should record its own goldens before 0.3.0 ships.
