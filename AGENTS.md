# Ultradian

Ultradian is a CLI for gated schedules and workflows that invoke AI. It is one Rust binary; the crate lives at the repository root with its toolchain pinned in `rust-toolchain.toml`. Read `PRODUCT.md`, `DESIGN.md`, and `docs/architecture.md` before changing the command surface, the store, the runner, the daemon, or the presentation contract.

## How we work

These principles govern how changes are judged in this repository, in priority order.

### 1. Tests earn their place

No earned signal, no test. A test exists to prove behavior that can regress independently of the edit that introduced it: a contract, an invariant, a boundary, or a workflow. It does not exist to mirror a diff, restate a literal, or make a change look safer.

- An existing test is evidence of a past decision and holds no authority over the next one. Before preserving behavior because a test asserts it, decide whether the test protects a real contract. Deleting a test that no longer earns its signal is a normal, expected change.
- Do not add tests because code was extracted, because a static value changed, or to assert that a mock was called with exactly what the test passed in.
- Put each test at its cheapest honest layer: pure logic in unit tests, wiring and schemas in integration tests through the real harness, user journeys end to end.

### 2. Build toward the right model

The existing implementation is raw material and never a constraint. In agent-built code especially, the current shape is often scaffolded history with no intent behind it. Decide the right model first, then make the code conform.

- Prefer the change that deletes complexity over the one that rearranges it.
- Route ambition through the seams that already exist: deepen the owning module instead of inventing a parallel helper, wrapper, flag, or mode beside it.
- Ambition is not expansion. No speculative robustness, no compatibility nobody depends on, no abstraction with one caller, no exports created solely to be unit tested.
- Cleanup is part of delivery: replacing a model means removing its obsolete fields, tests, docs, and affordances in the same change.

### 3. Evidence over confidence

The compiler, clippy, and unit tests prove code correctness only; feature correctness needs the real surface. When a change touches runtime behavior (a command, prompt, output mode, build script, the gate contract, or the daemon), exercise it before calling the work done: run the actual command against a scratch `ULTRADIAN_HOME`, drive the actual prompt in a TTY, let the actual daemon fire a schedule, and check the actual exit code and stderr. Report what was exercised and what was not.

## Hard rules

- Every command is described once, in `src/catalog.json`. The parser tree, help, `schema`, `describe`, and completions derive from it, and a test fails when the parser and the catalog disagree. Update `docs/commands.md` in the same change.
- Commands return a `Done` (data plus human text) or an `AppError`. Only `src/cli/mod.rs` writes to stdout or stderr.
- Structured data goes to stdout. Diagnostics and progress go to stderr.
- JSON, JSONL, CI, piped, and non-interactive execution never prompt or animate.
- Color is semantic, never the only signal, and must honor `NO_COLOR`, `TERM=dumb`, and `--color`.
- The JSON envelopes, record shapes, error codes, and exit codes are the public contract, versioned by `schemaVersion`. Scripts parse the text, so key order, spacing, and number formatting are part of it.
- The store is the only channel between the CLI and the daemon. Do not add sockets, RPC, or a second state file beside it. The schema stays at its current `user_version` (2) unless a migration is added on purpose.
- Do not add backwards-compatibility shims or old/new dual code paths. Two are kept on purpose: the upgrade of 0.1 databases (`src/store/legacy.rs`) and the longer wait for a 0.2.x daemon that lingers after releasing its lock (`src/daemon/control.rs`).
- Never touch the real `~/.ultradian`, a running daemon, or `~/Library/LaunchAgents`. `daemon install`, `restart`, and `uninstall` address the real user's launchd domain whatever HOME says, so anything that runs the binary uses a throwaway HOME and ULTRADIAN_HOME with `launchctl`, `systemctl`, and `loginctl` stubbed first on PATH, and signals only processes it started.
- Secrets never reach run records, docs, or diagnostics. Captured run logs live outside the repo in `ULTRADIAN_HOME`.
- Keep the dependency set small. A new crate needs a concrete cross-cutting benefit.
- Keep committed examples, docs, fixtures, and history public-safe. Never add real credentials, hostnames, customer data, internal URLs, or private project context.

## Tests

- Logic is tested in `#[cfg(test)]` modules beside the code, or a `tests.rs` beside a module. The installed command line is tested once, through the built binary, in `tests/cli.rs`. Service-manager calls are checked by `tests/service/calls.sh` against `launchd.txt` and `systemd.txt`.
- Each contract has one owner. Do not replay a command-line behavior in unit tests, or a store behavior through the binary, unless the other layer has a failure the owner cannot reach.
- Expected values come from a recording or a hand check, never from the code under test. Do not pin the crate version or another value that changes on every release; read it from `CARGO_PKG_VERSION`.
- No test-only production seams: no injected timing, I/O traits, or flags that only tests set. Test through the real boundary instead.
- No process-wide state in tests: never `set_var`, never a shared temp folder. Use `TempStore` from `src/store/tests.rs` for a store, or set the environment on a child process.

## Quality gate

Run:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build && BIN=target/debug/ultradian tests/service/calls.sh
```

CI runs the same on Ubuntu and macOS. The service-call check runs the launchd scenarios on macOS and the systemd ones on Linux.

For prompt or presentation changes, also exercise `cargo run -- add <name> --dry-run -- echo ok` in a real TTY with a scratch `ULTRADIAN_HOME`, and verify `NO_COLOR=1 cargo run -- list` renders without color.
