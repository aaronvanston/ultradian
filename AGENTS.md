# Ultradian

Ultradian is a Bun and TypeScript CLI for gated schedules and workflows that invoke AI. Read `PRODUCT.md`, `DESIGN.md`, `docs/architecture.md`, and `docs/extending.md` before changing the engine, the schedules module, or the presentation contract.

## Rust rewrite in progress

Ultradian 0.3.0 is a Rust rewrite with the same contract. The Rust crate lives at the repository root (`Cargo.toml`, `src/`); the 0.2.x TypeScript code, with its scripts and tooling, lives in `legacy/` until 0.3.0 ships, so the two can be compared. Run the TypeScript gate from `legacy/` (`cd legacy && bun run ci`); paths such as `src/engine/` below mean `legacy/src/engine/` until the Rust modules replace them. The frozen 0.2.1 contract is in `tests/contract/` (golden outputs, replayed with `BIN=<binary> tests/contract/run.sh`), `src/catalog.json` (the `schema --json` catalog) and `tests/fixtures/` (cron results).

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

Typecheck, lint, and unit tests prove code correctness only; feature correctness needs the real surface. When a change touches runtime behavior (a command, prompt, output mode, build script, the gate contract, or the daemon), exercise it before calling the work done: run the actual command against a scratch `ULTRADIAN_HOME`, drive the actual prompt in a TTY, let the actual daemon fire a schedule, and check the actual exit code and stderr. Report what was exercised and what was not.

## Hard rules

- A command is declared once with `defineCommand`; registration, help, schema, docs, and completions derive from that descriptor.
- Commands return typed outcomes. Only the engine writes to stdout or stderr.
- Structured data goes to stdout. Diagnostics and progress go to stderr.
- JSON, JSONL, CI, piped, and non-interactive execution never prompt or animate.
- Color is semantic, never the only signal, and must honor `NO_COLOR`, `TERM=dumb`, and `--color`.
- Domain modules depend on service tokens and never on concrete infrastructure.
- Tests are colocated: a `__tests__/` directory beside the code it proves, running on `bun:test`. No separate top-level test tree, no other runner.
- This project targets current Bun on macOS and Linux. Do not add backwards-compatibility shims, deprecation cycles, legacy runtime support, or old/new dual code paths; change the model and move forward.
- Secrets never reach run records, generated docs, or diagnostics. Captured run logs live outside the repo in `ULTRADIAN_HOME`.
- The store is the only channel between the CLI and the daemon. Do not add sockets, RPC, or a second state file beside it.
- Keep the runtime dependency set small. A new dependency needs a concrete cross-cutting benefit.
- Keep committed examples, docs, fixtures, screenshots, and history public-safe. Never add real credentials, customer data, internal URLs, or private project context.
- Ultracite is the quality-policy source. Oxlint and Oxfmt are the underlying Oxc tools; keep their committed configs aligned with the Ultracite presets.

## Quality gate

Run:

```bash
bun run ci
```

Use `bun run check` for the standalone Ultracite check and `bun run fix` for safe automatic rewrites. Review formatter or linter changes before committing.

Generated command documentation must be committed.

For prompt or presentation changes, also exercise `bun run dev -- add <name> --dry-run -- echo ok` in a real TTY and verify `NO_COLOR=1 bun run dev -- list` renders without color.
