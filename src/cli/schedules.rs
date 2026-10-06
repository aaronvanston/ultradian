//! The store-backed commands: add, once, set, pause, resume, rm, list,
//! run --detach, cancel, runs, status, logs and prune. Records and errors
//! are 0.2.1's, field for field; human renderings follow it closely.

use std::io::{BufRead, Write};
use std::path::Path;

use serde::{Serialize, Serializer};

use super::options::{self, Bound, Toggle};
use super::{ArgValue, Context, Done, NAME};
use crate::errors::{AppError, exit};
use crate::output::{iso_ms, now_ms};
use crate::store::{
    DaemonInfo, NewSchedule, Run, Schedule, SchedulePatch, Store, is_pid_alive, resolve_home,
    resolve_path,
};
use crate::style::Ui;
use crate::triggers::{
    Trigger, describe_trigger, format_seconds, next_fire_at, parse_catch_up, parse_duration,
    parse_trigger,
};

/// A heartbeat older than this means the daemon is gone.
const HEARTBEAT_STALE_MS: i64 = 15_000;

pub fn daemon_is_live(info: Option<&DaemonInfo>) -> bool {
    info.is_some_and(|info| {
        is_pid_alive(info.pid) && now_ms() - info.heartbeat_at < HEARTBEAT_STALE_MS
    })
}

fn open_store() -> Result<Store, AppError> {
    Store::open(&resolve_home())
}

/// Milliseconds printed as seconds: `ms / 1000`, which JavaScript prints
/// without a decimal point when it is whole.
#[derive(Debug, Clone, Copy)]
struct Seconds(i64);

impl Serialize for Seconds {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.0 % 1000 == 0 {
            serializer.serialize_i64(self.0 / 1000)
        } else {
            serializer.serialize_f64(self.0 as f64 / 1000.0)
        }
    }
}

#[derive(Serialize)]
pub struct RunRecord {
    action_exit: Option<i64>,
    cwd: Option<String>,
    executor: Option<String>,
    finished_at: Option<String>,
    gate_exit: Option<i64>,
    log_pointer: Option<String>,
    machine_id: String,
    pgid: Option<i64>,
    run_id: String,
    schedule: String,
    schedule_id: String,
    started_at: String,
    status: String,
    trigger: String,
}

pub fn run_record(run: &Run) -> RunRecord {
    RunRecord {
        action_exit: run.action_exit,
        cwd: run.working_directory.clone(),
        executor: run.executor.clone(),
        finished_at: run.finished_at.map(iso_ms),
        gate_exit: run.gate_exit,
        log_pointer: run.log_pointer.clone(),
        machine_id: run.machine_id.clone(),
        pgid: run.pgid,
        run_id: run.id.clone(),
        schedule: run.schedule_name.clone(),
        schedule_id: run.schedule_id.clone(),
        started_at: iso_ms(run.started_at),
        status: run.status.clone(),
        trigger: run.trigger.clone(),
    }
}

#[derive(Serialize)]
struct ScheduleRecord {
    active: bool,
    catch_up_seconds: Seconds,
    command: Vec<String>,
    created_at: String,
    cwd: String,
    gate: Option<String>,
    gate_mode: String,
    group: Option<String>,
    id: String,
    name: String,
    next_fire_at: Option<String>,
    timeout_seconds: Option<Seconds>,
    trigger: Trigger,
    updated_at: String,
}

fn schedule_record(schedule: &Schedule) -> ScheduleRecord {
    ScheduleRecord {
        active: schedule.status == "active",
        catch_up_seconds: Seconds(schedule.catch_up_ms),
        command: schedule.command.clone(),
        created_at: iso_ms(schedule.created_at),
        cwd: schedule.working_directory.clone(),
        gate: schedule.gate.clone(),
        gate_mode: schedule.gate_mode.clone(),
        group: schedule.group.clone(),
        id: schedule.id.clone(),
        name: schedule.name.clone(),
        next_fire_at: schedule.next_fire_at.map(iso_ms),
        timeout_seconds: schedule.timeout_ms.map(Seconds),
        trigger: schedule.trigger.clone(),
        updated_at: iso_ms(schedule.updated_at),
    }
}

