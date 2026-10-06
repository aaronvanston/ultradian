//! `version`: the build's name, version, target and commit. Install
//! scripts read `data.version` under `set -e`, so this must always
//! succeed. 0.3.0 reports `runtime: "rust"` where 0.2.x reported its Bun
//! version.

use serde::Serialize;

use super::{Context, Done, NAME, VERSION};

#[derive(Serialize)]
struct VersionRecord {
    arch: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<&'static str>,
    name: &'static str,
    platform: &'static str,
    runtime: &'static str,
    version: &'static str,
}

/// Node's names for the CPU, which the release archives are named after.
fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    }
}

/// Node's names for the OS.
fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

pub fn run(context: &Context) -> Done {
    let record = VersionRecord {
        arch: arch(),
        commit: option_env!("ULTRADIAN_COMMIT"),
        name: NAME,
        platform: platform(),
        runtime: "rust",
        version: VERSION,
    };
    let ui = &context.ui;
    let mut lines = vec![
        format!(
            "{} {} {}",
            ui.brand("◆"),
            ui.heading(record.name),
            record.version
        ),
        format!("{}  Rust", ui.muted("runtime")),
        format!(
            "{}   {}-{}",
            ui.muted("target"),
            record.platform,
            record.arch
        ),
    ];
    if let Some(commit) = record.commit {
        lines.push(format!("{}   {commit}", ui.muted("commit")));
    }
    Done::new(&record, lines.join("\n"))
}
