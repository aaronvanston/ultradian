// Preloaded before every test file (bunfig.toml). Tests start real daemons,
// write service files under HOME and delete their homes when done, so none
// of that may ever reach the home of the person running them. HOME and
// ULTRADIAN_HOME point at fresh temporary folders for the whole run, and the
// run stops if either still resolves outside the temporary directory.
import { mkdtempSync, realpathSync } from "node:fs";
import os from "node:os";
import path from "node:path";

import { resolveHome, userHome } from "../modules/schedules/store.ts";

const temporary = realpathSync(os.tmpdir());
const fresh = (prefix: string): string =>
  realpathSync(mkdtempSync(path.join(temporary, prefix)));

process.env.HOME = fresh("ultradian-test-home-");
process.env.ULTRADIAN_HOME = fresh("ultradian-test-state-");
delete process.env.XDG_CONFIG_HOME;
delete process.env.XDG_STATE_HOME;
delete process.env.XDG_DATA_HOME;

for (const [name, value] of [
  ["HOME", userHome()],
  ["ULTRADIAN_HOME", resolveHome()],
] as const) {
  if (!value?.startsWith(`${temporary}${path.sep}`)) {
    throw new Error(
      `Refusing to run tests: ${name} resolves to ${value}, outside ${temporary}.`
    );
  }
}
