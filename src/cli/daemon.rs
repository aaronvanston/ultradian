//! `daemon start|stop|run|install|restart|uninstall` and `self install`.

use serde::Serialize;
use serde_json::Value;

use super::schedules::open_store;
use super::{Context, Done, NAME, VERSION, options};
use crate::daemon::control;
use crate::daemon::run_loop::{LoopOptions, Timing, open_daemon_log, run_daemon_loop};
use crate::errors::AppError;
use crate::output::iso_ms;
use crate::store::{DaemonInfo, resolve_home};
use crate::triggers::parse_duration;

#[derive(Serialize)]
struct DaemonRecord {
    heartbeat_at: String,
    pid: i64,
    started_at: String,
    version: String,
}

fn daemon_record(info: &DaemonInfo) -> DaemonRecord {
    DaemonRecord {
        heartbeat_at: iso_ms(info.heartbeat_at),
        pid: info.pid,
        started_at: iso_ms(info.started_at),
        version: info.version.clone(),
    }
}

pub fn start(context: &Context) -> Result<Done, AppError> {
    let store = open_store()?;
    let record = daemon_record(&control::start_daemon(&store)?);
    let ui = &context.ui;
    let human = format!(
        "{} daemon running (pid {})",
        ui.success(ui.symbols.active),
        record.pid
    );
    Ok(Done::new(&record, human))
}

pub fn stop(context: &Context) -> Result<Done, AppError> {
    let store = open_store()?;
    let (pid, stopped) = control::stop_daemon(&store)?;
    #[derive(Serialize)]
    struct Stopped {
        pid: Option<i64>,
        stopped: bool,
    }
    let ui = &context.ui;
    let human = if stopped {
        format!(
            "{} daemon stopped (pid {})",
            ui.success(ui.symbols.success),
            pid.unwrap_or_default()
        )
    } else {
        ui.muted("The daemon was not running.")
    };
    Ok(Done::new(&Stopped { pid, stopped }, human))
}

/// ULTRADIAN_RETENTION: how long the daemon keeps run history, as a
/// duration, or `off` to keep everything. Defaults to 30 days.
fn parse_retention(value: Option<String>) -> Result<Option<i64>, AppError> {
    match value.as_deref().map(str::trim) {
        None | Some("") => parse_duration("30d").map(Some),
        Some("off") => Ok(None),
        Some(_) => parse_duration(value.as_deref().unwrap_or_default()).map(Some),
    }
}

pub fn run(context: &Context) -> Result<Done, AppError> {
    let store = open_store()?;
    // The daemon loop writes its own rotated log, mirrored to stderr when
    // someone is watching it in a terminal.
    let log = {
        use std::io::IsTerminal;
        open_daemon_log(&store.home, std::io::stderr().is_terminal())
    };
    let retention_ms = parse_retention(std::env::var("ULTRADIAN_RETENTION").ok())?;
    crate::runner::watch_for_shutdown();
    run_daemon_loop(LoopOptions {
        store: &store,
        stop: &crate::runner::SHUTDOWN,
        version: VERSION,
        retention_ms,
        log,
        timing: Timing::default(),
    })?;
    #[derive(Serialize)]
    struct Stopped {
        stopped: bool,
    }
    Ok(Done::new(
        &Stopped { stopped: true },
        context.ui.muted("daemon stopped"),
    ))
}

fn service_record(service: &control::DaemonService) -> serde_json::Map<String, Value> {
    let mut record = serde_json::Map::new();
    record.insert("environment".into(), service.spec.environment_json());
    record.insert(
        "path".into(),
        Value::from(service.path.to_string_lossy().into_owned()),
    );
    record.insert("platform".into(), Value::from(service.platform));
    record.insert("program".into(), Value::from(service.spec.program.clone()));
    record
}

pub fn install(context: &Context) -> Result<Done, AppError> {
    let store = open_store()?;
    let service = control::plan_daemon_service(
        &store,
        options::string(&context.options, "path"),
        std::env::var("ULTRADIAN_RETENTION").ok(),
    );
    let mut record = service_record(&service);
    let ui = &context.ui;
    if options::flag(&context.options, "dryRun") {
        record.insert("content".into(), Value::from(service.content.as_str()));
        record.insert("mode".into(), Value::from("plan"));
        let human = [
            format!(
                "{} {} {}",
                ui.info(ui.symbols.pending),
                ui.heading("Dry run"),
                ui.muted(&service.path.to_string_lossy())
            ),
            service.content.trim_end().to_owned(),
        ]
        .join("\n");
        return Ok(Done::new(&record, human));
    }
    let info = control::install_daemon(&store, &service)?;
    let daemon = serde_json::to_value(daemon_record(&info)).unwrap_or(Value::Null);
    record.insert("daemon".into(), daemon);
    record.insert("mode".into(), Value::from("applied"));
    let human = [
        format!(
            "{} daemon supervised by {} (pid {})",
            ui.success(ui.symbols.active),
            service.platform,
            info.pid
        ),
        format!("{}  {}", ui.muted("service"), service.path.display()),
    ]
    .join("\n");
    Ok(Done::new(&record, human))
}

pub fn restart(context: &Context) -> Result<Done, AppError> {
    let store = open_store()?;
    let record = daemon_record(&control::restart_daemon(&store)?);
    let ui = &context.ui;
    let human = format!(
        "{} daemon running (pid {}, {})",
        ui.success(ui.symbols.active),
        record.pid,
        record.version
    );
    Ok(Done::new(&record, human))
}

pub fn uninstall(context: &Context) -> Result<Done, AppError> {
    let store = open_store()?;
    let (platform, path) = control::uninstall_daemon(&store)?;
    #[derive(Serialize)]
    struct Removed {
        path: String,
        platform: &'static str,
    }
    let ui = &context.ui;
    let human = format!(
        "{} daemon service removed ({platform})",
        ui.success(ui.symbols.success)
    );
    Ok(Done::new(
        &Removed {
            path: path.to_string_lossy().into_owned(),
            platform,
        },
        human,
    ))
}

pub fn self_install(context: &Context) -> Result<Done, AppError> {
    let target = options::string(&context.options, "to").map_or_else(
        || resolve_home().join("bin").join("udian"),
        std::path::PathBuf::from,
    );
    let (path, replaced) = control::install_self(&target)?;
    #[derive(Serialize)]
    struct Installed {
        path: String,
        replaced: bool,
        version: &'static str,
    }
    let data = Installed {
        path: path.to_string_lossy().into_owned(),
        replaced,
        version: VERSION,
    };
    let ui = &context.ui;
    let human = format!(
        "{} {} {} ({VERSION})",
        ui.success(ui.symbols.success),
        if replaced { "Replaced" } else { "Installed" },
        ui.command(&data.path)
    );
    let mut done = Done::new(&data, human);
    if replaced {
        done.outcome.hint = Some(format!(
            "A running daemon stays on the old binary until '{NAME} daemon restart'."
        ));
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_defaults_to_thirty_days_and_can_be_off() {
        assert_eq!(parse_retention(None), Ok(Some(30 * 86_400_000)));
        assert_eq!(parse_retention(Some(" ".into())), Ok(Some(30 * 86_400_000)));
        assert_eq!(parse_retention(Some("off".into())), Ok(None));
        assert_eq!(parse_retention(Some("7d".into())), Ok(Some(7 * 86_400_000)));
        assert_eq!(
            parse_retention(Some("0d".into())).unwrap_err().code,
            "invalid_duration"
        );
    }
}
