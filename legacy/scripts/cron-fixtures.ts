// Writes the cron and time-zone fixtures the Rust rewrite must reproduce,
// computed by the real 0.2.1 trigger code (croner and Intl), so the two
// implementations are compared on the same inputs instead of on what
// anyone believes croner does:
//
//   tests/fixtures/cron.json      (expression, zone, from) -> next three fires
//   tests/fixtures/cron-errors.json  expressions croner refuses, with the
//                                 message invalid_cron embeds
//   tests/fixtures/tz-names.json  how --tz input is canonicalized, or refused
//
// A null zone means the machine's local time, so the local zone is pinned:
// the script re-runs itself under TZ=Australia/Sydney when TZ is anything
// else, and the fixture records that zone. Run it with
// 'bun scripts/cron-fixtures.ts' from legacy/.
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";

import { AppError } from "../src/engine/index.ts";
import { nextFireAt, parseTrigger } from "../src/modules/schedules/triggers.ts";

const LOCAL_ZONE = "Australia/Sydney";

if (process.env.TZ !== LOCAL_ZONE) {
  const child = Bun.spawnSync([process.execPath, ...process.argv.slice(1)], {
    env: { ...process.env, TZ: LOCAL_ZONE },
    stderr: "inherit",
    stdout: "inherit",
  });
  process.exit(child.exitCode ?? 1);
}

const zones = [
  "Australia/Sydney",
  "Australia/Melbourne",
  "America/New_York",
  "Europe/Berlin",
  "UTC",
  null,
] as const;

const expressions = [
  // hourly
  "0 * * * *",
  "30 * * * *",
  "@hourly",
  // daily, including the hours DST skips or repeats somewhere
  "0 9 * * *",
  "0 0 * * *",
  "30 1 * * *",
  "0 2 * * *",
  "30 2 * * *",
  "0 3 * * *",
  "* 2 * * *",
  "@daily",
  // weekday
  "0 9 * * 1-5",
  "0 9 * * MON-FRI",
  "0 9 * * 0",
  "0 9 * * 7",
  "@weekly",
  // step
  "*/15 * * * *",
  "*/5 9-17 * * *",
  "0 */2 * * *",
  "15 */6 * * *",
  "0 0 */3 * *",
  // list
  "0 9,17 * * *",
  "0,30 8 * * 1,3,5",
  "0 9 1,15 * *",
  // day of month and day of week together (croner ORs them)
  "0 9 1 * 1",
  "0 0 13 * 5",
  // L, W and #
  "0 9 L * *",
  "0 9 15W * *",
  "0 9 1W * *",
  "0 9 31W * *",
  "0 9 LW * *",
  "0 9 * * 1#1",
  "0 9 * * 5#2",
  "0 9 * * 5#L",
  "0 9 * * 5L",
  "0 9 L * 1",
  // calendar edges
  "0 0 29 2 *",
  "0 0 31 * *",
  "@monthly",
  "@yearly",
  // six fields (seconds first)
  "*/30 * * * * *",
  "0 0 9 * * *",
  "15 30 2 * * *",
  "0 */20 * * * *",
  // seven fields (a trailing year), which croner also accepts
  "0 0 9 * * * 2027",
  "0 30 2 * * * *",
] as const;

const invalidExpressions = [
  "",
  "   ",
  "not a cron",
  "* * * *",
  "1 2 3 4 5 6 7",
  "61 * * * *",
  "0 25 * * *",
  "0 9 32 * *",
  "0 9 0 * *",
  "0 9 * 13 *",
  "0 9 * * 8",
  "0 9 * * MON#6",
  "0 9 * * 1#0",
  "0 9 32W * *",
  "*/0 * * * *",
  "5-1 * * * *",
  "0 9 * * FOO",
  "@sometimes",
  "0 9 ? * *",
  "0 9 * * ?",
] as const;

const at = (iso: string): number => Date.parse(iso);

// Offsets come from Intl, the same source croner uses.
const offsetMinutes = (zone: string, ms: number): number => {
  const parts = new Intl.DateTimeFormat("en-US", {
    day: "2-digit",
    hour: "2-digit",
    hourCycle: "h23",
    minute: "2-digit",
    month: "2-digit",
    second: "2-digit",
    timeZone: zone,
    year: "numeric",
  }).formatToParts(new Date(ms));
  const value = (type: string): number =>
    Number(parts.find((part) => part.type === type)?.value);
  const local = Date.UTC(
    value("year"),
    value("month") - 1,
    value("day"),
    value("hour"),
    value("minute"),
    value("second")
  );
  return Math.round((local - Math.floor(ms / 1000) * 1000) / 60_000);
};

// Every instant in 2026-2027 where the zone's UTC offset changes, to the
// millisecond.
const transitions = (zone: string): number[] => {
  const found: number[] = [];
  const end = at("2028-01-01T00:00:00Z");
  for (let ms = at("2026-01-01T00:00:00Z"); ms < end; ms += 3_600_000) {
    const next = ms + 3_600_000;
    if (offsetMinutes(zone, ms) === offsetMinutes(zone, next)) {
      continue;
    }
    let low = ms;
    let high = next;
    while (high - low > 1) {
      const middle = Math.floor((low + high) / 2);
      if (offsetMinutes(zone, middle) === offsetMinutes(zone, ms)) {
        low = middle;
      } else {
        high = middle;
      }
    }
    found.push(high);
  }
  return found;
};

