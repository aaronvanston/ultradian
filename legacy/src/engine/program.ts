import { Command, CommanderError, Option } from "commander";
import type { HelpConfiguration } from "commander";
import type { ZodError } from "zod";

import { buildCatalog } from "./catalog.ts";
import { AppError, ExitCode } from "./errors.ts";
import { renderError, renderOutcome } from "./output.ts";
import { ServiceRegistry } from "./services.ts";
import { createUi } from "./style.ts";
import type {
  ArgumentSpec,
  CliApplication,
  CliDefinition,
  CliModule,
  CommandContext,
  CommandSpec,
  GlobalOptions,
  OptionSpec,
} from "./types.ts";

const preflightGlobals = (argv: readonly string[]): GlobalOptions => {
  const valueAfter = (name: string): string | undefined => {
    const index = argv.indexOf(name);
    if (index !== -1) {
      return argv[index + 1];
    }
    const prefix = `${name}=`;
    const match = argv.find((value) => value.startsWith(prefix));
    return match?.slice(prefix.length);
  };
  const colorValue = valueAfter("--color");
  let color: GlobalOptions["color"] = "auto";
  if (argv.includes("--no-color") || colorValue === "never") {
    color = "never";
  } else if (colorValue === "always") {
    color = "always";
  }
  let mode: GlobalOptions["mode"] = "human";
  if (argv.includes("--jsonl")) {
    mode = "jsonl";
  } else if (argv.includes("--json")) {
    mode = "json";
  }
  return {
    color,
    compact: argv.includes("--compact"),
    mode,
    nonInteractive:
      argv.includes("--non-interactive") ||
      argv.includes("--no-input") ||
      Boolean(process.env.CI),
    quiet: argv.includes("--quiet") || argv.includes("-q"),
    verbose: argv.includes("--verbose"),
  };
};

const normalizeGlobals = (command: Command): GlobalOptions => {
  const options = command.optsWithGlobals<{
    color?: string | false;
    compact?: boolean;
    input?: boolean;
    json?: boolean;
    jsonl?: boolean;
    nonInteractive?: boolean;
    quiet?: boolean;
    verbose?: boolean;
  }>();
  if (options.json === true && options.jsonl === true) {
    throw new AppError({
      code: "conflicting_output_modes",
      exitCode: ExitCode.USAGE,
      message: "Use either --json or --jsonl, not both.",
    });
  }
  let color: GlobalOptions["color"] = "auto";
  const { color: optionColor } = options;
  if (optionColor === false) {
    color = "never";
  } else if (optionColor === "always" || optionColor === "never") {
    color = optionColor;
  }
  let mode: GlobalOptions["mode"] = "human";
  if (options.jsonl === true) {
    mode = "jsonl";
  } else if (options.json === true) {
    mode = "json";
  }
  return {
    color,
    compact: options.compact ?? false,
    mode,
    nonInteractive:
      options.nonInteractive === true ||
      options.input === false ||
      Boolean(process.env.CI),
    quiet: options.quiet ?? false,
    verbose: options.verbose ?? false,
  };
};

const argumentSyntax = (argument: ArgumentSpec): string => {
  const value =
    argument.variadic === true ? `${argument.name}...` : argument.name;
  return argument.required === true ? `<${value}>` : `[${value}]`;
};

const addOption = (command: Command, spec: OptionSpec): void => {
  const option = new Option(spec.flags, spec.description);
  if (spec.choices !== undefined) {
    option.choices([...spec.choices]);
  }
  if (spec.defaultValue !== undefined) {
    option.default(spec.defaultValue);
  }
  if (spec.collect === true) {
    option.argParser((value: string, previous: string[]) => [
      ...previous,
      value,
    ]);
  }
  if (spec.hidden === true) {
    option.hideHelp();
  }
  command.addOption(option);
};

const ensureParent = (
  program: Command,
  parents: Map<string, Command>,
  modules: readonly CliModule[],
  tokens: readonly string[]
): Command => {
  let current = program;
  const path: string[] = [];
  for (const token of tokens) {
    path.push(token);
    const key = path.join(" ");
    let parent = parents.get(key);
    if (parent === undefined) {
      parent = current.command(token);
      const module = modules.find((candidate) => candidate.id === token);
      const groupDescription = modules
        .map((candidate) => candidate.commandGroups?.[token])
        .find((description) => description !== undefined);
      parent.description(
        groupDescription ?? module?.summary ?? `${token} commands`
      );
      parent.showHelpAfterError();
      parents.set(key, parent);
    }
    current = parent;
  }
  return current;
};

