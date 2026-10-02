import { z } from "zod";

import {
  AppError,
  defineCommand,
  ExitCode,
  generateCompletion,
} from "../../engine/index.ts";
import type { Ui } from "../../engine/index.ts";

const versionOutput = z.object({
  arch: z.string(),
  bun: z.string(),
  commit: z.string().optional(),
  name: z.string(),
  platform: z.string(),
  version: z.string(),
});

const commitMetadata = (): { commit?: string } => {
  const commit = process.env.ULTRADIAN_COMMIT;
  if (commit === undefined || commit === "") {
    return {};
  }
  return { commit };
};

export const versionCommand = defineCommand({
  examples: ["ultradian version", "ultradian version --json"],
  module: "system",
  outputSchema: versionOutput,
  path: ["version"],
  render(data, context) {
    return [
      `${context.ui.brand("◆")} ${context.ui.heading(data.name)} ${data.version}`,
      `${context.ui.muted("runtime")}  Bun ${data.bun}`,
      `${context.ui.muted("target")}   ${data.platform}-${data.arch}`,
      ...(data.commit === undefined
        ? []
        : [`${context.ui.muted("commit")}   ${data.commit}`]),
    ].join("\n");
  },
  run(context) {
    return {
      data: {
        arch: process.arch,
        bun: Bun.version,
        ...commitMetadata(),
        name: context.app.meta.name,
        platform: process.platform,
        version: context.app.meta.version,
      },
    };
  },
  summary: "Show detailed version and runtime information",
});

const checkSchema = z.object({
  detail: z.string(),
  fix: z.string().optional(),
  name: z.string(),
  status: z.enum(["pass", "warn", "fail"]),
});

const doctorOutput = z.object({
  checks: z.array(checkSchema),
  status: z.enum(["pass", "warn", "fail"]),
});

const statusSymbol = (status: "pass" | "warn" | "fail", ui: Ui): string => {
  if (status === "pass") {
    return ui.success(ui.symbols.success);
  }
  if (status === "warn") {
    return ui.warning(ui.symbols.warning);
  }
  return ui.danger(ui.symbols.error);
};

const overallStatus = (checks: readonly z.infer<typeof checkSchema>[]) => {
  if (checks.some((check) => check.status === "fail")) {
    return "fail" as const;
  }
  if (checks.some((check) => check.status === "warn")) {
    return "warn" as const;
  }
  return "pass" as const;
};

export const doctorCommand = defineCommand({
  description:
    "Runs fast, offline checks. It does not contact the configured service.",
  examples: ["ultradian doctor", "ultradian doctor --json"],
  module: "system",
  outputSchema: doctorOutput,
  path: ["doctor"],
  render(data, context) {
    return context.ui.table(
      ["", "Check", "Detail"],
      data.checks.map((check) => {
        const symbol = statusSymbol(check.status, context.ui);
        const detail =
          check.fix === undefined
            ? check.detail
            : `${check.detail} ${context.ui.muted(`Fix: ${check.fix}`)}`;
        return [symbol, check.name, detail];
      })
    );
  },
  async run(context) {
    const checks = await Promise.all(
      context.app.healthChecks.map(async (check) => await check.run())
    );
    const status = overallStatus(checks);
    return {
      data: { checks, status },
      exitCode: status === "fail" ? ExitCode.ERROR : ExitCode.OK,
      ...(status === "fail"
        ? { hint: "Resolve failed checks, then run the doctor again." }
        : {}),
    };
  },
  summary: "Check the local install and configuration",
});

const catalogOutput = z.object({
  commands: z.array(z.unknown()).readonly(),
  schemaVersion: z.number(),
});

export const schemaCommand = defineCommand({
  examples: ["ultradian schema --json"],
  module: "system",
  outputSchema: catalogOutput,
  path: ["schema"],
  render(data) {
    return JSON.stringify(data, null, 2);
  },
  run(context) {
    return { data: context.app.catalog.toJSON() };
  },
  summary: "Print the machine-readable command catalog",
});

