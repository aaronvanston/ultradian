//! The command line: builds the command tree from the catalog, parses argv
//! the way 0.2.1 did, runs the command, and writes its outcome or error in
//! the mode the global flags ask for. Only this module writes to stdout or
//! stderr.

pub mod commander;
mod daemon;
mod doctor;
mod options;
mod schedules;
mod system;
mod version;

use std::collections::HashMap;
use std::io::Write;

use serde_json::Value;

use crate::catalog;
use crate::errors::{AppError, exit};
use crate::output::{self, ColorMode, Globals, Mode, Outcome};
use crate::style::Ui;
use commander::{Arg, ArgValue, Cmd, HelpAfterError, Invocation, Opt, OptValue, Output};

pub const NAME: &str = "ultradian";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const DESCRIPTION: &str = "Gated schedules and workflows for invoking AI";

/// Commands in the order 0.2.1 registered them, which is the order help
/// lists them in (the catalog itself is sorted by path).
const REGISTRATION_ORDER: [&str; 25] = [
    "add",
    "once",
    "list",
    "run",
    "runs",
    "cancel",
    "status",
    "logs",
    "set",
    "pause",
    "resume",
    "rm",
    "prune",
    "daemon start",
    "daemon stop",
    "daemon restart",
    "daemon install",
    "daemon uninstall",
    "daemon run",
    "self install",
    "version",
    "doctor",
    "schema",
    "describe",
    "completion",
];

const GROUPS: [(&str, &str); 2] = [
    ("daemon", "Run and manage the scheduling daemon"),
    ("self", "Manage this ultradian binary"),
];

/// The program-level options, in their help order.
fn global_options() -> Vec<Opt> {
    let mut version = Opt::new("-V, --version", "Print version");
    version.prints_version = true;
    let mut color = Opt::new("--color <when>", "Color output: auto, always, or never");
    color.choices = Some(vec!["auto".into(), "always".into(), "never".into()]);
    color.default = Some(Value::from("auto"));
    vec![
        version,
        color,
        Opt::new("--no-color", "Disable color output"),
        Opt::new("--json", "Emit one structured JSON result"),
        Opt::new("--jsonl", "Emit versioned JSON event records"),
        Opt::new("--compact", "Compact JSON onto one line"),
        Opt::new("--non-interactive", "Never prompt or animate"),
        Opt::new("--no-input", "Alias for --non-interactive"),
        Opt::new("-q, --quiet", "Suppress warnings and hints"),
        Opt::new("--verbose", "Include verbose diagnostics"),
    ]
}

fn examples_text(examples: &[String], ui: &Ui) -> String {
    if examples.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = examples
        .iter()
        .map(|example| format!("  {} {}", ui.muted("$"), ui.command(example)))
        .collect();
    format!("\n{}\n{}\n", ui.heading("Examples:"), lines.join("\n"))
}