/// "in 5m" or "3h ago", rounded to the largest whole unit.
fn relative(ms: i64) -> String {
    let delta = ms - now_ms();
    let magnitude = delta.unsigned_abs() as f64;
    let (size, label) = [
        (86_400_000.0, "d"),
        (3_600_000.0, "h"),
        (60_000.0, "m"),
        (1000.0, "s"),
    ]
    .into_iter()
    .find(|(size, _)| magnitude >= *size)
    .unwrap_or((1000.0, "s"));
    let amount = ((magnitude / size).round() as i64).max(1);
    if delta >= 0 {
        format!("in {amount}{label}")
    } else {
        format!("{amount}{label} ago")
    }
}

fn status_word(status: &str, ui: &Ui) -> String {
    match status {
        "succeeded" | "clean" => ui.success(status),
        "failed" | "gate_failed" | "timed_out" => ui.danger(status),
        "canceled" | "interrupted" | "skipped" | "missed" => ui.warning(status),
        "running" | "queued" => ui.info(status),
        _ => ui.muted(status),
    }
}

fn argument(context: &Context, index: usize) -> Option<String> {
    match context.arguments.get(index) {
        Some(ArgValue::One(value)) => value.clone(),
        _ => None,
    }
}

fn arguments(context: &Context, index: usize) -> Vec<String> {
    match context.arguments.get(index) {
        Some(ArgValue::Many(values)) => values.clone(),
        Some(ArgValue::One(Some(value))) => vec![value.clone()],
        _ => Vec::new(),
    }
}

fn require_command(command: Vec<String>, example: &str) -> Result<Vec<String>, AppError> {
    if command.is_empty() {
        return Err(
            AppError::usage("command_required", "A command to run is required after --.")
                .hint(format!("Example: {example}")),
        );
    }
    Ok(command)
}

/// Letters, digits, dots, dashes and underscores, starting with a letter or
/// digit; surrounding whitespace is trimmed first.
fn require_name(value: Option<&str>) -> Result<String, AppError> {
    let name = value.map(str::trim).unwrap_or_default();
    let mut characters = name.chars();
    let valid = characters
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !valid {
        return Err(AppError::usage(
            "invalid_schedule_name",
            "Schedule names use letters, digits, dots, dashes, and underscores.",
        ));
    }
    Ok(name.to_owned())
}

fn resolve_working_directory(value: Option<String>, base: &Path) -> Result<String, AppError> {
    let Some(value) = value else {
        return Ok(base.to_string_lossy().into_owned());
    };
    let directory = resolve_path(base, Path::new(&value));
    if !std::fs::metadata(&directory).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(AppError::usage(
            "invalid_working_directory",
            format!("No such directory \"{value}\"."),
        )
        .hint("Pass a directory that exists to --cwd."));
    }
    Ok(directory.to_string_lossy().into_owned())
}

/// A y/N question on stderr; anything but yes, or no terminal, is no.
fn confirm(message: &str, ui: &Ui) -> bool {
    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "{} {message} {} ", ui.info("?"), ui.muted("(y/N)"));
    let _ = stderr.flush();
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

/// `--yes`, a yes at the prompt, or the error 0.2.1 gave for neither.
fn confirmed(
    context: &Context,
    question: &str,
    canceled: &str,
    hint: &str,
) -> Result<(), AppError> {
    if options::flag(&context.options, "yes")
        || (context.interactive && confirm(question, &context.ui))
    {
        return Ok(());
    }
    Err(if context.interactive {
        AppError::new("action_canceled", canceled).hint(hint)
    } else {
        AppError::usage(
            "action_required",
            "This command needs explicit confirmation in non-interactive mode.",
        )
        .hint(hint)
    })
}

fn not_running_hint(store: &Store, text: &str) -> Result<Option<String>, AppError> {
    Ok((!daemon_is_live(store.read_daemon()?.as_ref())).then(|| text.to_owned()))
}

fn schedule_lines(record: &ScheduleRecord, ui: &Ui) -> Vec<String> {
    let mut lines = vec![format!("{}     {}", ui.muted("name"), record.name)];
    if let Some(group) = &record.group {
        lines.push(format!("{}    {group}", ui.muted("group")));
    }
    lines.push(format!(
        "{}  {}",
        ui.muted("trigger"),
        describe_trigger(&record.trigger)
    ));
    if let Some(gate) = &record.gate {
        lines.push(format!("{}     {gate}", ui.muted("gate")));
    }
    lines.push(format!(
        "{}  {}",
        ui.muted("command"),
        record.command.join(" ")
    ));
    if let Some(next) = &record.next_fire_at_ms() {
        lines.push(format!("{}     {}", ui.muted("next"), relative(*next)));
    }
    lines
}