const helpConfiguration = (
  ui: ReturnType<typeof createUi>
): HelpConfiguration => ({
  // The root description lives in the hero banner instead.
  commandDescription(command) {
    return command.parent === null ? "" : command.description();
  },
  optionDescription(option) {
    const fallback = option.description;
    return ui.muted(fallback);
  },
  optionTerm(option) {
    return ui.flag(option.flags);
  },
  styleTitle(title) {
    return ui.heading(title);
  },
  subcommandDescription(command) {
    return ui.muted(command.description());
  },
  subcommandTerm(command) {
    return ui.command(command.name());
  },
});

const formatExamples = (
  examples: readonly string[],
  ui: ReturnType<typeof createUi>
): string => {
  if (examples.length === 0) {
    return "";
  }
  return `\n${ui.heading("Examples:")}\n${examples
    .map((example) => `  ${ui.muted("$")} ${ui.command(example)}`)
    .join("\n")}\n`;
};

const zodMessage = (error: ZodError): string =>
  error.issues
    .map((issue) => {
      const path = issue.path.length > 0 ? `${issue.path.join(".")}: ` : "";
      return `${path}${issue.message}`;
    })
    .join("; ");

export const createCli = (definition: CliDefinition): CliApplication => {
  const built = buildCatalog(definition.modules);
  const providers = [
    ...definition.modules.flatMap((module) => module.services ?? []),
    ...(definition.serviceOverrides ?? []),
  ];
  const healthChecks = definition.modules.flatMap(
    (module) => module.healthChecks ?? []
  );

  return {
    catalog: built.catalog,
    meta: definition.meta,
    async run(argv = process.argv.slice(2)) {
      const preflight = preflightGlobals(argv);
      const preflightUi = createUi({
        colorMode: preflight.color,
        machine: preflight.mode !== "human",
        streamIsTty: process.stderr.isTTY,
      });
      const services = new ServiceRegistry(providers);
      const abortController = new AbortController();
      const abort = () => {
        abortController.abort();
      };
      process.once("SIGINT", abort);
      process.once("SIGTERM", abort);
      let exitCode: number = ExitCode.OK;

      const program = new Command();
      program
        .name(definition.meta.name)
        .description(definition.meta.description)
        .version(definition.meta.version, "-V, --version", "Print version")
        .showSuggestionAfterError()
        .showHelpAfterError(
          `(run '${definition.meta.name} <command> --help' for details)`
        )
        .helpCommand(false)
        .exitOverride()
        .configureHelp(helpConfiguration(preflightUi))
        .addOption(
          new Option("--color <when>", "Color output: auto, always, or never")
            .choices(["auto", "always", "never"])
            .default("auto")
        )
        .option("--no-color", "Disable color output")
        .option("--json", "Emit one structured JSON result")
        .option("--jsonl", "Emit versioned JSON event records")
        .option("--compact", "Compact JSON onto one line")
        .option("--non-interactive", "Never prompt or animate")
        .option("--no-input", "Alias for --non-interactive")
        .option("-q, --quiet", "Suppress warnings and hints")
        .option("--verbose", "Include verbose diagnostics");

      program.addHelpText(
        "before",
        `${preflightUi.brand("◆")} ${preflightUi.heading(
          definition.meta.name
        )} ${preflightUi.muted(definition.meta.version)}\n${
          definition.meta.description
        }\n`
      );
      program.addHelpText(
        "after",
        `\n${preflightUi.muted(
          `Discover contracts with '${definition.meta.name} schema --json' or '${definition.meta.name} describe <command>'.`
        )}\n`
      );

      const parentCommands = new Map<string, Command>();
      const registerCommand = (spec: CommandSpec): void => {
        const parent = ensureParent(
          program,
          parentCommands,
          definition.modules,
          spec.path.slice(0, -1)
        );
        const commandPath = spec.path.join(" ");
        const commandName = spec.path.at(-1);
        if (commandName === undefined) {
          throw new AppError({
            code: "invalid_command_path",
            message: "A command path cannot be empty.",
          });
        }
        const command =
          parentCommands.get(commandPath) ?? parent.command(commandName);
        command.description(spec.summary);
        parentCommands.set(commandPath, command);
        const owningModule = definition.modules.find(
          (candidate) => candidate.id === spec.module
        );
        if (owningModule?.helpGroup !== undefined) {
          const topLevel =
            spec.path.length === 1
              ? command
              : parentCommands.get(spec.path[0] ?? "");
          topLevel?.helpGroup(owningModule.helpGroup);
        }
        for (const alias of spec.aliases ?? []) {
          command.alias(alias);
        }
        for (const argument of spec.arguments ?? []) {
          command.argument(
            argumentSyntax(argument),
            argument.description ?? ""
          );
        }
        for (const option of spec.options ?? []) {
          addOption(command, option);
        }
        if (spec.description !== undefined) {
          command.addHelpText("before", `\n${spec.description}\n`);
        }
        command.addHelpText(
          "after",
          formatExamples(spec.examples ?? [], preflightUi)
        );
        command.action(async (...actionArguments: unknown[]) => {
          const commandArgument = actionArguments.pop();
          if (!(commandArgument instanceof Command)) {
            throw new AppError({
              code: "invalid_command_context",
              message: "Commander did not provide the invoked command.",
            });
          }
          const invokedCommand = commandArgument;
          const globals = normalizeGlobals(invokedCommand);
          const ui = createUi({
            colorMode: globals.color,
            machine: globals.mode !== "human",
            streamIsTty: process.stderr.isTTY,
          });
          const argumentValues = Object.fromEntries(
            (spec.arguments ?? []).map((argument, index) => [
              argument.name,
              actionArguments[index],
            ])
          );
          let options = invokedCommand.opts();
          if (spec.optionsSchema !== undefined) {
            const parsed = spec.optionsSchema.safeParse(options);
            if (!parsed.success) {
              throw new AppError({
                code: "invalid_options",
                details: parsed.error.issues,
                exitCode: ExitCode.USAGE,
                message: zodMessage(parsed.error),
              });
            }
            options = parsed.data;
          }
          const context: CommandContext<Record<string, unknown>> = {
            app: {
              catalog: built.catalog,
              healthChecks,
              meta: definition.meta,
            },
            arguments: argumentValues,
            cwd: process.cwd(),
            env: process.env,
            globals,
            interactive:
              globals.mode === "human" &&
              !globals.nonInteractive &&
              process.stdin.isTTY &&
              process.stderr.isTTY,
            options,
            services,
            signal: abortController.signal,
            ui,
          };
          const outcome = await spec.run(context);
          if (spec.outputSchema !== undefined) {
            const parsed = spec.outputSchema.safeParse(outcome.data);
            if (!parsed.success) {
              throw new AppError({
                code: "invalid_command_output",
                details: parsed.error.issues,
                message: `Command "${spec.path.join(
                  " "
                )}" returned data outside its declared schema.`,
              });
            }
          }
          exitCode = renderOutcome(spec, outcome, context);
        });
      };
      for (const { spec } of built.specs) {
        registerCommand(spec);
      }

      if (argv.length === 0) {
        program.outputHelp();
      } else {
        try {
          await program.parseAsync(["node", definition.meta.name, ...argv]);
        } catch (error) {
          if (error instanceof CommanderError) {
            if (error.exitCode !== 0) {
              exitCode =
                preflight.mode === "human"
                  ? ExitCode.USAGE
                  : renderError(
                      new AppError({
                        code: "invalid_usage",
                        exitCode: ExitCode.USAGE,
                        message: error.message,
                      }),
                      preflight,
                      { ui: preflightUi }
                    );
            }
          } else {
            exitCode = renderError(error, preflight, { ui: preflightUi });
          }
        }
      }

      try {
        await services.dispose();
      } catch (error) {
        if (exitCode === ExitCode.OK) {
          exitCode = renderError(error, preflight, { ui: preflightUi });
        }
      } finally {
        process.off("SIGINT", abort);
        process.off("SIGTERM", abort);
      }
      return exitCode;
    },
  };
};