/// The whole command tree, built from the catalog so the parser accepts
/// exactly what `schema` describes.
pub fn program(ui: &Ui) -> Cmd {
    let root_hint = HelpAfterError::Message(format!("(run '{NAME} <command> --help' for details)"));
    let mut program = Cmd {
        name: NAME.into(),
        aliases: Vec::new(),
        summary: DESCRIPTION.into(),
        before_help: Some(format!(
            "{} {} {}\n{DESCRIPTION}\n",
            ui.brand("◆"),
            ui.heading(NAME),
            ui.muted(VERSION)
        )),
        after_help: Some(format!(
            "\n{}\n",
            ui.muted(&format!(
                "Discover contracts with '{NAME} schema --json' or '{NAME} describe <command>'."
            ))
        )),
        arguments: Vec::new(),
        options: global_options(),
        commands: Vec::new(),
        help_after_error: root_hint.clone(),
        help_group: None,
        has_action: false,
        help_command: false,
        hide_description: true,
    };
    let catalog = catalog::catalog();
    for path in REGISTRATION_ORDER {
        let tokens: Vec<String> = path.split(' ').map(str::to_owned).collect();
        let Some(spec) = catalog.find(&tokens) else {
            continue;
        };
        let help_group = (spec.module == "system").then(|| "System commands:".to_owned());
        let mut parent = &mut program;
        for token in &tokens[..tokens.len() - 1] {
            if parent.find_index(token).is_none() {
                let description = GROUPS.iter().find(|(name, _)| name == token).map_or_else(
                    || format!("{token} commands"),
                    |(_, text)| (*text).to_owned(),
                );
                parent.commands.push(Cmd {
                    name: token.clone(),
                    aliases: Vec::new(),
                    summary: description,
                    before_help: None,
                    after_help: None,
                    arguments: Vec::new(),
                    options: Vec::new(),
                    commands: Vec::new(),
                    help_after_error: HelpAfterError::Help,
                    help_group: help_group.clone(),
                    has_action: false,
                    help_command: true,
                    hide_description: false,
                });
            }
            let index = parent.find_index(token).expect("group was just added");
            parent = &mut parent.commands[index];
        }
        // Commander copies this setting into a command when it is created:
        // the program's one-line hint, or a group's whole help.
        let inherited = parent.help_after_error.clone();
        let options = spec
            .options
            .iter()
            .map(|option| {
                let mut built = Opt::new(&option.flags, &option.description);
                built.choices.clone_from(&option.choices);
                built.default.clone_from(&option.default_value);
                built
            })
            .collect();
        parent.commands.push(Cmd {
            name: tokens.last().cloned().unwrap_or_default(),
            aliases: spec.aliases.clone(),
            summary: spec.summary.clone(),
            before_help: spec.description.as_ref().map(|text| format!("\n{text}\n")),
            after_help: Some(examples_text(&spec.examples, ui)),
            arguments: spec
                .arguments
                .iter()
                .map(|argument| Arg {
                    name: argument.name.clone(),
                    required: argument.required,
                    variadic: argument.variadic,
                    description: Some(argument.description.clone().unwrap_or_default()),
                })
                .collect(),
            options,
            commands: Vec::new(),
            help_after_error: inherited,
            help_group,
            has_action: true,
            help_command: false,
            hide_description: false,
        });
    }
    program
}

impl Cmd {
    fn find_index(&self, name: &str) -> Option<usize> {
        self.commands
            .iter()
            .position(|command| command.name == name)
    }
}

/// What 0.2.1 read from argv before parsing, to decide how to report a
/// failure: plain substring checks over every word, including words after
/// `--`. Errors are rendered in this mode even when parsing never got as
/// far as reading the flags.
fn preflight(argv: &[String]) -> Globals {
    let has = |flag: &str| argv.iter().any(|arg| arg == flag);
    let value_after = |name: &str| -> Option<String> {
        if let Some(index) = argv.iter().position(|arg| arg == name) {
            return argv.get(index + 1).cloned();
        }
        let prefix = format!("{name}=");
        argv.iter()
            .find_map(|arg| arg.strip_prefix(&prefix).map(str::to_owned))
    };
    let color_value = value_after("--color");
    let color = if has("--no-color") || color_value.as_deref() == Some("never") {
        ColorMode::Never
    } else if color_value.as_deref() == Some("always") {
        ColorMode::Always
    } else {
        ColorMode::Auto
    };
    let mode = if has("--jsonl") {
        Mode::Jsonl
    } else if has("--json") {
        Mode::Json
    } else {
        Mode::Human
    };
    Globals {
        color,
        compact: has("--compact"),
        mode,
        non_interactive: has("--non-interactive") || has("--no-input") || ci(),
        quiet: has("--quiet") || has("-q"),
        verbose: has("--verbose"),
    }
}

/// `Boolean(process.env.CI)`: set and not empty.
fn ci() -> bool {
    std::env::var_os("CI").is_some_and(|value| !value.is_empty())
}

fn flag(values: &HashMap<String, OptValue>, name: &str) -> bool {
    matches!(values.get(name), Some(OptValue::Bool(true)))
}

