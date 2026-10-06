//! ultradian: gated schedules and workflows for invoking AI.
//!
//! The 0.3.0 rewrite of the 0.2.x TypeScript build in legacy/, landing in
//! phases behind the frozen contract in tests/contract/.

// Modules are declared before the code that uses them lands. Remove this
// once every command is ported.
#![allow(dead_code)]

mod catalog;
mod daemon;
mod runner;
mod store;

fn main() {
    eprintln!("ultradian {}: the Rust rewrite has no commands yet", env!("CARGO_PKG_VERSION"));
    std::process::exit(1);
}
