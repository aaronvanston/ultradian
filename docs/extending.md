# Extending Ultradian

This guide covers the decisions and files involved in adding to the CLI. `src/modules/schedules/` is the worked example: read it alongside this guide, because every pattern described here is already applied there.

## 1. Choose the smallest extension seam

Use a standalone command when the feature has no shared infrastructure. Use a module when several commands form one feature. Add a service when commands need shared state, I/O, storage, or lifecycle cleanup.

Most new work belongs inside `schedules`, because most new work touches schedules, runs, or the daemon. Deepen that module before adding a parallel one. Keep feature names concrete. The engine should not learn product nouns.

## 2. Add a command

Create a descriptor with `defineCommand`:

```ts
import { z } from "zod";
import { defineCommand } from "../../engine/index.ts";

export const widgetsListCommand = defineCommand({
  path: ["widgets", "list"],
  aliases: ["ls"],
  module: "widgets",
  summary: "List widgets",
  description: "Reads widgets from the configured service.",
  options: [
    {
      flags: "--limit <count>",
      description: "Maximum number of widgets",
      defaultValue: 20,
    },
  ],
  examples: [
    "ultradian widgets list",
    "ultradian widgets list --limit 5 --json",
  ],
  optionsSchema: z.object({
    limit: z.coerce.number().int().min(1).max(100).default(20),
  }),
  outputSchema: z.array(
    z.object({
      id: z.string(),
      name: z.string(),
    })
  ),
  async run(context) {
    return { data: [] };
  },
  render(data, context) {
    return context.ui.table(
      ["ID", "Name"],
      data.map((widget) => [widget.id, widget.name])
    );
  },
});
```

The descriptor owns grammar, documentation, validation, execution, and human presentation. Do not add the same command to a second help or docs registry.

### Descriptor checklist

- `path` is the canonical token sequence.
- `aliases` are optional and should remain unambiguous.
- `module` groups commands in root help.
- `summary` is one line; `description` adds context only when needed.
- `arguments` and `options` describe the parser surface.
- `examples` are copy-pasteable and include a machine-mode example.
- `kind` is `write` for mutations and defaults to `read`.
- `optionsSchema` normalizes and validates parser values.
- `outputSchema` stabilises the public result contract.
- `run` performs work without printing.
- `render` formats only the human result.

`src/modules/schedules/commands.ts` shows the full shape, including a union output schema for the dry-run and applied forms of `add`.

## 3. Attach a module

Group commands in `src/modules/widgets/module.ts`:

```ts
import { defineModule } from "../../engine/index.ts";
import { widgetsListCommand } from "./commands.ts";

export const widgetsModule = defineModule({
  id: "widgets",
  summary: "Manage widgets",
  commands: [widgetsListCommand],
});
```

Import it in `src/app.ts` and add it to `modules`. Modules are static imports so compiled executables do not depend on runtime file discovery. A module can also contribute `services` and `healthChecks`; `schedules` contributes both, and its health checks are what `doctor` reports about the data directory and the daemon.

## 4. Add a typed service

Define a token and provider close to the infrastructure it owns:

```ts
import {
  createServiceToken,
  type ServiceProvider,
} from "../../engine/index.ts";

type WidgetsService = {
  list(limit: number): Promise<readonly Widget[]>;
};

export const widgetsService =
  createServiceToken<WidgetsService>("widgets-service");

export const widgetsServiceProvider: ServiceProvider<WidgetsService> = {
  token: widgetsService,
  create() {
    return new RemoteWidgetsService();
  },
  async dispose(service) {
    await service.close?.();
  },
};
```

Add the provider to its module and resolve it only inside commands that need it:

```ts
const service = await context.services.get(widgetsService);
```

Providers are lazy, cached for one invocation, cycle-checked, and disposed in reverse construction order. `scheduleStoreProvider` in `src/modules/schedules/store.ts` is the live example: it opens the SQLite database on first use and closes it during disposal.

## 5. Model errors at the boundary

Throw `AppError` for expected failures:

```ts
throw new AppError({
  code: "widget_not_found",
  message: `No widget matches "${id}".`,
  exitCode: ExitCode.ERROR,
  hint: `Run '${context.app.meta.name} widgets list'.`,
});
```

Codes are stable automation contracts. Messages and hints are human guidance. Unexpected errors are normalized by the engine.

Validate external data before returning it. A subprocess result, config file, or stdin payload should not leak unchecked values into the command output schema.

## 6. Design writes for both people and automation

A write command should:

1. declare `kind: "write"`;
2. expose all required input as arguments, flags, stdin, or files;
3. support `--dry-run` when it can show a meaningful plan;
4. accept `--yes` or another explicit confirmation flag;
5. prompt only when `context.interactive` is true; and
6. return `action_required` with a retry hint in headless mode.

Keep the dry-run result schema close to the applied result so callers can inspect the same intended operation before committing to it. `add` does this with a `mode` discriminant over one shared schedule record.

## 7. Add an interactive prompt

Prompts are adapters. They are never the only workflow.

