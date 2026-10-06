import { z } from "zod";

import { AppError } from "./errors.ts";
import { SCHEMA_VERSION } from "./output.ts";
import type {
  CatalogCommand,
  CliModule,
  CommandCatalog,
  CommandSpec,
} from "./types.ts";

const outputSchemaFor = (spec: CommandSpec): unknown => {
  if (spec.outputSchema === undefined) {
    return undefined;
  }
  return z.toJSONSchema(spec.outputSchema, { target: "draft-7" });
};

const toCatalogCommand = (
  module: CliModule,
  spec: CommandSpec
): CatalogCommand => {
  const base = {
    aliases: spec.aliases ?? [],
    arguments: (spec.arguments ?? []).map((argument) => ({
      name: argument.name,
      required: argument.required ?? false,
      variadic: argument.variadic ?? false,
      ...(argument.description === undefined
        ? {}
        : { description: argument.description }),
    })),
    examples: spec.examples ?? [],
    kind: spec.kind ?? "read",
    module: module.id,
    options: (spec.options ?? [])
      .filter((option) => {
        if (option.hidden === true) {
          return false;
        }
        return true;
      })
      .map((option) => ({
        collect: option.collect ?? false,
        description: option.description,
        flags: option.flags,
        ...(option.choices === undefined ? {} : { choices: option.choices }),
        ...(option.defaultValue === undefined
          ? {}
          : { defaultValue: option.defaultValue }),
      })),
    path: spec.path,
    summary: spec.summary,
  };
  return {
    ...base,
    ...(spec.description === undefined
      ? {}
      : { description: spec.description }),
    ...(spec.outputSchema === undefined
      ? {}
      : { outputSchema: outputSchemaFor(spec) }),
  };
};

export interface BuiltCatalog {
  catalog: CommandCatalog;
  specs: readonly { module: CliModule; spec: CommandSpec }[];
}

export const buildCatalog = (modules: readonly CliModule[]): BuiltCatalog => {
  const specs = modules.flatMap((module) =>
    module.commands.map((spec) => ({ module, spec }))
  );
  const seen = new Map<string, string>();

  for (const { module, spec } of specs) {
    const path = spec.path.join(" ");
    const existing = seen.get(path);
    if (existing !== undefined) {
      throw new AppError({
        code: "duplicate_command",
        message: `Command "${path}" is declared by both "${existing}" and "${module.id}".`,
      });
    }
    seen.set(path, module.id);
  }

  const commands = specs
    .map(({ module, spec }) => toCatalogCommand(module, spec))
    .toSorted((left, right) =>
      left.path.join(" ").localeCompare(right.path.join(" "))
    );
  const byPath = new Map(
    commands.map((command) => [command.path.join(" "), command])
  );

  return {
    catalog: {
      commands,
      find(path) {
        return byPath.get(path.join(" "));
      },
      toJSON() {
        return { commands, schemaVersion: SCHEMA_VERSION };
      },
    },
    specs,
  };
};

const defaultValue = (value: unknown): string => {
  if (typeof value !== "string" || value === "") {
    return JSON.stringify(value);
  }
  return value;
};

export const renderMarkdownReference = (
  meta: { name: string; version: string; description: string },
  catalog: CommandCatalog
): string => {
  const sections = catalog.commands.map((command) => {
    const path = command.path.join(" ");
    const args = command.arguments
      .map((argument) => {
        const name = argument.variadic ? `${argument.name}...` : argument.name;
        return argument.required ? `<${name}>` : `[${name}]`;
      })
      .join(" ");
    const lines = [
      `## \`${meta.name} ${path}${args === "" ? "" : ` ${args}`}\``,
      "",
      command.summary,
    ];
    if (command.description !== undefined) {
      lines.push("", command.description);
    }
    if (command.options.length > 0) {
      lines.push("", "### Options", "");
      for (const option of command.options) {
        const suffix =
          option.defaultValue === undefined
            ? ""
            : ` Default: \`${defaultValue(option.defaultValue)}\`.`;
        lines.push(`- \`${option.flags}\`: ${option.description}.${suffix}`);
      }
    }
    if (command.examples.length > 0) {
      lines.push("", "### Examples", "", "```bash", ...command.examples, "```");
    }
    return lines.join("\n");
  });

  return [
    `# ${meta.name} command reference`,
    "",
    `Generated from command descriptors for ${meta.name} ${meta.version}.`,
    "",
    meta.description,
    "",
    ...sections.flatMap((section) => [section, ""]),
  ].join("\n");
};