/// The global flags as the parser resolved them, for a command that runs.
fn normalize(values: &HashMap<String, OptValue>) -> Result<Globals, AppError> {
    let json = flag(values, "json");
    let jsonl = flag(values, "jsonl");
    if json && jsonl {
        return Err(AppError::usage(
            "conflicting_output_modes",
            "Use either --json or --jsonl, not both.",
        ));
    }
    let color = match values.get("color") {
        Some(OptValue::Bool(false)) => ColorMode::Never,
        Some(OptValue::Str(value)) if value == "always" => ColorMode::Always,
        Some(OptValue::Str(value)) if value == "never" => ColorMode::Never,
        _ => ColorMode::Auto,
    };
    let mode = if jsonl {
        Mode::Jsonl
    } else if json {
        Mode::Json
    } else {
        Mode::Human
    };
    Ok(Globals {
        color,
        compact: flag(values, "compact"),
        mode,
        non_interactive: flag(values, "nonInteractive")
            || matches!(values.get("input"), Some(OptValue::Bool(false)))
            || ci(),
        quiet: flag(values, "quiet"),
        verbose: flag(values, "verbose"),
    })
}

/// Everything a command gets to run with.
pub struct Context {
    /// Prompts are allowed: human mode, not --non-interactive, and both
    /// stdin and stderr are terminals.
    pub interactive: bool,
    pub cwd: std::path::PathBuf,
    pub ui: Ui,
    pub arguments: Vec<ArgValue>,
    pub options: HashMap<String, OptValue>,
}

/// A command's success: the data for machine modes and the text for
/// people. Empty text prints nothing.
pub struct Done {
    pub outcome: Outcome<Value>,
    pub human: String,
}

fn to_value<T: serde::Serialize>(data: &T) -> Value {
    serde_json::to_value(data).unwrap_or(Value::Null)
}

impl Done {
    pub fn new<T: serde::Serialize>(data: &T, human: String) -> Self {
        Self {
            outcome: Outcome::new(to_value(data)),
            human,
        }
    }
}

fn dispatch(path: &str, context: &Context) -> Result<Done, AppError> {
    match path {
        "version" => Ok(version::run(context)),
        "schema" => Ok(system::schema()),
        "describe" => system::describe(context),
        "doctor" => doctor::doctor(context),
        "completion" => doctor::completion(context),
        "add" => schedules::add(context),
        "once" => schedules::once(context),
        "list" => schedules::list(context),
        "run" => schedules::run(context),
        "runs" => schedules::runs(context),
        "cancel" => schedules::cancel(context),
        "status" => schedules::status(context),
        "logs" => schedules::logs(context),
        "set" => schedules::set(context),
        "pause" => schedules::toggle(context, false),
        "resume" => schedules::toggle(context, true),
        "rm" => schedules::rm(context),
        "prune" => schedules::prune(context),
        "daemon start" => daemon::start(context),
        "daemon stop" => daemon::stop(context),
        "daemon run" => daemon::run(context),
        "daemon install" => daemon::install(context),
        "daemon restart" => daemon::restart(context),
        "daemon uninstall" => daemon::uninstall(context),
        "self install" => daemon::self_install(context),
        _ => Err(AppError::new(
            "not_implemented",
            format!("'{NAME} {path}' is not implemented in this build yet."),
        )),
    }
}

/// Where output goes; tests capture it, the binary writes to the process.
pub trait Io {
    fn stdout(&mut self, text: &str);
    fn stderr(&mut self, text: &str);
}

/// Writes one block, adding the final newline 0.2.1 always ended with.
fn line(text: &str) -> String {
    if text.ends_with('\n') {
        text.to_owned()
    } else {
        format!("{text}\n")
    }
}

