import type { CliModule, CommandSpec } from "./types.ts";

export const defineCommand = <
  TOptions extends Record<string, unknown>,
  TResult,
>(
  spec: CommandSpec<TOptions, TResult>
): CommandSpec<TOptions, TResult> => Object.freeze(spec);

export const defineModule = (module: CliModule): CliModule =>
  Object.freeze(module);
