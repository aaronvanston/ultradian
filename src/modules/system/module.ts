import { defineModule } from "../../engine/index.ts";
import {
  completionCommand,
  describeCommand,
  doctorCommand,
  schemaCommand,
  versionCommand,
} from "./commands.ts";

const compareVersion = (
  actual: string,
  minimum: readonly [number, number, number]
): boolean => {
  const parts = actual.split(".").map(Number);
  for (const [index, required] of minimum.entries()) {
    const value = parts[index] ?? 0;
    if (value > required) {
      return true;
    }
    if (value < required) {
      return false;
    }
  }
  return true;
};

export const systemModule = defineModule({
  commands: [
    versionCommand,
    doctorCommand,
    schemaCommand,
    describeCommand,
    completionCommand,
  ],
  healthChecks: [
    {
      name: "Bun runtime",
      run() {
        const supported = compareVersion(Bun.version, [1, 3, 0]);
        if (supported) {
          return {
            detail: `Bun ${Bun.version}`,
            name: "Bun runtime",
            status: "pass" as const,
          };
        }
        return {
          detail: `Bun ${Bun.version}`,
          fix: "Upgrade to Bun 1.3 or later.",
          name: "Bun runtime",
          status: "fail" as const,
        };
      },
    },
    {
      name: "Operating system",
      run() {
        const supported =
          process.platform === "darwin" || process.platform === "linux";
        if (supported) {
          return {
            detail: `${process.platform}-${process.arch}`,
            name: "Operating system",
            status: "pass" as const,
          };
        }
        return {
          detail: `${process.platform}-${process.arch}`,
          fix: "Use macOS or Linux, or add and test a platform target.",
          name: "Operating system",
          status: "warn" as const,
        };
      },
    },
  ],
  helpGroup: "System commands:",
  id: "system",
  summary: "Inspect and diagnose the CLI",
});