impl ScheduleRecord {
    fn next_fire_at_ms(&self) -> Option<i64> {
        self.next_fire_at.as_deref().and_then(parse_iso)
    }
}

/// Reads back a time `iso_ms` wrote.
fn parse_iso(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.timestamp_millis())
}

pub fn add(context: &Context) -> Result<Done, AppError> {
    let opts = &context.options;
    let name = require_name(argument(context, 0).as_deref())?;
    let command = require_command(
        arguments(context, 1),
        "add backup --cron \"0 2 * * *\" -- ./backup.sh",
    )?;
    let trigger = parse_trigger(
        options::string(opts, "cron").as_deref(),
        options::string(opts, "every").as_deref(),
        options::string(opts, "tz").as_deref(),
    )?;
    let catch_up_ms =
        options::string(opts, "catchUp").map_or(Ok(0), |value| parse_catch_up(&value))?;
    let group = options::string(opts, "group")
        .map(|group| require_name(Some(&group)))
        .transpose()?;
    let timeout_ms = options::string(opts, "timeout")
        .map(|value| parse_duration(&value))
        .transpose()?;
    let working_directory = resolve_working_directory(options::string(opts, "cwd"), &context.cwd)?;
    let input = NewSchedule {
        name: name.clone(),
        group,
        gate: options::string(opts, "gate"),
        gate_mode: options::string(opts, "gateMode").unwrap_or_else(|| "output".into()),
        command,
        working_directory,
        timeout_ms,
        catch_up_ms,
        trigger,
    };
    let render = |mode: &str, record: &ScheduleRecord| -> String {
        let ui = &context.ui;
        let mut lines = if mode == "plan" {
            vec![format!(
                "{} {}",
                ui.info(ui.symbols.pending),
                ui.heading("Dry run")
            )]
        } else {
            vec![format!(
                "{} Added {}",
                ui.success(ui.symbols.success),
                ui.command(&record.name)
            )]
        };
        lines.extend(schedule_lines(record, ui));
        lines.join("\n")
    };
    #[derive(Serialize)]
    struct Added {
        mode: &'static str,
        schedule: ScheduleRecord,
    }
    if options::flag(opts, "dryRun") {
        let now = now_ms();
        let preview = Schedule {
            id: "schedule_preview".into(),
            name: input.name.clone(),
            kind: "schedule".into(),
            group: input.group.clone(),
            next_fire_at: next_fire_at(&input.trigger, now),
            trigger: input.trigger.clone(),
            gate: input.gate.clone(),
            gate_mode: input.gate_mode.clone(),
            command: input.command.clone(),
            working_directory: input.working_directory.clone(),
            status: "active".into(),
            timeout_ms: input.timeout_ms,
            catch_up_ms: input.catch_up_ms,
            created_at: now,
            updated_at: now,
        };
        let data = Added {
            mode: "plan",
            schedule: schedule_record(&preview),
        };
        let mut done = Done::new(&data, render("plan", &data.schedule));
        done.outcome.hint = Some(format!("Apply with '{NAME} add {name} ... --yes'."));
        return Ok(done);
    }
    confirmed(
        context,
        &format!("Add the schedule \"{name}\"?"),
        "The schedule was not added.",
        "Preview with '--dry-run' or apply with '--yes'.",
    )?;
    let store = open_store()?;
    let schedule = store.add_schedule(input)?;
    let data = Added {
        mode: "applied",
        schedule: schedule_record(&schedule),
    };
    let mut done = Done::new(&data, render("applied", &data.schedule));
    done.outcome.hint = not_running_hint(
        &store,
        &format!("The daemon is not running; start it with '{NAME} daemon start'."),
    )?;
    Ok(done)
}

/// Node's path.basename: the last segment, ignoring trailing slashes.
fn basename(value: &str) -> String {
    value
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned()
}

