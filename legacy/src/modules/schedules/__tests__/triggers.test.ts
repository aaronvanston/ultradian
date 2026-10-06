import { describe, expect, test } from "bun:test";

import { nextFireAt, parseDuration, parseTrigger } from "../triggers.ts";

describe("trigger parsing", () => {
  test("parses durations and rejects junk", () => {
    expect(parseDuration("30s")).toBe(30_000);
    expect(parseDuration("15m")).toBe(900_000);
    expect(parseDuration("2h")).toBe(7_200_000);
    expect(parseDuration("1d")).toBe(86_400_000);
    expect(() => parseDuration("10")).toThrow();
    expect(() => parseDuration("5x")).toThrow();
    expect(() => parseDuration("0m")).toThrow();
  });

  test("rejects invalid cron and conflicting triggers", () => {
    expect(() => parseTrigger({ cron: "not a cron" })).toThrow();
    expect(() => parseTrigger({ cron: "0 * * * *", every: "5m" })).toThrow();
    expect(parseTrigger({})).toEqual({ kind: "manual" });
  });

  test("computes the next fire", () => {
    const from = Date.parse("2026-08-06T10:30:00Z");
    const onTheHour = nextFireAt(
      { expression: "0 * * * *", kind: "cron", timezone: null },
      from
    );
    expect(onTheHour).toBe(Date.parse("2026-08-06T11:00:00Z"));
    expect(nextFireAt({ kind: "every", seconds: 300 }, from)).toBe(
      from + 300_000
    );
    expect(nextFireAt({ kind: "manual" }, from)).toBeNull();
  });

  test("reads a cron expression in its own time zone", () => {
    const from = Date.parse("2026-08-06T10:30:00Z");
    const trigger = parseTrigger({ cron: "0 9 * * *", tz: "Australia/Sydney" });
    expect(trigger).toEqual({
      expression: "0 9 * * *",
      kind: "cron",
      timezone: "Australia/Sydney",
    });
    // 09:00 in Sydney (UTC+10 in August) is 23:00 UTC the day before.
    expect(nextFireAt(trigger, from)).toBe(Date.parse("2026-08-06T23:00:00Z"));
    const newYork = parseTrigger({ cron: "0 9 * * *", tz: "America/New_York" });
    expect(nextFireAt(newYork, from)).toBe(Date.parse("2026-08-06T13:00:00Z"));
    expect(() =>
      parseTrigger({ cron: "0 9 * * *", tz: "Mars/Olympus" })
    ).toThrow(/Unknown time zone/u);
    expect(() => parseTrigger({ every: "5m", tz: "UTC" })).toThrow(/--cron/u);
  });
});
