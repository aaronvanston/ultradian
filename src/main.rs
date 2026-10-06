//! ultradian: gated schedules and workflows for invoking AI.

// Errors carry their whole envelope (code, message, hint, details) and are
// only built on the way out, so their size never costs anything.
#![allow(clippy::result_large_err)]

mod catalog;
mod cli;
mod cron;
mod daemon;
mod errors;
mod ids;
mod output;
mod runner;
mod store;
mod style;
mod triggers;
mod zones;

fn main() {
    // Arguments that aren't valid UTF-8 are read lossily, as Node did.
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let code = cli::run(&argv);
    std::process::exit(code);
}