pub fn once(context: &Context) -> Result<Done, AppError> {
    let opts = &context.options;
    let command = require_command(arguments(context, 0), "once -- ./deploy.sh")?;
    let store = open_store()?;
    let label = options::string(opts, "name")
        .unwrap_or_else(|| basename(command.first().map_or("job", String::as_str)));
    let name = crate::ids::create_job_name(&label);
    let timeout_ms = options::string(opts, "timeout")
        .map(|value| parse_duration(&value))
        .transpose()?;
    let working_directory = resolve_working_directory(options::string(opts, "cwd"), &context.cwd)?;
    let job = store.add_once(name, command, working_directory, timeout_ms)?;
    #[derive(Serialize)]
    struct Queued {
        command: Vec<String>,
        created_at: String,
        cwd: String,
        job: String,
        job_id: String,
        timeout_seconds: Option<Seconds>,
    }
    let data = Queued {
        command: job.command.clone(),
        created_at: iso_ms(job.created_at),
        cwd: job.working_directory.clone(),
        job: job.name.clone(),
        job_id: job.id.clone(),
        timeout_seconds: job.timeout_ms.map(Seconds),
    };
    let ui = &context.ui;
    let mut lines = vec![
        format!(
            "{} Queued {}",
            ui.success(ui.symbols.success),
            ui.command(&data.job)
        ),
        format!("{}      {}", ui.muted("job"), data.job_id),
        format!("{}  {}", ui.muted("command"), data.command.join(" ")),
        format!("{}      {}", ui.muted("cwd"), data.cwd),
    ];
    if let Some(timeout) = job.timeout_ms {
        lines.push(format!(
            "{}  {}",
            ui.muted("timeout"),
            format_seconds(timeout / 1000)
        ));
    }
    let mut done = Done::new(&data, lines.join("\n"));
    done.outcome.hint = not_running_hint(
        &store,
        &format!(
            "The daemon is not running; this job waits until it starts with '{NAME} daemon start'."
        ),
    )?;
    Ok(done)
}

pub fn runs(context: &Context) -> Result<Done, AppError> {
    let limit = context
        .options
        .get("limit")
        .map(|value| {
            options::coerce_int(
                "limit",
                value,
                Some(Bound {
                    value: 0,
                    inclusive: false,
                }),
                None,
            )
        })
        .transpose()?;
    let store = open_store()?;
    let since = match options::string(&context.options, "since") {
        None => 0,
        Some(value) => {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(AppError::usage("invalid_cursor", format!("\"{value}\" is not a runs cursor."))
                    .hint("Pass the cursor a previous 'runs' call returned, or omit --since to start from the beginning."));
            }
            value.parse().unwrap_or(i64::MAX)
        }
    };
    let runs = store.export_runs(since, limit)?;
    #[derive(Serialize)]
    struct Page {
        cursor: String,
        runs: Vec<RunRecord>,
    }
    let data = Page {
        cursor: runs.last().map_or(since, |run| run.revision).to_string(),
        runs: runs.iter().map(run_record).collect(),
    };
    let human = format!(
        "{} run record(s). Cursor {} for the next call. Use {} or {} for the records.",
        data.runs.len(),
        data.cursor,
        context.ui.flag("--json"),
        context.ui.flag("--jsonl")
    );
    Ok(Done::new(&data, human))
}