const describeOutput = z.object({
  aliases: z.array(z.string()).readonly(),
  arguments: z
    .array(
      z.object({
        description: z.string().optional(),
        name: z.string(),
        required: z.boolean(),
        variadic: z.boolean(),
      })
    )
    .readonly(),
  description: z.string().optional(),
  examples: z.array(z.string()).readonly(),
  kind: z.enum(["read", "write"]),
  module: z.string(),
  options: z
    .array(
      z.object({
        choices: z.array(z.string()).readonly().optional(),
        collect: z.boolean(),
        defaultValue: z.unknown().optional(),
        description: z.string(),
        flags: z.string(),
      })
    )
    .readonly(),
  outputSchema: z.unknown().optional(),
  path: z.array(z.string()).readonly(),
  summary: z.string(),
});

export const describeCommand = defineCommand({
  arguments: [
    {
      description: "Command path, for example: daemon start",
      name: "command",
      required: true,
      variadic: true,
    },
  ],
  examples: [
    "ultradian describe add",
    "ultradian describe daemon start --json",
  ],
  module: "system",
  outputSchema: describeOutput,
  path: ["describe"],
  render(data, context) {
    const lines = [
      context.ui.heading(`${context.app.meta.name} ${data.path.join(" ")}`),
      data.summary,
    ];
    if (data.description !== undefined) {
      lines.push("", data.description);
    }
    if (data.arguments.length > 0) {
      lines.push(
        "",
        context.ui.heading("Arguments:"),
        context.ui.table(
          ["Name", "Required", "Description"],
          data.arguments.map((argument) => [
            argument.name,
            argument.required ? "yes" : "no",
            argument.description ?? "",
          ])
        )
      );
    }
    if (data.options.length > 0) {
      lines.push(
        "",
        context.ui.heading("Options:"),
        context.ui.table(
          ["Flags", "Description"],
          data.options.map((option) => [option.flags, option.description])
        )
      );
    }
    if (data.examples.length > 0) {
      lines.push(
        "",
        context.ui.heading("Examples:"),
        ...data.examples.map(
          (example) => `  ${context.ui.muted("$")} ${example}`
        )
      );
    }
    return lines.join("\n");
  },
  run(context) {
    const path = context.arguments.command;
    const tokens = Array.isArray(path) ? path.map(String) : [String(path)];
    const command = context.app.catalog.find(tokens);
    if (command === undefined) {
      throw new AppError({
        code: "command_not_found",
        exitCode: ExitCode.USAGE,
        hint: `Run '${context.app.meta.name} schema --json' to list command paths.`,
        message: `No command matches "${tokens.join(" ")}".`,
      });
    }
    return { data: command };
  },
  summary: "Describe one command and its contract",
});

const completionOptions = z.object({});

export const completionCommand = defineCommand({
  arguments: [
    {
      description: "bash, zsh, or fish",
      name: "shell",
      required: true,
    },
  ],
  examples: [
    "ultradian completion zsh > ~/.zfunc/_ultradian",
    "ultradian completion fish > ~/.config/fish/completions/ultradian.fish",
  ],
  module: "system",
  optionsSchema: completionOptions,
  outputSchema: z.string(),
  path: ["completion"],
  render(data) {
    return data.trimEnd();
  },
  run(context) {
    const shell = String(context.arguments.shell);
    if (shell !== "bash" && shell !== "zsh" && shell !== "fish") {
      throw new AppError({
        code: "unsupported_shell",
        exitCode: ExitCode.USAGE,
        hint: "Choose bash, zsh, or fish.",
        message: `Unsupported shell "${shell}".`,
      });
    }
    return {
      data: generateCompletion(
        context.app.meta.name,
        shell,
        context.app.catalog
      ),
    };
  },
  summary: "Generate a shell completion script",
});