fn render_error(error: &AppError, globals: &Globals, ui: &Ui, io: &mut dyn Io) -> i32 {
    if globals.mode == Mode::Human {
        io.stderr(&line(&format!(
            "{} {} {}",
            ui.danger(ui.symbols.error),
            ui.danger("Error:"),
            error.message
        )));
        if let Some(hint) = &error.hint {
            io.stderr(&line(&format!("{} {hint}", ui.muted("hint:"))));
        }
        if let Some(docs) = &error.docs_url {
            io.stderr(&line(&format!("{} {docs}", ui.muted("docs:"))));
        }
        return error.exit_code;
    }
    io.stderr(&line(&output::error_envelope(error, globals.compact)));
    error.exit_code
}

fn render_done(path: &str, done: &Done, globals: &Globals, ui: &Ui, io: &mut dyn Io) -> i32 {
    if globals.mode == Mode::Human {
        if !done.human.is_empty() {
            io.stdout(&line(&done.human));
        }
        if !globals.quiet {
            for warning in &done.outcome.warnings {
                io.stderr(&line(&format!(
                    "{} {} {warning}",
                    ui.warning(ui.symbols.warning),
                    ui.warning("Warning:")
                )));
            }
            if let Some(hint) = &done.outcome.hint {
                io.stderr(&line(&format!("{} {hint}", ui.muted("hint:"))));
            }
        }
    } else {
        io.stdout(&line(&output::success_envelope(
            path,
            &done.outcome,
            globals,
        )));
    }
    done.outcome.exit_code
}

/// Runs one invocation and returns the process exit code.
pub fn run(argv: &[String], io: &mut dyn Io) -> i32 {
    let preflight = preflight(argv);
    let preflight_ui = Ui::new(&preflight);
    let program = program(&preflight_ui);
    if argv.is_empty() {
        io.stdout(&commander::program_help(&program));
        return exit::OK;
    }
    let mut parsed_output = Output::default();
    let parsed = commander::parse(&program, VERSION, argv, &mut parsed_output);
    if !parsed_output.stdout.is_empty() {
        io.stdout(&parsed_output.stdout);
    }
    if !parsed_output.stderr.is_empty() {
        io.stderr(&parsed_output.stderr);
    }
    let invocation: Invocation = match parsed {
        Ok(invocation) => invocation,
        Err(stop) if stop.exit_code == 0 => return exit::OK,
        Err(stop) => {
            if preflight.mode == Mode::Human {
                return exit::USAGE;
            }
            let error = AppError::usage("invalid_usage", stop.message);
            return render_error(&error, &preflight, &preflight_ui, io);
        }
    };
    let globals = match normalize(&invocation.globals) {
        Ok(globals) => globals,
        Err(error) => return render_error(&error, &preflight, &preflight_ui, io),
    };
    let path = invocation.path.join(" ");
    let interactive = {
        use std::io::IsTerminal;
        globals.mode == Mode::Human
            && !globals.non_interactive
            && std::io::stdin().is_terminal()
            && std::io::stderr().is_terminal()
    };
    let context = Context {
        interactive,
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")),
        ui: Ui::new(&globals),
        arguments: invocation.arguments,
        options: invocation.options,
    };
    match dispatch(&path, &context) {
        Ok(done) => render_done(&path, &done, &globals, &context.ui, io),
        Err(error) => render_error(&error, &preflight, &preflight_ui, io),
    }
}

/// The process's stdout and stderr. A closed pipe (`ultradian runs | head`)
/// ends the program quietly with exit 0, as 0.2.1 did.
pub struct ProcessIo;

fn write_or_exit(mut stream: impl Write, text: &str) {
    if let Err(error) = stream
        .write_all(text.as_bytes())
        .and_then(|()| stream.flush())
    {
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(exit::OK);
        }
    }
}