pub fn list(context: &Context) -> Result<Done, AppError> {
    let store = open_store()?;
    #[derive(Serialize)]
    struct Entry {
        last_finished_at: Option<String>,
        last_status: Option<String>,
        schedule: ScheduleRecord,
        total_runs: i64,
    }
    let mut data = Vec::new();
    for schedule in store.list_schedules()? {
        let last = store.last_run(&schedule.name)?;
        data.push(Entry {
            last_finished_at: last.as_ref().and_then(|run| run.finished_at).map(iso_ms),
            last_status: last.map(|run| run.status),
            total_runs: store.count_runs(Some(&schedule.name))?,
            schedule: schedule_record(&schedule),
        });
    }
    let ui = &context.ui;
    let human = if data.is_empty() {
        [
            ui.muted("No schedules yet."),
            format!(
                "Add one with {}.",
                ui.command(&format!(
                    "{NAME} add <name> --cron \"0 * * * *\" -- <command>"
                ))
            ),
        ]
        .join("\n")
    } else {
        let row = |entry: &Entry| -> Vec<String> {
            let schedule = &entry.schedule;
            vec![
                schedule.name.clone(),
                describe_trigger(&schedule.trigger),
                if schedule.active {
                    "active".into()
                } else {
                    ui.warning("paused")
                },
                if schedule.gate.is_some() {
                    "yes".into()
                } else {
                    String::new()
                },
                schedule.next_fire_at_ms().map(relative).unwrap_or_default(),
                match &entry.last_status {
                    None => ui.muted("never"),
                    Some(status) => {
                        let when = entry
                            .last_finished_at
                            .as_deref()
                            .and_then(parse_iso)
                            .map(|ms| format!(" {}", relative(ms)))
                            .unwrap_or_default();
                        format!("{}{when}", status_word(status, ui))
                    }
                },
                if entry.total_runs == 0 {
                    ui.muted("0")
                } else {
                    entry.total_runs.to_string()
                },
            ]
        };
        let headers = [
            "NAME", "TRIGGER", "STATUS", "GATE", "NEXT", "LAST RUN", "RUNS",
        ];
        let mut groups: Vec<String> = data
            .iter()
            .filter_map(|entry| entry.schedule.group.clone())
            .collect();
        groups.sort();
        groups.dedup();
        if groups.is_empty() {
            ui.table(&headers, &data.iter().map(row).collect::<Vec<_>>())
        } else {
            let mut sections = Vec::new();
            let ungrouped: Vec<Vec<String>> = data
                .iter()
                .filter(|entry| entry.schedule.group.is_none())
                .map(row)
                .collect();
            if !ungrouped.is_empty() {
                sections.push((None, ungrouped));
            }
            for group in groups {
                let rows = data
                    .iter()
                    .filter(|entry| entry.schedule.group.as_deref() == Some(group.as_str()))
                    .map(row)
                    .collect();
                sections.push((Some(group), rows));
            }
            ui.sectioned_table(&headers, &sections)
        }
    };
    Ok(Done::new(&data, human))
}

fn run_lines(record: &RunRecord, ui: &Ui) -> String {
    let mut lines = vec![format!(
        "{} {} {}",
        status_word(&record.status, ui),
        ui.command(&record.schedule),
        record.run_id
    )];
    if let Some(executor) = &record.executor {
        lines.push(format!("{} {executor}", ui.muted("executor")));
    }
    if let Some(log) = &record.log_pointer {
        lines.push(format!("{}      {log}", ui.muted("log")));
    }
    lines.join("\n")
}

pub fn run(context: &Context) -> Result<Done, AppError> {
    let detach = options::flag(&context.options, "detach");
    if !detach {
        // The foreground fire needs the runner (phase 4). Refuse before the
        // store records a run nothing would execute.
        return Err(AppError::new(
            "not_implemented",
            format!(
                "'{NAME} run' in the foreground is not implemented in this build yet; use --detach."
            ),
        ));
    }
    let store = open_store()?;
    let schedule = store.require_schedule(&require_name(argument(context, 0).as_deref())?)?;
    let Some(run) = store.begin_run(&schedule, "manual", true)? else {
        return Err(AppError::new(
            "run_in_flight",
            format!("\"{}\" already has a run in flight.", schedule.name),
        )
        .exit(exit::TEMPFAIL)
        .hint(format!(
            "Wait for it with '{NAME} status', or stop it with '{NAME} cancel <run_id>'."
        )));
    };
    let record = run_record(&run);
    let mut done = Done::new(&record, run_lines(&record, &context.ui));
    done.outcome.hint = not_running_hint(
        &store,
        &format!(
            "The daemon is not running; this run waits until it starts with '{NAME} daemon start'."
        ),
    )?;
    Ok(done)
}

pub fn cancel(context: &Context) -> Result<Done, AppError> {
    let store = open_store()?;
    let run = store.cancel_run(&argument(context, 0).unwrap_or_default())?;
    let record = run_record(&run);
    let ui = &context.ui;
    let human = format!(
        "{} Canceled {} {}",
        ui.success(ui.symbols.success),
        ui.command(&record.schedule),
        record.run_id
    );
    Ok(Done::new(&record, human))
}