const generalStarts = [
  at("2026-01-15T00:00:00.000Z"),
  at("2026-07-01T12:34:56.789Z"),
  at("2026-12-31T23:59:59.999Z"),
  at("2027-02-28T10:00:00.000Z"),
];

const startsFor = (zone: string): { from: number; label: string }[] => {
  const starts = generalStarts.map((from) => ({ from, label: "general" }));
  for (const change of transitions(zone)) {
    for (const [offset, label] of [
      [-26 * 3_600_000, "dst-26h"],
      [-90 * 60_000, "dst-90m"],
      [-1, "dst-1ms"],
      [0, "dst"],
      [30 * 60_000, "dst+30m"],
    ] as const) {
      starts.push({ from: change + offset, label });
    }
  }
  return starts;
};

const cases: unknown[] = [];
for (const zone of zones) {
  const effectiveZone = zone ?? LOCAL_ZONE;
  const starts = startsFor(effectiveZone);
  for (const expression of expressions) {
    const trigger = parseTrigger({
      cron: expression,
      ...(zone === null ? {} : { tz: zone }),
    });
    for (const { from, label } of starts) {
      const next: (number | null)[] = [];
      let cursor: number | null = from;
      for (let index = 0; index < 3 && cursor !== null; index += 1) {
        cursor = nextFireAt(trigger, cursor);
        next.push(cursor);
      }
      cases.push({
        expr: expression,
        from_ms: from,
        label,
        next_ms: next,
        tz: zone,
      });
    }
  }
}

const errors = invalidExpressions.map((expression) => {
  try {
    const trigger = parseTrigger({ cron: expression, tz: "UTC" });
    return { accepted: true, expr: expression, trigger };
  } catch (error) {
    if (!(error instanceof AppError)) {
      throw error;
    }
    return { code: error.code, expr: expression, message: error.message };
  }
});

const curatedZoneNames = [
  "Australia/Sydney",
  "australia/sydney",
  "AUSTRALIA/SYDNEY",
  "aUsTrAlIa/MeLbOuRnE",
  "america/new_york",
  "europe/berlin",
  "UTC",
  "utc",
  "Etc/UTC",
  "etc/utc",
  "Etc/GMT",
  "GMT",
  "gmt",
  "Etc/GMT+5",
  "etc/gmt-10",
  "Z",
  "Zulu",
  "Universal",
  "UCT",
  "US/Eastern",
  "us/pacific",
  "EST",
  "EST5EDT",
  "CET",
  "Asia/Calcutta",
  "Asia/Kolkata",
  "Asia/Saigon",
  "Asia/Ho_Chi_Minh",
  "Europe/Kiev",
  "Europe/Kyiv",
  "America/Buenos_Aires",
  "America/Argentina/Buenos_Aires",
  "America/Indianapolis",
  "America/Indiana/Indianapolis",
  "Australia/ACT",
  "Australia/Canberra",
  "Antarctica/South_Pole",
  "Africa/Asmera",
  "Pacific/Auckland",
  "NZ",
  "+10:00",
  "+1000",
  "-05:00",
  "Mars/Olympus",
  "Australia/Sydney ",
  " Australia/Sydney",
  "Australia//Sydney",
  "local",
  "",
];

const supported = Intl.supportedValuesOf("timeZone");
const zoneInputs = [
  ...new Set([
    ...curatedZoneNames,
    ...supported,
    ...supported.map((zone) => zone.toLowerCase()),
  ]),
];
const zoneNames = zoneInputs.map((input) => {
  try {
    const trigger = parseTrigger({ cron: "0 9 * * *", tz: input });
    return {
      canonical: trigger.kind === "cron" ? trigger.timezone : null,
      input,
    };
  } catch (error) {
    if (!(error instanceof AppError)) {
      throw error;
    }
    return { code: error.code, input };
  }
});

const root = path.resolve(import.meta.dir, "../../tests/fixtures");
await mkdir(root, { recursive: true });
const cronerPackage = await import("croner/package.json");
const source = {
  bun: Bun.version,
  croner: cronerPackage.version,
  icu: process.versions.icu,
  local_zone: LOCAL_ZONE,
  tz_data: process.versions.tz,
};
// One case per line keeps the files reviewable and their diffs readable.
const write = async (name: string, rows: readonly unknown[]): Promise<void> => {
  const file = path.join(root, name);
  const body = rows.map((row) => `  ${JSON.stringify(row)}`).join(",\n");
  await writeFile(
    file,
    `{\n "source": ${JSON.stringify(source)},\n "cases": [\n${body}\n ]\n}\n`
  );
  console.log(`wrote ${file} (${rows.length} cases)`);
};
await write("cron.json", cases);
await write("cron-errors.json", errors);
await write("tz-names.json", zoneNames);
