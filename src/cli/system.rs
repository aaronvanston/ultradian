//! `schema` and `describe`: the catalog, whole or one command at a time.

use super::{ArgValue, Context, Done, NAME};
use crate::catalog;
use crate::errors::AppError;
use crate::output::to_json;

pub fn schema() -> Done {
    let data = catalog::json();
    Done::new(data, to_json(data, false))
}

pub fn describe(context: &Context) -> Result<Done, AppError> {
    let tokens = match context.arguments.first() {
        Some(ArgValue::Many(tokens)) => tokens.clone(),
        Some(ArgValue::One(Some(token))) => vec![token.clone()],
        _ => Vec::new(),
    };
    let position = catalog::COMMANDS
        .iter()
        .position(|command| command.path == tokens);
    let Some(position) = position else {
        return Err(AppError::usage(
            "command_not_found",
            format!("No command matches \"{}\".", tokens.join(" ")),
        )
        .hint(format!("Run '{NAME} schema --json' to list command paths.")));
    };
    let command = &catalog::COMMANDS[position];
    let data = &catalog::json()["commands"][position];
    let ui = &context.ui;
    let mut lines = vec![
        ui.heading(&format!("{NAME} {}", command.path.join(" "))),
        command.summary.to_owned(),
    ];
    if let Some(description) = command.description {
        lines.extend([String::new(), description.to_owned()]);
    }
    if !command.arguments.is_empty() {
        let rows = command
            .arguments
            .iter()
            .map(|argument| {
                vec![
                    argument.name.to_owned(),
                    if argument.required {
                        "yes".into()
                    } else {
                        "no".into()
                    },
                    argument.description.unwrap_or_default().to_owned(),
                ]
            })
            .collect::<Vec<_>>();
        lines.extend([
            String::new(),
            ui.heading("Arguments:"),
            ui.table(&["Name", "Required", "Description"], &rows),
        ]);
    }
    if !command.options.is_empty() {
        let rows = command
            .options
            .iter()
            .map(|option| vec![option.flags.to_owned(), option.description.to_owned()])
            .collect::<Vec<_>>();
        lines.extend([
            String::new(),
            ui.heading("Options:"),
            ui.table(&["Flags", "Description"], &rows),
        ]);
    }
    if !command.examples.is_empty() {
        lines.extend([String::new(), ui.heading("Examples:")]);
        lines.extend(
            command
                .examples
                .iter()
                .map(|example| format!("  {} {example}", ui.muted("$"))),
        );
    }
    Ok(Done::new(data, lines.join("\n")))
}