- Load prompt libraries inside the prompt function so headless startup stays fast.
- Pass `context.signal`.
- Use stdin for input and stderr for prompt output.
- Treat cancellation as an expected result or `AppError`.
- Never prompt in JSON, JSONL, CI, piped, or explicit non-interactive modes.
- Provide an equivalent flag route and document it in examples.

Use line prompts for bounded choices. A full-screen UI belongs in a separate surface.

## 8. Work inside the schedules module

The store, the runner, and the daemon have distinct jobs. Put a change where the job lives:

- `store.ts` owns the schema, all SQL, and every state transition. New persisted fields, new queries, and new recovery behavior belong here.
- `triggers.ts` owns trigger parsing and next-fire computation. A new trigger kind is a variant of `Trigger` plus its cases in `parseTrigger`, `describeTrigger`, and `nextFireAt`.
- `runner.ts` owns one fire: gate, context handoff, action, log file, and final status. Changes to the gate contract live here and nowhere else.
- `daemon.ts` owns the tick loop, heartbeat, liveness, start, stop, and recovery.
- `commands.ts` owns the CLI surface over all of it and holds no scheduling logic of its own.

Two invariants are load-bearing. The store is the only channel between the CLI and the daemon, so any new coordination must be a row someone reads. Manual and scheduled fires both go through `executeFire`, so `run` and the daemon can never disagree about what a fire means.

Read `docs/architecture.md` before changing recovery, claiming, or the gate contract.

## 9. Add configuration

Keep configuration precedence explicit and document it. A common order is:

1. command flags;
2. environment variables;
3. a user config file; and
4. compiled defaults.

Every environment variable uses the `ULTRADIAN_` prefix. `ULTRADIAN_HOME` decides where all state lives, so anything new that writes to disk resolves its path from `store.home` or `resolveHome()` and never from `os.homedir()` directly.

Put file access and credential retrieval behind services. Redact secrets in errors, debug output, and test snapshots. Do not rely on implicit `.env` loading in compiled releases.

Add offline `doctor` checks for configuration shape. Anything slow or networked belongs in a separate command so `doctor` stays quick and deterministic.

## 10. Keep output composable

Use the semantic UI helpers for human presentation. Never call `console.log` from a feature module.

- stdout: final data or event records;
- stderr: prompts, diagnostics, warnings, and progress;
- `--json`: one versioned envelope;
- `--jsonl`: one versioned event per line;
- human: borderless, width-aware terminal text.

Add a semantic style role before hardcoding ANSI output in a command. Ensure the plain-text result remains understandable. Run records use snake_case keys because they are a public contract; keep new fields in that style.

## 11. Update discovery and documentation

After changing descriptors:

```bash
bun run docs
bun run dev -- --help
bun run dev -- schema --json
bun run dev -- describe add
```

Commit `docs/commands.md`. Help, schema, describe, docs, and completion should agree because they derive from the same catalog.

## 12. Test at the right levels

A test earns its place by proving behavior that can regress independently of the change: a contract, an invariant, a boundary, or a workflow. Do not add tests that mirror implementation data, restate a changed literal, or exist only because code was extracted. Deleting a test that no longer earns its signal is part of delivery.

Tests live in a `__tests__/` directory beside the code they prove and run on `bun:test`. When behavior is worth protecting, choose the cheapest honest layer:

- unit tests for pure parsing, validation, filtering, or transformation logic;
- integration tests through real argv for exit codes, JSON envelopes, stdout/stderr discipline, `NO_COLOR` output, and headless write safety;
- one end-to-end path when the change spans command, store, and runner; and
- a compiled-binary smoke test for release-sensitive changes.

Anything touching the store or the daemon should point `ULTRADIAN_HOME` at a temporary directory, so tests never share the developer's real database.

Run:

```bash
bun run check
bun run ci
bun run build:release
```

`bun run check` applies the Ultracite policy through Oxlint and Oxfmt. Use `bun run fix` for safe mechanical rewrites, then review the resulting diff and rerun the complete gate. Type-aware Oxlint rules are enabled and complement the separate TypeScript compiler check.

The release build covers macOS and Linux on arm64 and x64. Exercise a real schedule end to end when the runner, the daemon, or a prompt changes: add one against a scratch `ULTRADIAN_HOME`, fire it with `run`, start the daemon, and read `logs`.

## 13. Adapt the agent files

`AGENTS.md` governs repository changes. Add architectural rules there without duplicating implementation details.

`SKILL.md` teaches an agent how to operate the built CLI. Update its safe read paths, write confirmation rules, examples, and recovery steps whenever the command surface moves. Prefer `schema --json` and `describe` over copying the entire command catalog into the skill.

## 14. Prepare a release

Before publishing:

1. run the complete gate on macOS and Linux;
2. build release targets;
3. verify `--help`, `version`, `doctor`, `schema --json`, and completion;
4. test `NO_COLOR=1` and a headless write;
5. confirm a daemon started from the built binary survives a restart of itself;
6. inspect the repository for secrets or private references; and
7. set `version` in `package.json` and push a matching `v<version>` tag. The release workflow runs the gate, builds the archives and `SHA256SUMS`, checks the Linux builds start on Linux, and publishes them.
