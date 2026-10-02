import { AppError, ExitCode, toAppError } from "./errors.ts";
import type {
  CommandContext,
  CommandOutcome,
  CommandSpec,
  GlobalOptions,
} from "./types.ts";

// Every envelope, success or error, and the command catalog carry this
// version. It changes whenever a public record shape does.
export const SCHEMA_VERSION = 2;

export interface Io {
  stdout: (value: string) => void;
  stderr: (value: string) => void;
}

export const processIo: Io = {
  stderr(value) {
    process.stderr.write(value.endsWith("\n") ? value : `${value}\n`);
  },
  stdout(value) {
    process.stdout.write(value.endsWith("\n") ? value : `${value}\n`);
  },
};

const machineSuccess = <TOptions extends Record<string, unknown>, TResult>(
  spec: CommandSpec<TOptions, TResult>,
  outcome: CommandOutcome<TResult>,
  globals: GlobalOptions
): string => {
  const envelope = {
    command: spec.path.join(" "),
    data: outcome.data,
    ok: true,
    schemaVersion: SCHEMA_VERSION,
    ...(outcome.warnings !== undefined && outcome.warnings.length > 0
      ? { warnings: outcome.warnings }
      : {}),
    ...(outcome.hint === undefined ? {} : { hint: outcome.hint }),
  };
  if (globals.mode === "jsonl") {
    return JSON.stringify({
      timestamp: new Date().toISOString(),
      type: "result",
      ...envelope,
    });
  }
  return JSON.stringify(envelope, null, globals.compact ? 0 : 2);
};

export const renderOutcome = <
  TOptions extends Record<string, unknown>,
  TResult,
>(
  spec: CommandSpec<TOptions, TResult>,
  outcome: CommandOutcome<TResult>,
  context: CommandContext<TOptions>,
  io: Io = processIo
): number => {
  if (outcome.silent !== true) {
    if (context.globals.mode === "human") {
      const human =
        spec.render?.(outcome.data, context) ??
        JSON.stringify(outcome.data, null, 2);
      if (human !== "") {
        io.stdout(human);
      }
    } else {
      io.stdout(machineSuccess(spec, outcome, context.globals));
    }
  }

  if (!context.globals.quiet && context.globals.mode === "human") {
    for (const warning of outcome.warnings ?? []) {
      io.stderr(
        `${context.ui.warning(context.ui.symbols.warning)} ${context.ui.warning(
          "Warning:"
        )} ${warning}`
      );
    }
    if (outcome.hint !== undefined) {
      io.stderr(`${context.ui.muted("hint:")} ${outcome.hint}`);
    }
  }

  return outcome.exitCode ?? ExitCode.OK;
};

export const renderError = (
  error: unknown,
  globals: GlobalOptions,
  context: Pick<CommandContext<Record<string, unknown>>, "ui">,
  io: Io = processIo
): number => {
  const appError = toAppError(error);
  if (globals.mode === "human") {
    io.stderr(
      `${context.ui.danger(context.ui.symbols.error)} ${context.ui.danger(
        "Error:"
      )} ${appError.message}`
    );
    if (appError.hint !== undefined) {
      io.stderr(`${context.ui.muted("hint:")} ${appError.hint}`);
    }
    if (appError.docsUrl !== undefined) {
      io.stderr(`${context.ui.muted("docs:")} ${appError.docsUrl}`);
    }
    return appError.exitCode;
  }

  io.stderr(
    JSON.stringify(
      {
        error: {
          code: appError.code,
          message: appError.message,
          ...(appError.hint === undefined ? {} : { hint: appError.hint }),
          ...(appError.docsUrl === undefined
            ? {}
            : { docsUrl: appError.docsUrl }),
          ...(appError.details === undefined
            ? {}
            : { details: appError.details }),
        },
        ok: false,
        schemaVersion: SCHEMA_VERSION,
      },
      null,
      globals.compact ? 0 : 2
    )
  );
  return appError.exitCode;
};

export const usageError = (message: string): AppError =>
  new AppError({
    code: "invalid_usage",
    exitCode: ExitCode.USAGE,
    message,
  });