pub fn status(context: &Context) -> Result<Done, AppError> {
    let store = open_store()?;
    let daemon = store.read_daemon()?;
    let live = daemon_is_live(daemon.as_ref());
    let schedules = store.list_schedules()?;
    let mut active_runs: Vec<RunRecord> = store.active_runs()?.iter().map(run_record).collect();
    active_runs.extend(store.queued_runs()?.iter().map(run_record));
    #[derive(Serialize)]
    struct Daemon {
        heartbeat_at: Option<String>,
        live: bool,
        pid: Option<i64>,
        started_at: Option<String>,
        version: Option<String>,
    }
    #[derive(Serialize)]
    struct Counts {
        active: usize,
        paused: usize,
        total: usize,
    }
    #[derive(Serialize)]
    struct Status {
        active_runs: Vec<RunRecord>,
        daemon: Daemon,
        schedules: Counts,
    }
    let data = Status {
        active_runs,
        daemon: Daemon {
            heartbeat_at: daemon.as_ref().map(|info| iso_ms(info.heartbeat_at)),
            live,
            pid: daemon.as_ref().map(|info| info.pid),
            started_at: daemon.as_ref().map(|info| iso_ms(info.started_at)),
            version: daemon.as_ref().map(|info| info.version.clone()),
        },
        schedules: Counts {
            active: schedules.iter().filter(|s| s.status == "active").count(),
            paused: schedules.iter().filter(|s| s.status == "paused").count(),
            total: schedules.len(),
        },
    };
    let ui = &context.ui;
    let daemon_line = if live {
        format!(
            "{} daemon running (pid {}, since {})",
            ui.success(ui.symbols.active),
            data.daemon.pid.unwrap_or_default(),
            daemon
                .as_ref()
                .map_or_else(|| "?".into(), |info| relative(info.started_at))
        )
    } else {
        format!(
            "{} daemon not running (start it with '{NAME} daemon start')",
            ui.warning(ui.symbols.warning)
        )
    };
    let mut lines = vec![
        daemon_line,
        format!(
            "{} {} active, {} paused",
            ui.muted("schedules"),
            data.schedules.active,
            data.schedules.paused
        ),
    ];
    if !data.active_runs.is_empty() {
        lines.push(ui.heading("Active runs"));
        for run in &data.active_runs {
            let started = parse_iso(&run.started_at).map(relative).unwrap_or_default();
            lines.push(format!(
                "  {} {} {}",
                run.schedule,
                run.run_id,
                ui.muted(&format!("started {started}"))
            ));
        }
    }
    Ok(Done::new(&data, lines.join("\n")))
}

