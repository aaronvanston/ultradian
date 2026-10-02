import type { z } from "zod";

import type { ExitCodeValue } from "./errors.ts";
import type { ServiceProvider, ServiceRegistry } from "./services.ts";
import type { Ui } from "./style.ts";

type BivariantCallback<TArguments extends unknown[], TResult> = {
  // Heterogeneous command descriptors are intentionally type-erased in the
  // module catalog, while defineCommand preserves each descriptor's types.
  // oxlint-disable-next-line typescript/method-signature-style -- method syntax supplies the required callback bivariance at that boundary.
  bivarianceHack(...arguments_: TArguments): TResult;
}["bivarianceHack"];

export type OutputMode = "human" | "json" | "jsonl";
export type CommandKind = "read" | "write";

export interface CliMeta {
  name: string;
  version: string;
  description: string;
  homepage?: string;
}

export interface ArgumentSpec {
  name: string;
  description?: string;
  required?: boolean;
  variadic?: boolean;
}

export interface OptionSpec {
  flags: string;
  description: string;
  choices?: readonly string[];
  collect?: boolean;
  defaultValue?: unknown;
  hidden?: boolean;
}

export interface GlobalOptions {
  color: "auto" | "always" | "never";
  compact: boolean;
  mode: OutputMode;
  nonInteractive: boolean;
  quiet: boolean;
  verbose: boolean;
}

export interface CatalogArgument {
  name: string;
  required: boolean;
  variadic: boolean;
  description?: string;
}

export interface CatalogOption {
  flags: string;
  description: string;
  choices?: readonly string[];
  collect: boolean;
  defaultValue?: unknown;
}

export interface CatalogCommand {
  path: readonly string[];
  aliases: readonly string[];
  module: string;
  summary: string;
  description?: string;
  examples: readonly string[];
  kind: CommandKind;
  arguments: readonly CatalogArgument[];
  options: readonly CatalogOption[];
  outputSchema?: unknown;
}

export interface CommandCatalog {
  commands: readonly CatalogCommand[];
  find: (path: readonly string[]) => CatalogCommand | undefined;
  toJSON: () => {
    schemaVersion: number;
    commands: readonly CatalogCommand[];
  };
}

export interface HealthCheckResult {
  name: string;
  status: "pass" | "warn" | "fail";
  detail: string;
  fix?: string;
}

export interface HealthCheck {
  name: string;
  run: () => HealthCheckResult | Promise<HealthCheckResult>;
}

export interface CommandAppContext {
  meta: CliMeta;
  catalog: CommandCatalog;
  healthChecks: readonly HealthCheck[];
}

export interface CommandContext<TOptions extends Record<string, unknown>> {
  app: CommandAppContext;
  arguments: Readonly<Record<string, unknown>>;
  cwd: string;
  env: Readonly<NodeJS.ProcessEnv>;
  globals: GlobalOptions;
  interactive: boolean;
  options: Readonly<TOptions>;
  services: ServiceRegistry;
  signal: AbortSignal;
  ui: Ui;
}

export interface CommandOutcome<TResult> {
  data: TResult;
  exitCode?: ExitCodeValue;
  hint?: string;
  silent?: boolean;
  warnings?: readonly string[];
}

export interface CommandSpec<
  TOptions extends Record<string, unknown> = Record<string, unknown>,
  TResult = unknown,
> {
  path: readonly [string, ...string[]];
  module?: string;
  aliases?: readonly string[];
  summary: string;
  description?: string;
  examples?: readonly string[];
  kind?: CommandKind;
  arguments?: readonly ArgumentSpec[];
  options?: readonly OptionSpec[];
  optionsSchema?: z.ZodType<TOptions>;
  outputSchema?: z.ZodType<TResult>;
  run: BivariantCallback<
    [context: CommandContext<TOptions>],
    CommandOutcome<TResult> | Promise<CommandOutcome<TResult>>
  >;
  render?: BivariantCallback<
    [data: TResult, context: CommandContext<TOptions>],
    string
  >;
}

export interface CliModule {
  id: string;
  summary: string;
  // Heading for this module's commands in root help. Omit to use
  // Commander's default "Commands:" group.
  helpGroup?: string;
  // Descriptions for intermediate command groups this module creates,
  // keyed by the group token (e.g. { daemon: "Manage the daemon" }).
  commandGroups?: Readonly<Record<string, string>>;
  commands: readonly CommandSpec[];
  services?: readonly ServiceProvider<unknown>[];
  healthChecks?: readonly HealthCheck[];
}

export interface CliDefinition {
  meta: CliMeta;
  modules: readonly CliModule[];
  serviceOverrides?: readonly ServiceProvider<unknown>[];
}

export interface CliApplication {
  readonly catalog: CommandCatalog;
  readonly meta: CliMeta;
  run: (argv?: readonly string[]) => Promise<number>;
}