impl Io for ProcessIo {
    fn stdout(&mut self, text: &str) {
        write_or_exit(std::io::stdout().lock(), text);
    }
    fn stderr(&mut self, text: &str) {
        write_or_exit(std::io::stderr().lock(), text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Captured {
        stdout: String,
        stderr: String,
    }

    impl Io for Captured {
        fn stdout(&mut self, text: &str) {
            self.stdout.push_str(text);
        }
        fn stderr(&mut self, text: &str) {
            self.stderr.push_str(text);
        }
    }

    fn invoke(args: &[&str]) -> (i32, Captured) {
        let argv: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let mut captured = Captured::default();
        let code = run(&argv, &mut captured);
        (code, captured)
    }

    fn opts(args: &[&str]) -> Invocation {
        let argv: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let ui = Ui::new(&preflight(&[]));
        commander::parse(&program(&ui), VERSION, &argv, &mut Output::default()).expect("parses")
    }

    fn string(invocation: &Invocation, name: &str) -> Option<String> {
        match invocation.options.get(name) {
            Some(OptValue::Str(value)) => Some(value.clone()),
            _ => None,
        }
    }

    #[test]
    fn every_catalog_command_is_in_the_tree() {
        let ui = Ui::new(&preflight(&[]));
        let program = program(&ui);
        for command in &catalog::catalog().commands {
            let mut level = &program;
            for token in &command.path {
                level = level
                    .commands
                    .iter()
                    .find(|candidate| &candidate.name == token)
                    .unwrap_or_else(|| panic!("{} is missing", command.path.join(" ")));
            }
            assert!(level.has_action);
        }
        assert_eq!(REGISTRATION_ORDER.len(), catalog::catalog().commands.len());
    }

    #[test]
    fn globals_are_taken_from_anywhere_before_the_separator() {
        let invocation = opts(&[
            "add",
            "x",
            "--every",
            "5m",
            "--gate",
            "--json",
            "--yes",
            "--",
            "echo",
            "--dry-run",
        ]);
        assert_eq!(string(&invocation, "gate").as_deref(), Some("--yes"));
        assert_eq!(invocation.globals.get("json"), Some(&OptValue::Bool(true)));
        assert_eq!(
            invocation.arguments,
            vec![
                ArgValue::One(Some("x".into())),
                ArgValue::Many(vec!["echo".into(), "--dry-run".into()])
            ]
        );
    }

    #[test]
    fn reads_equals_negation_and_repeats() {
        let invocation = opts(&[
            "set",
            "a",
            "--every=5m",
            "--every",
            "10m",
            "--gate",
            "g",
            "--no-gate",
            "--no-timeout",
        ]);
        assert_eq!(string(&invocation, "every").as_deref(), Some("10m"));
        assert_eq!(invocation.options.get("gate"), Some(&OptValue::Bool(false)));
        assert_eq!(
            invocation.options.get("timeout"),
            Some(&OptValue::Bool(false))
        );
        assert_eq!(invocation.options.get("group"), None);
        let add = opts(&["add", "a", "--", "echo"]);
        assert_eq!(
            add.options.get("gateMode"),
            Some(&OptValue::Default(Value::from("output")))
        );
    }

    #[test]
    fn parse_errors_exit_2_with_an_envelope_under_json() {
        let (code, out) = invoke(&["list", "--bogus", "--json"]);
        assert_eq!(code, 2);
        assert_eq!(
            out.stderr,
            "error: unknown option '--bogus'\n(run 'ultradian <command> --help' for details)\n{\n  \"error\": {\n    \"code\": \"invalid_usage\",\n    \"message\": \"error: unknown option '--bogus'\"\n  },\n  \"ok\": false,\n  \"schemaVersion\": 2\n}\n"
        );
        let (code, out) = invoke(&["bogus"]);
        assert_eq!(code, 2);
        assert_eq!(
            out.stderr,
            "error: unknown command 'bogus'\n(Did you mean logs?)\n(run 'ultradian <command> --help' for details)\n"
        );
    }

    #[test]
    fn version_answers_in_every_mode() {
        let (code, out) = invoke(&["version", "--json", "--compact"]);
        assert_eq!(code, 0);
        assert!(
            out.stdout
                .starts_with("{\"command\":\"version\",\"data\":{\"arch\":")
        );
        assert!(
            out.stdout
                .contains("\"runtime\":\"rust\",\"version\":\"0.3.0\"}")
        );
        let (code, out) = invoke(&["list", "--version"]);
        assert_eq!((code, out.stdout.as_str()), (0, "0.3.0\n"));
    }
}