pub fn logs(context: &Context) -> Result<Done, AppError> {
    let limit = options::coerce_int(
        "limit",
        context
            .options
            .get("limit")
            .unwrap_or(&super::commander::OptValue::Default(10.into())),
        Some(Bound {
            value: 1,
            inclusive: true,
        }),
        Some(Bound {
            value: 200,
            inclusive: true,
        }),
    )?;
    let store = open_store()?;
    let schedule_name = match argument(context, 0) {
        None => None,
        Some(name) => Some(store.require_schedule(&require_name(Some(&name))?)?.name),
    };
    #[derive(Serialize)]
    struct Log {
        content: String,
        run_id: String,
    }
    #[derive(Serialize)]
    struct Logs {
        log: Option<Log>,
        runs: Vec<RunRecord>,
        total_runs: i64,
    }
    let ui = &context.ui;
    if let Some(run_id) = options::string(&context.options, "run") {
        let Some(run) = store.get_run(&run_id)? else {
            return Err(AppError::new(
                "run_not_found",
                format!("No run with id \"{run_id}\"."),
            ));
        };
        let content = run
            .log_pointer
            .as_ref()
            .and_then(|pointer| std::fs::read(pointer).ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let data = Logs {
            log: Some(Log {
                content: content.clone(),
                run_id: run.id.clone(),
            }),
            runs: vec![run_record(&run)],
            total_runs: store.count_runs(Some(&run.schedule_name))?,
        };
        return Ok(Done::new(&data, content));
    }
    let runs = store.list_runs(schedule_name.as_deref(), limit)?;
    let data = Logs {
        log: None,
        runs: runs.iter().map(run_record).collect(),
        total_runs: store.count_runs(schedule_name.as_deref())?,
    };
    let human = if data.runs.is_empty() {
        ui.muted("No runs recorded yet.")
    } else {
        let rows: Vec<Vec<String>> = data
            .runs
            .iter()
            .map(|run| {
                vec![
                    run.run_id.clone(),
                    run.schedule.clone(),
                    status_word(&run.status, ui),
                    run.trigger.clone(),
                    parse_iso(&run.started_at).map(relative).unwrap_or_default(),
                    run.executor.clone().unwrap_or_default(),
                ]
            })
            .collect();
        [
            ui.table(&["RUN", "SCHEDULE", "STATUS", "TRIGGER", "STARTED", "EXECUTOR"], &rows),
            String::new(),
            ui.muted(&format!(
                "Showing {} of {} recorded runs. Read one run's output with '{NAME} logs <name> --run <id>'.",
                data.runs.len(),
                data.total_runs
            )),
        ]
        .join("\n")
    };
    Ok(Done::new(&data, human))
}

pub fn prune(context: &Context) -> Result<Done, AppError> {
    let Some(older_than) = options::string(&context.options, "olderThan") else {
        return Err(options::missing_string("olderThan"));
    };
    let schedule_name = argument(context, 0)
        .map(|name| require_name(Some(&name)))
        .transpose()?;
    let cutoff = now_ms() - parse_duration(&older_than)?;
    let whose = schedule_name
        .as_ref()
        .map_or_else(|| "all".to_owned(), |name| format!("{name}'s"));
    confirmed(
        context,
        &format!("Delete {whose} runs older than {older_than}?"),
        "Nothing was pruned.",
        "Confirm with '--yes'.",
    )?;
    let store = open_store()?;
    let (removed, freed) = store.prune_history(cutoff, schedule_name.as_deref())?;
    #[derive(Serialize)]
    struct Pruned {
        freed_bytes: i64,
        removed_runs: i64,
    }
    let ui = &context.ui;
    let human = if removed == 0 {
        ui.muted("Nothing old enough to prune.")
    } else {
        format!(
            "{} Pruned {removed} run(s), freed {:.1}MB of logs",
            ui.success(ui.symbols.success),
            freed as f64 / 1_000_000.0
        )
    };
    Ok(Done::new(
        &Pruned {
            freed_bytes: freed,
            removed_runs: removed,
        },
        human,
    ))
}

/// The new trigger, if any flag asks for one. --tz alone re-zones the
/// existing cron trigger; 'local' returns it to the machine's time.
fn trigger_patch(context: &Context, existing: &Trigger) -> Result<Option<Trigger>, AppError> {
    let opts = &context.options;
    let cron = options::string(opts, "cron");
    let every = options::string(opts, "every");
    let given_tz = options::string(opts, "tz");
    let tz = given_tz.clone().filter(|zone| zone != "local");
    if options::flag(opts, "manual") {
        if cron.is_some() || every.is_some() {
            return Err(AppError::usage(
                "conflicting_triggers",
                "Use one of --cron, --every, or --manual.",
            ));
        }
        return parse_trigger(None, None, tz.as_deref()).map(Some);
    }
    if cron.is_some() || every.is_some() {
        return parse_trigger(cron.as_deref(), every.as_deref(), tz.as_deref()).map(Some);
    }
    if let Some(zone) = given_tz {
        return match existing {
            Trigger::Cron { expression, .. } => {
                parse_trigger(Some(expression), None, tz.as_deref()).map(Some)
            }
            _ => parse_trigger(None, None, Some(&zone)).map(Some),
        };
    }
    Ok(None)
}

pub fn set(context: &Context) -> Result<Done, AppError> {
    let opts = &context.options;
    let reference = require_name(argument(context, 0).as_deref())?;
    let command = arguments(context, 1);
    let store = open_store()?;
    let existing = store.require_schedule(&reference)?;
    let mut patch = SchedulePatch {
        trigger: trigger_patch(context, &existing.trigger)?,
        ..SchedulePatch::default()
    };
    match options::toggle(opts, "gate") {
        Toggle::Removed => patch.gate = Some(None),
        Toggle::Value(gate) => patch.gate = Some(Some(gate)),
        Toggle::Unset => {}
    }
    patch.gate_mode = options::string(opts, "gateMode");
    match options::toggle(opts, "timeout") {
        Toggle::Removed => patch.timeout_ms = Some(None),
        Toggle::Value(timeout) => patch.timeout_ms = Some(Some(parse_duration(&timeout)?)),
        Toggle::Unset => {}
    }
    if let Some(catch_up) = options::string(opts, "catchUp") {
        patch.catch_up_ms = Some(parse_catch_up(&catch_up)?);
    }
    match options::toggle(opts, "group") {
        Toggle::Removed => patch.group = Some(None),
        Toggle::Value(group) => patch.group = Some(Some(require_name(Some(&group))?)),
        Toggle::Unset => {}
    }
    if let Some(cwd) = options::string(opts, "cwd") {
        patch.working_directory = Some(resolve_working_directory(Some(cwd), &context.cwd)?);
    }
    if !command.is_empty() {
        patch.command = Some(command);
    }
    if patch.is_empty() {
        return Err(AppError::usage("nothing_to_set", "Nothing to change.")
            .hint("Pass at least one flag, or a new command after --. See 'describe set'."));
    }
    let record = schedule_record(&store.update_schedule(&existing.id, patch)?);
    let ui = &context.ui;
    let mut lines = vec![
        format!(
            "{} Updated {}",
            ui.success(ui.symbols.success),
            ui.command(&record.name)
        ),
        format!(
            "{}  {}",
            ui.muted("trigger"),
            describe_trigger(&record.trigger)
        ),
    ];
    if let Some(next) = record.next_fire_at_ms() {
        lines.push(format!("{}     {}", ui.muted("next"), relative(next)));
    }
    Ok(Done::new(&record, lines.join("\n")))
}

/// `pause` and `resume`, by name or by --group.
pub fn toggle(context: &Context, resume: bool) -> Result<Done, AppError> {
    let store = open_store()?;
    let status = if resume { "active" } else { "paused" };
    let group = options::string(&context.options, "group");
    let name = argument(context, 0);
    if group.is_some() && name.is_some() {
        return Err(AppError::usage(
            "conflicting_targets",
            "Give a schedule name or --group, one at a time.",
        ));
    }
    let schedules = match group {
        Some(group) => store.set_group_status(&require_name(Some(&group))?, status)?,
        None => vec![store.set_schedule_status(&require_name(name.as_deref())?, status)?],
    };
    let records: Vec<ScheduleRecord> = schedules.iter().map(schedule_record).collect();
    let ui = &context.ui;
    let human = records
        .iter()
        .map(|record| {
            let suffix = match record.next_fire_at_ms() {
                Some(next) if resume => format!(" (next {})", relative(next)),
                _ => String::new(),
            };
            let state = if record.active { "active" } else { "paused" };
            format!(
                "{} {} {state}{suffix}",
                ui.success(ui.symbols.success),
                record.name
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Done::new(&records, human))
}

pub fn rm(context: &Context) -> Result<Done, AppError> {
    let store = open_store()?;
    let schedule = store.require_schedule(&require_name(argument(context, 0).as_deref())?)?;
    confirmed(
        context,
        &format!("Remove the schedule \"{}\"?", schedule.name),
        "The schedule was not removed.",
        "Confirm with '--yes'.",
    )?;
    store.remove_schedule(&schedule.id)?;
    #[derive(Serialize)]
    struct Removed {
        id: String,
        removed: String,
    }
    let ui = &context.ui;
    let human = format!(
        "{} Removed {} (run history kept)",
        ui.success(ui.symbols.success),
        ui.command(&schedule.name)
    );
    Ok(Done::new(
        &Removed {
            id: schedule.id,
            removed: schedule.name,
        },
        human,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_0_2_pattern() {
        for good in ["a", "arbor-x", "A.b_c-9", " padded "] {
            assert!(require_name(Some(good)).is_ok(), "{good}");
        }
        for bad in ["", " ", ".hidden", "-x", "bad name", "ümlaut", "a/b"] {
            assert_eq!(
                require_name(Some(bad)).unwrap_err().code,
                "invalid_schedule_name",
                "{bad}"
            );
        }
        assert!(require_name(None).is_err());
    }

    #[test]
    fn seconds_print_as_javascript_numbers() {
        let json = serde_json::to_string(&[Seconds(1_800_000), Seconds(0), Seconds(1500)])
            .unwrap_or_default();
        assert_eq!(json, "[1800,0,1.5]");
    }

    #[test]
    fn basenames_like_node() {
        assert_eq!(basename("/usr/bin/true"), "true");
        assert_eq!(basename("./deploy.sh"), "deploy.sh");
        assert_eq!(basename("dir/"), "dir");
        assert_eq!(basename("plain"), "plain");
    }

    #[test]
    fn relative_times_round_to_one_unit() {
        let now = now_ms();
        assert_eq!(relative(now + 5 * 60_000 + 10_000), "in 5m");
        assert_eq!(relative(now - 3 * 3_600_000), "3h ago");
    }
}
