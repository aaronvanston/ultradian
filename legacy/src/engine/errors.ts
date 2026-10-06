export const ExitCode = {
  CONFIG: 78,
  ERROR: 1,
  NOPERM: 77,
  OK: 0,
  TEMPFAIL: 75,
  USAGE: 2,
} as const;

export type ExitCodeValue = (typeof ExitCode)[keyof typeof ExitCode];

export interface AppErrorOptions {
  code: string;
  message: string;
  exitCode?: ExitCodeValue;
  hint?: string;
  docsUrl?: string;
  cause?: unknown;
  details?: unknown;
}

export class AppError extends Error {
  readonly code: string;
  readonly exitCode: ExitCodeValue;
  readonly hint: string | undefined;
  readonly docsUrl: string | undefined;
  readonly details: unknown;

  constructor(options: AppErrorOptions) {
    super(options.message, { cause: options.cause });
    this.name = "AppError";
    this.code = options.code;
    this.exitCode = options.exitCode ?? ExitCode.ERROR;
    this.hint = options.hint;
    this.docsUrl = options.docsUrl;
    this.details = options.details;
  }
}

export const toAppError = (error: unknown): AppError => {
  if (error instanceof AppError) {
    return error;
  }

  if (error instanceof DOMException && error.name === "AbortError") {
    return new AppError({
      cause: error,
      code: "canceled",
      exitCode: ExitCode.ERROR,
      message: "The command was canceled.",
    });
  }

  return new AppError({
    cause: error,
    code: "unexpected_error",
    message: error instanceof Error ? error.message : String(error),
  });
};
