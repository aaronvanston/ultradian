import { Cron } from "croner";

import { AppError, ExitCode } from "../../engine/index.ts";

export type Trigger =
  | { kind: "cron"; expression: string; timezone: string | null }
  | { kind: "every"; seconds: number }
  | { kind: "manual" };

const durationPattern = /^(?<amount>\d+)(?<unit>[dhms])$/u;
const unitMs: Record<string, number> = {
  d: 86_400_000,
  h: 3_600_000,
  m: 60_000,
  s: 1000,
};

export const parseDuration = (value: string): number => {
  const groups = durationPattern.exec(value.trim())?.groups;
  const amount = Number(groups?.amount);
  const unit = groups?.unit === undefined ? undefined : unitMs[groups.unit];
  if (unit === undefined || !Number.isInteger(amount) || amount <= 0) {
    throw new AppError({
      code: "invalid_duration",
      exitCode: ExitCode.USAGE,
      message: `Expected a duration like 30s, 15m, 2h, or 1d, received "${value}".`,
    });
  }
  return amount * unit;
};

// The largest unit that divides the span evenly, so 900 reads as 15m.
export const formatSeconds = (seconds: number): string => {
  for (const [unit, size] of [
    ["d", 86_400],
    ["h", 3600],
    ["m", 60],
  ] as const) {
    if (seconds >= size && seconds % size === 0) {
      return `${seconds / size}${unit}`;
    }
  }
  return `${seconds}s`;
};

// A cron trigger reads its expression in this IANA zone; null means the
// machine's local time.
const requireTimezone = (zone: string): string => {
  try {
    return new Intl.DateTimeFormat("en-US", {
      timeZone: zone,
    }).resolvedOptions().timeZone;
  } catch {
    throw new AppError({
      code: "invalid_timezone",
      exitCode: ExitCode.USAGE,
      hint: "Use an IANA zone name such as Australia/Sydney or America/New_York.",
      message: `Unknown time zone "${zone}".`,
    });
  }
};

const cronInstance = (expression: string, timezone: string | null): Cron => {
  try {
    return new Cron(expression, {
      paused: true,
      ...(timezone === null ? {} : { timezone }),
    });
  } catch (error) {
    throw new AppError({
      cause: error,
      code: "invalid_cron",
      exitCode: ExitCode.USAGE,
      message: `Invalid cron expression "${expression}": ${
        error instanceof Error ? error.message : String(error)
      }`,
    });
  }
};

export const parseTrigger = (options: {
  cron?: string | undefined;
  every?: string | undefined;
  tz?: string | undefined;
}): Trigger => {
  if (options.tz !== undefined && options.cron === undefined) {
    throw new AppError({
      code: "timezone_requires_cron",
      exitCode: ExitCode.USAGE,
      message: "--tz applies to --cron triggers only.",
    });
  }
  if (options.cron !== undefined && options.every !== undefined) {
    throw new AppError({
      code: "conflicting_triggers",
      exitCode: ExitCode.USAGE,
      message: "Use either --cron or --every, not both.",
    });
  }
  if (options.cron !== undefined) {
    const timezone =
      options.tz === undefined ? null : requireTimezone(options.tz);
    cronInstance(options.cron, timezone).stop();
    return { expression: options.cron, kind: "cron", timezone };
  }
  if (options.every !== undefined) {
    return { kind: "every", seconds: parseDuration(options.every) / 1000 };
  }
  return { kind: "manual" };
};

export const describeTrigger = (trigger: Trigger): string => {
  if (trigger.kind === "cron") {
    return trigger.timezone === null
      ? `cron ${trigger.expression}`
      : `cron ${trigger.expression} (${trigger.timezone})`;
  }
  if (trigger.kind === "every") {
    return `every ${formatSeconds(trigger.seconds)}`;
  }
  return "manual";
};

export const nextFireAt = (trigger: Trigger, fromMs: number): number | null => {
  if (trigger.kind === "cron") {
    const cron = cronInstance(trigger.expression, trigger.timezone);
    const next = cron.nextRun(new Date(fromMs));
    cron.stop();
    return next?.getTime() ?? null;
  }
  if (trigger.kind === "every") {
    return fromMs + trigger.seconds * 1000;
  }
  return null;
};
