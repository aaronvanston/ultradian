//! The SQLite store at `$ULTRADIAN_HOME/ultradian.db`, the only channel
//! between the CLI and the daemon: schedules, runs, revision counters and
//! the daemon lock. A database from any earlier release opens and moves
//! forward in place; one this release has opened is refused by releases
//! before it, which stop at user_version 1.

mod legacy;
mod schema;

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::errors::{AppError, exit};
use crate::output::{iso_ms, now_ms};
use crate::triggers::{Trigger, next_fire_at};

pub use schema::SCHEMA_VERSION;

/// How late a fire may be reached and still count as on time.
pub const ON_TIME_MS: i64 = 30_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    pub id: String,
    pub name: String,
    /// `schedule`, or `once` for a one-shot job the daemon fires once and
    /// removes; every schedule-facing read filters those out.
    pub kind: String,
    pub group: Option<String>,
    pub trigger: Trigger,
    pub gate: Option<String>,
    /// `output` (open on exit 0 with stdout) or `exit` (exit 0 alone).
    pub gate_mode: String,
    pub command: Vec<String>,
    pub working_directory: String,
    /// `active` or `paused`.
    pub status: String,
    pub timeout_ms: Option<i64>,
    pub catch_up_ms: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub next_fire_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub id: String,
    pub schedule_id: String,
    pub schedule_name: String,
    pub machine_id: String,
    pub working_directory: Option<String>,
    pub executor: Option<String>,
    /// `scheduled`, `manual` or `once`.
    pub trigger: String,
    /// One of the frozen statuses: queued, running, clean, succeeded,
    /// failed, gate_failed, timed_out, canceled, interrupted, skipped,
    /// missed.
    pub status: String,
    pub gate_exit: Option<i64>,
    pub action_exit: Option<i64>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub log_pointer: Option<String>,
    pub owner_pid: i64,
    pub pgid: Option<i64>,
    pub revision: i64,
    /// The agent session the action started, when it is known; see
    /// `runner::execute_fire` for how it is decided.
    pub agent_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonInfo {
    pub pid: i64,
    pub version: String,
    pub started_at: i64,
    pub heartbeat_at: i64,
}

/// What `add` saves.
#[derive(Debug, Clone)]
pub struct NewSchedule {
    pub name: String,
    pub group: Option<String>,
    pub trigger: Trigger,
    pub gate: Option<String>,
    pub gate_mode: String,
    pub command: Vec<String>,
    pub working_directory: String,
    pub timeout_ms: Option<i64>,
    pub catch_up_ms: i64,
}

/// What `set` changes; `None` leaves a field as it is.
#[derive(Debug, Clone, Default)]
pub struct SchedulePatch {
    pub trigger: Option<Trigger>,
    pub gate: Option<Option<String>>,
    pub gate_mode: Option<String>,
    pub timeout_ms: Option<Option<i64>>,
    pub catch_up_ms: Option<i64>,
    pub group: Option<Option<String>>,
    pub command: Option<Vec<String>>,
    pub working_directory: Option<String>,
}

impl SchedulePatch {
    pub fn is_empty(&self) -> bool {
        self.trigger.is_none()
            && self.gate.is_none()
            && self.gate_mode.is_none()
            && self.timeout_ms.is_none()
            && self.catch_up_ms.is_none()
            && self.group.is_none()
            && self.command.is_none()
            && self.working_directory.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recovered {
    pub interrupted: usize,
    pub swept: usize,
}

pub type Result<T> = std::result::Result<T, AppError>;

/// SQLite failures surface as 0.2.1 surfaced them: unexpected_error with
/// SQLite's own message.
impl From<rusqlite::Error> for AppError {
    fn from(error: rusqlite::Error) -> Self {
        let message = match &error {
            rusqlite::Error::SqliteFailure(failure, Some(message)) => {
                let _ = failure;
                message.clone()
            }
            other => other.to_string(),
        };
        AppError::new("unexpected_error", message)
    }
}

/// The user's home folder: HOME when set and not empty, else the account's.
pub fn user_home() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home),
        _ => account_home().unwrap_or_else(|| PathBuf::from("/")),
    }
}

fn account_home() -> Option<PathBuf> {
    // SAFETY: getpwuid returns a pointer into static storage or null; it is
    // read at once and copied.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() || (*entry).pw_dir.is_null() {
            return None;
        }
        let dir = std::ffi::CStr::from_ptr((*entry).pw_dir);
        Some(PathBuf::from(
            std::ffi::OsStr::from_encoded_bytes_unchecked(dir.to_bytes()),
        ))
    }
}

/// `path.resolve(base, value)`: absolute, with `.` and `..` folded away
/// lexically and no trailing slash.
pub fn resolve_path(base: &Path, value: &Path) -> PathBuf {
    let joined = if value.is_absolute() {
        value.to_path_buf()
    } else {
        base.join(value)
    };
    let mut parts: Vec<Component> = Vec::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(parts.last(), Some(Component::Normal(_))) {
                    parts.pop();
                }
            }
            other => parts.push(other),
        }
    }
    let resolved: PathBuf = parts.iter().collect();
    if resolved.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        resolved
    }
}

/// `$ULTRADIAN_HOME`, or `~/.ultradian`.
pub fn resolve_home() -> PathBuf {
    match std::env::var_os("ULTRADIAN_HOME") {
        Some(home) if !home.is_empty() => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            resolve_path(&cwd, Path::new(&home))
        }
        _ => user_home().join(".ultradian"),
    }
}

/// The machine's hostname, which run records carry as machine_id.
pub fn machine_id() -> String {
    let mut buffer = [0_u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname NUL-terminates
    // within it on success.
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return String::new();
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

/// Whether a process exists. EPERM still means it does. Zero and negative
/// numbers name process groups to kill(2), never one process, so they are
/// never alive here.
pub fn is_pid_alive(pid: i64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks for the process.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn trigger_value(trigger: &Trigger) -> Option<String> {
    match trigger {
        Trigger::Cron { expression, .. } => Some(expression.clone()),
        Trigger::Every { seconds } => Some(seconds.to_string()),
        Trigger::Manual => None,
    }
}

fn timezone_of(trigger: &Trigger) -> Option<String> {
    match trigger {
        Trigger::Cron { timezone, .. } => timezone.clone(),
        _ => None,
    }
}

fn command_json(command: &[String]) -> String {
    serde_json::to_string(command).unwrap_or_else(|_| "[]".into())
}

/// The stored argv, keeping only its strings as 0.2.1 did.
fn parse_command(json: &str) -> Vec<String> {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(serde_json::Value::Array(parts)) => parts
            .into_iter()
            .filter_map(|part| part.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

fn schedule_from_row(row: &Row) -> rusqlite::Result<Schedule> {
    let kind: String = row.get("trigger_kind")?;
    let value: Option<String> = row.get("trigger_value")?;
    let trigger = match kind.as_str() {
        "cron" => Trigger::Cron {
            expression: value.unwrap_or_default(),
            timezone: row.get("timezone")?,
        },
        "every" => Trigger::Every {
            seconds: value
                .as_deref()
                .and_then(|text| text.trim().parse().ok())
                .unwrap_or(0),
        },
        _ => Trigger::Manual,
    };
    Ok(Schedule {
        id: row.get("id")?,
        name: row.get("name")?,
        kind: row.get("kind")?,
        group: row.get("schedule_group")?,
        trigger,
        gate: row.get("gate")?,
        gate_mode: row.get("gate_mode")?,
        command: parse_command(&row.get::<_, String>("command")?),
        working_directory: row.get("working_directory")?,
        status: row.get("status")?,
        timeout_ms: row.get("timeout_ms")?,
        catch_up_ms: row.get("catch_up_ms")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        next_fire_at: row.get("next_fire_at")?,
    })
}

fn run_from_row(row: &Row) -> rusqlite::Result<Run> {
    Ok(Run {
        id: row.get("id")?,
        schedule_id: row.get("schedule_id")?,
        schedule_name: row.get("schedule_name")?,
        machine_id: row.get("machine_id")?,
        working_directory: row.get("working_directory")?,
        executor: row.get("executor")?,
        trigger: row.get("trigger")?,
        status: row.get("status")?,
        gate_exit: row.get("gate_exit")?,
        action_exit: row.get("action_exit")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
        log_pointer: row.get("log_pointer")?,
        owner_pid: row.get("owner_pid")?,
        pgid: row.get("pgid")?,
        revision: row.get("revision")?,
        agent_session_id: row.get("agent_session_id")?,
    })
}

fn user_version(db: &Connection) -> rusqlite::Result<i64> {
    db.query_row("PRAGMA user_version", [], |row| row.get(0))
}

fn not_found(reference: &str) -> AppError {
    AppError::new(
        "schedule_not_found",
        format!("No schedule named \"{reference}\"."),
    )
    .hint("List schedules with 'list'.")
}

/// Makes the database and its WAL companions private to their owner,
/// refusing any that is not a regular file owned by this user.
fn secure_database_files(file: &Path) -> Result<()> {
    let io = |error: std::io::Error| AppError::new("unexpected_error", error.to_string());
    // SAFETY: geteuid has no preconditions and cannot fail.
    let user = unsafe { libc::geteuid() };
    for suffix in ["", "-wal", "-shm"] {
        let companion = PathBuf::from(format!("{}{suffix}", file.display()));
        let metadata = match std::fs::symlink_metadata(&companion) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io(error)),
        };
        if !metadata.file_type().is_file() || metadata.uid() != user {
            return Err(AppError::new(
                "unsafe_store",
                format!(
                    "{} is not a regular file owned by this user.",
                    companion.display()
                ),
            )
            .exit(exit::CONFIG)
            .hint("Point ULTRADIAN_HOME at a folder only you have written to."));
        }
        std::fs::set_permissions(&companion, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
    }
    Ok(())
}

pub struct Store {
    pub home: PathBuf,
    db: Connection,
}

impl Store {
    /// Opens (creating if needed) the store in `home`, bringing its schema
    /// to the latest user_version.
    pub fn open(home: &Path) -> Result<Self> {
        // Everything under the home is private to its owner: run logs hold
        // whatever gates and actions printed.
        let logs = home.join("logs");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&logs)
            .map_err(|error| AppError::new("unexpected_error", error.to_string()))?;
        for folder in [home, logs.as_path()] {
            std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| AppError::new("unexpected_error", error.to_string()))?;
        }
        let file = home.join("ultradian.db");
        // The database holds commands the daemon runs as this user, so one
        // left in the folder by anyone else is refused, not opened.
        secure_database_files(&file)?;
        let db = Connection::open(&file)?;
        // The busy timeout comes first so a second process opening a fresh
        // database waits for the first one's migration instead of failing.
        db.execute_batch("PRAGMA busy_timeout = 5000;")?;
        db.query_row("PRAGMA journal_mode = WAL;", [], |_| Ok(()))?;
        let store = Self {
            home: home.to_path_buf(),
            db,
        };
        store.migrate(&file)?;
        secure_database_files(&file)?;
        Ok(store)
    }

    /// Runs `work` in one transaction, `BEGIN IMMEDIATE` when it writes, so
    /// concurrent writers serialize instead of failing midway.
    fn transaction<T>(&self, immediate: bool, work: impl FnOnce() -> Result<T>) -> Result<T> {
        self.db.execute_batch(if immediate {
            "BEGIN IMMEDIATE"
        } else {
            "BEGIN"
        })?;
        match work() {
            Ok(value) => {
                self.db.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.db.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn migrate(&self, file: &Path) -> Result<()> {
        self.transaction(true, || {
            let current = user_version(&self.db)?;
            if current > SCHEMA_VERSION {
                return Err(AppError::new(
                    "database_too_new",
                    format!(
                        "{} is at schema version {current}, newer than this release understands ({SCHEMA_VERSION}).",
                        file.display()
                    ),
                )
                .exit(exit::CONFIG)
                .hint("Upgrade ultradian to the release that wrote this database."));
            }
            let mut start = current;
            if current == 0 && !legacy::table_columns(&self.db, "schedules")?.is_empty() {
                legacy::upgrade(&self.db)?;
                start = 1;
            }
            for (index, step) in schema::MIGRATIONS.iter().enumerate() {
                if index as i64 >= start {
                    step(&self.db)?;
                }
            }
            if current < SCHEMA_VERSION {
                self.db.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
            }
            Ok(())
        })
    }

    fn insert(&self, schedule: Schedule) -> Result<Schedule> {
        let taken: Option<String> = self
            .db
            .query_row(
                "SELECT id FROM schedules WHERE name = ?",
                [&schedule.name],
                |row| row.get(0),
            )
            .optional()?;
        if taken.is_some() {
            return Err(AppError::usage(
                "schedule_exists",
                format!("A schedule named \"{}\" already exists.", schedule.name),
            )
            .hint(format!(
                "Remove it first with 'rm {}' or pick another name.",
                schedule.name
            )));
        }
        self.db.execute(
            "INSERT INTO schedules
               (id, name, kind, schedule_group, trigger_kind, trigger_value,
                timezone, gate, gate_mode, command, working_directory, status,
                timeout_ms, catch_up_ms, created_at, updated_at, next_fire_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                schedule.id,
                schedule.name,
                schedule.kind,
                schedule.group,
                schedule.trigger.kind(),
                trigger_value(&schedule.trigger),
                timezone_of(&schedule.trigger),
                schedule.gate,
                schedule.gate_mode,
                command_json(&schedule.command),
                schedule.working_directory,
                schedule.status,
                schedule.timeout_ms,
                schedule.catch_up_ms,
                schedule.created_at,
                schedule.updated_at,
                schedule.next_fire_at,
            ],
        )?;
        Ok(schedule)
    }

    pub fn add_schedule(&self, input: NewSchedule) -> Result<Schedule> {
        let now = now_ms();
        let next = next_fire_at(&input.trigger, now);
        self.insert(Schedule {
            id: crate::ids::create_id("schedule"),
            name: input.name,
            kind: "schedule".into(),
            group: input.group,
            trigger: input.trigger,
            gate: input.gate,
            gate_mode: input.gate_mode,
            command: input.command,
            working_directory: input.working_directory,
            status: "active".into(),
            timeout_ms: input.timeout_ms,
            catch_up_ms: input.catch_up_ms,
            created_at: now,
            updated_at: now,
            next_fire_at: next,
        })
    }

    /// A one-shot job is due the moment it is registered, so the daemon
    /// claims it on its next tick. Claiming advances its manual trigger to
    /// no next fire, which is what makes it fire exactly once.
    pub fn add_once(
        &self,
        name: String,
        command: Vec<String>,
        working_directory: String,
        timeout_ms: Option<i64>,
    ) -> Result<Schedule> {
        let now = now_ms();
        self.insert(Schedule {
            id: crate::ids::create_id("job"),
            name,
            kind: "once".into(),
            group: None,
            trigger: Trigger::Manual,
            gate: None,
            gate_mode: "output".into(),
            command,
            working_directory,
            status: "active".into(),
            timeout_ms,
            catch_up_ms: 0,
            created_at: now,
            updated_at: now,
            next_fire_at: Some(now),
        })
    }

    pub fn remove_job(&self, id: &str) -> Result<()> {
        self.db
            .execute("DELETE FROM schedules WHERE id = ?", [id])?;
        Ok(())
    }

    /// A schedule by id or name, so tools can hold the stable id while
    /// people type names.
    pub fn get_schedule(&self, reference: &str) -> Result<Option<Schedule>> {
        Ok(self
            .db
            .query_row(
                "SELECT * FROM schedules
                 WHERE (id = ?1 OR name = ?1) AND kind = 'schedule'
                 ORDER BY (id = ?1) DESC LIMIT 1",
                [reference],
                schedule_from_row,
            )
            .optional()?)
    }

    pub fn require_schedule(&self, reference: &str) -> Result<Schedule> {
        self.get_schedule(reference)?
            .ok_or_else(|| not_found(reference))
    }

    pub fn list_schedules(&self) -> Result<Vec<Schedule>> {
        let mut statement = self
            .db
            .prepare("SELECT * FROM schedules WHERE kind = 'schedule' ORDER BY name")?;
        let rows = statement.query_map([], schedule_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_schedule_status(&self, reference: &str, status: &str) -> Result<Schedule> {
        let schedule = self.require_schedule(reference)?;
        let now = now_ms();
        let next = if status == "active" {
            next_fire_at(&schedule.trigger, now)
        } else {
            None
        };
        self.db.execute(
            "UPDATE schedules SET status = ?, next_fire_at = ?, updated_at = ? WHERE id = ?",
            params![status, next, now, schedule.id],
        )?;
        Ok(Schedule {
            next_fire_at: next,
            status: status.into(),
            updated_at: now,
            ..schedule
        })
    }

    /// Applies only the fields the patch sets. A new trigger recomputes the
    /// next fire from now unless the schedule is paused.
    pub fn update_schedule(&self, reference: &str, patch: SchedulePatch) -> Result<Schedule> {
        let existing = self.require_schedule(reference)?;
        let now = now_ms();
        let trigger_changed = patch.trigger.is_some();
        let mut updated = Schedule {
            catch_up_ms: patch.catch_up_ms.unwrap_or(existing.catch_up_ms),
            command: patch.command.unwrap_or_else(|| existing.command.clone()),
            gate: patch.gate.unwrap_or_else(|| existing.gate.clone()),
            gate_mode: patch
                .gate_mode
                .unwrap_or_else(|| existing.gate_mode.clone()),
            group: patch.group.unwrap_or_else(|| existing.group.clone()),
            timeout_ms: patch.timeout_ms.unwrap_or(existing.timeout_ms),
            trigger: patch.trigger.unwrap_or_else(|| existing.trigger.clone()),
            updated_at: now,
            working_directory: patch
                .working_directory
                .unwrap_or_else(|| existing.working_directory.clone()),
            ..existing.clone()
        };
        updated.next_fire_at = if !trigger_changed || existing.status == "paused" {
            existing.next_fire_at
        } else {
            next_fire_at(&updated.trigger, now)
        };
        self.db.execute(
            "UPDATE schedules
             SET trigger_kind = ?, trigger_value = ?, timezone = ?, gate = ?,
                 gate_mode = ?, timeout_ms = ?, catch_up_ms = ?,
                 schedule_group = ?, command = ?, working_directory = ?,
                 next_fire_at = ?, updated_at = ?
             WHERE id = ?",
            params![
                updated.trigger.kind(),
                trigger_value(&updated.trigger),
                timezone_of(&updated.trigger),
                updated.gate,
                updated.gate_mode,
                updated.timeout_ms,
                updated.catch_up_ms,
                updated.group,
                command_json(&updated.command),
                updated.working_directory,
                updated.next_fire_at,
                updated.updated_at,
                existing.id,
            ],
        )?;
        Ok(updated)
    }

    pub fn set_group_status(&self, group: &str, status: &str) -> Result<Vec<Schedule>> {
        let members: Vec<Schedule> = self
            .list_schedules()?
            .into_iter()
            .filter(|schedule| schedule.group.as_deref() == Some(group))
            .collect();
        if members.is_empty() {
            return Err(AppError::new(
                "group_not_found",
                format!("No schedules in group \"{group}\"."),
            )
            .hint("List schedules and their groups with 'list'."));
        }
        members
            .iter()
            .map(|member| self.set_schedule_status(&member.id, status))
            .collect()
    }

    /// Queued fires die with their schedule; a running one finishes as is.
    pub fn remove_schedule(&self, reference: &str) -> Result<Schedule> {
        let schedule = self.require_schedule(reference)?;
        self.db.execute(
            "UPDATE runs SET status = 'canceled', finished_at = ?
             WHERE schedule_id = ? AND status = 'queued'",
            params![now_ms(), schedule.id],
        )?;
        self.db
            .execute("DELETE FROM schedules WHERE id = ?", [&schedule.id])?;
        Ok(schedule)
    }

    /// Claims every schedule whose fire has come, advancing each to its next
    /// fire in the same transaction so a fire is claimed once. A fire
    /// reached later than the schedule's catch-up window (never less than
    /// ON_TIME_MS) records one missed run and skips forward instead. One-shot
    /// jobs are never missed.
    pub fn claim_due(&self, now: i64) -> Result<(Vec<Schedule>, Vec<Schedule>)> {
        self.transaction(true, || {
            let ready: Vec<Schedule> = {
                let mut statement = self.db.prepare(
                    "SELECT * FROM schedules
                     WHERE status = 'active'
                       AND next_fire_at IS NOT NULL
                       AND next_fire_at <= ?",
                )?;
                let rows = statement.query_map([now], schedule_from_row)?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            let mut due = Vec::new();
            let mut missed = Vec::new();
            for schedule in ready {
                self.db.execute(
                    "UPDATE schedules SET next_fire_at = ? WHERE id = ?",
                    params![next_fire_at(&schedule.trigger, now), schedule.id],
                )?;
                let late_by = now - schedule.next_fire_at.unwrap_or(now);
                let window = schedule.catch_up_ms.max(ON_TIME_MS);
                if schedule.kind == "schedule" && late_by > window {
                    self.record_run(&schedule, "scheduled", "missed")?;
                    missed.push(schedule);
                } else {
                    due.push(schedule);
                }
            }
            Ok((due, missed))
        })
    }

    /// The earliest fire still to come, of every active schedule and job:
    /// nothing is due before it unless another process changes the store.
    pub fn next_due_at(&self) -> Result<Option<i64>> {
        Ok(self.db.query_row(
            "SELECT MIN(next_fire_at) FROM schedules WHERE status = 'active'",
            [],
            |row| row.get(0),
        )?)
    }

    /// Changes whenever another connection commits to the store, and only
    /// then: this connection's own writes leave it as it was.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .db
            .query_row("PRAGMA data_version", [], |row| row.get(0))?)
    }

    /// A run is in flight from the moment it is queued until it finishes.
    pub fn has_active_run(&self, schedule_id: &str) -> Result<bool> {
        Ok(self
            .db
            .query_row(
                "SELECT id FROM runs
                 WHERE schedule_id = ? AND status IN ('running', 'queued') LIMIT 1",
                [schedule_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    fn insert_run(
        &self,
        schedule: &Schedule,
        trigger: &str,
        status: &str,
        with_log: bool,
    ) -> Result<Run> {
        let now = now_ms();
        let id = crate::ids::create_id("run");
        let finished = status != "running" && status != "queued";
        let log_pointer = with_log.then(|| {
            self.home
                .join("logs")
                .join(&schedule.name)
                .join(&iso_ms(now)[..10])
                .join(format!("{id}.log"))
                .to_string_lossy()
                .into_owned()
        });
        self.db.execute(
            "INSERT INTO runs
               (id, schedule_id, schedule_name, machine_id, working_directory,
                executor, trigger, status, gate_exit, action_exit, started_at,
                finished_at, log_pointer, owner_pid)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                id,
                schedule.id,
                schedule.name,
                machine_id(),
                schedule.working_directory,
                None::<String>,
                trigger,
                status,
                None::<i64>,
                None::<i64>,
                now,
                finished.then_some(now),
                log_pointer,
                i64::from(std::process::id()),
            ],
        )?;
        self.get_run(&id)?
            .ok_or_else(|| AppError::new("unexpected_error", "The run vanished as it was written."))
    }

    /// Starts a run, or queues one for the daemon, unless the schedule
    /// already has one in flight. The check and the insert share one
    /// immediate transaction, so two racing processes cannot both win.
    pub fn begin_run(
        &self,
        schedule: &Schedule,
        trigger: &str,
        queued: bool,
    ) -> Result<Option<Run>> {
        self.transaction(true, || {
            if self.has_active_run(&schedule.id)? {
                return Ok(None);
            }
            let status = if queued { "queued" } else { "running" };
            self.insert_run(schedule, trigger, status, true).map(Some)
        })
    }

    /// Records a fire that never ran: skipped for overlap, or missed.
    pub fn record_run(&self, schedule: &Schedule, trigger: &str, status: &str) -> Result<Run> {
        self.insert_run(schedule, trigger, status, false)
    }

    /// Hands queued manual fires to the daemon, moving them to running under
    /// its pid in the same transaction, so each is claimed once.
    pub fn claim_queued(&self, owner_pid: i64) -> Result<Vec<(Run, Schedule)>> {
        self.transaction(true, || {
            let now = now_ms();
            let queued = self.query_runs(
                "SELECT * FROM runs WHERE status = 'queued' ORDER BY started_at",
                [],
            )?;
            let mut claimed = Vec::new();
            for run in queued {
                let schedule = self
                    .db
                    .query_row(
                        "SELECT * FROM schedules WHERE id = ?",
                        [&run.schedule_id],
                        schedule_from_row,
                    )
                    .optional()?;
                let Some(schedule) = schedule else { continue };
                self.db.execute(
                    "UPDATE runs SET status = 'running', owner_pid = ?, started_at = ?
                     WHERE id = ?",
                    params![owner_pid, now, run.id],
                )?;
                claimed.push((
                    Run {
                        owner_pid,
                        started_at: now,
                        status: "running".into(),
                        ..run
                    },
                    schedule,
                ));
            }
            Ok(claimed)
        })
    }

    pub fn set_run_executor(&self, run_id: &str, executor: &str) -> Result<()> {
        self.db.execute(
            "UPDATE runs SET executor = ? WHERE id = ?",
            [executor, run_id],
        )?;
        Ok(())
    }

    pub fn set_run_agent_session(&self, run_id: &str, agent_session_id: &str) -> Result<()> {
        self.db.execute(
            "UPDATE runs SET agent_session_id = ? WHERE id = ?",
            [agent_session_id, run_id],
        )?;
        Ok(())
    }

    /// The process group a run is waiting on: the gate's, then the action's.
    pub fn set_run_process_group(&self, run_id: &str, pgid: i64) -> Result<()> {
        self.db.execute(
            "UPDATE runs SET pgid = ? WHERE id = ?",
            params![pgid, run_id],
        )?;
        Ok(())
    }

    /// First writer wins: a run finishes exactly once.
    pub fn finish_run(
        &self,
        run_id: &str,
        status: &str,
        gate_exit: Option<i64>,
        action_exit: Option<i64>,
    ) -> Result<()> {
        self.db.execute(
            "UPDATE runs
             SET status = ?, gate_exit = ?, action_exit = ?, finished_at = ?
             WHERE id = ? AND status = 'running'",
            params![status, gate_exit, action_exit, now_ms(), run_id],
        )?;
        Ok(())
    }

    /// Finishes a queued or running run as canceled; the process that owns
    /// the fire sees the change and stops its process group.
    pub fn cancel_run(&self, run_id: &str) -> Result<Run> {
        let Some(run) = self.get_run(run_id)? else {
            return Err(
                AppError::new("run_not_found", format!("No run with id \"{run_id}\"."))
                    .hint("List recent runs with 'runs' or 'logs'."),
            );
        };
        if run.status != "running" && run.status != "queued" {
            return Err(AppError::new(
                "run_finished",
                format!("Run {run_id} already finished as {}.", run.status),
            ));
        }
        self.db.execute(
            "UPDATE runs SET status = 'canceled', finished_at = ?
             WHERE id = ? AND status IN ('running', 'queued')",
            params![now_ms(), run_id],
        )?;
        Ok(self.get_run(run_id)?.unwrap_or(run))
    }

    pub fn get_run(&self, run_id: &str) -> Result<Option<Run>> {
        Ok(self
            .db
            .query_row("SELECT * FROM runs WHERE id = ?", [run_id], run_from_row)
            .optional()?)
    }

    fn query_runs<P: rusqlite::Params>(&self, sql: &str, params: P) -> Result<Vec<Run>> {
        let mut statement = self.db.prepare(sql)?;
        let rows = statement.query_map(params, run_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The read surface for other tools: every run changed after a revision
    /// cursor, oldest change first, each in its latest state.
    pub fn export_runs(&self, since: i64, limit: Option<i64>) -> Result<Vec<Run>> {
        self.query_runs(
            "SELECT * FROM runs WHERE revision > ? ORDER BY revision ASC LIMIT ?",
            params![since, limit.unwrap_or(-1)],
        )
    }

    pub fn list_runs(&self, schedule_name: Option<&str>, limit: i64) -> Result<Vec<Run>> {
        match schedule_name {
            Some(name) => self.query_runs(
                "SELECT * FROM runs WHERE schedule_name = ?
                 ORDER BY started_at DESC LIMIT ?",
                params![name, limit],
            ),
            None => self.query_runs(
                "SELECT * FROM runs ORDER BY started_at DESC LIMIT ?",
                [limit],
            ),
        }
    }

    pub fn active_runs(&self) -> Result<Vec<Run>> {
        self.query_runs(
            "SELECT * FROM runs WHERE status = 'running' ORDER BY started_at",
            [],
        )
    }

    pub fn queued_runs(&self) -> Result<Vec<Run>> {
        self.query_runs(
            "SELECT * FROM runs WHERE status = 'queued' ORDER BY started_at",
            [],
        )
    }

    pub fn last_run(&self, schedule_name: &str) -> Result<Option<Run>> {
        Ok(self
            .db
            .query_row(
                "SELECT * FROM runs WHERE schedule_name = ?
                 ORDER BY started_at DESC LIMIT 1",
                [schedule_name],
                run_from_row,
            )
            .optional()?)
    }

    pub fn count_runs(&self, schedule_name: Option<&str>) -> Result<i64> {
        Ok(match schedule_name {
            Some(name) => self.db.query_row(
                "SELECT COUNT(*) AS total FROM runs WHERE schedule_name = ?",
                [name],
                |row| row.get(0),
            )?,
            None => self
                .db
                .query_row("SELECT COUNT(*) AS total FROM runs", [], |row| row.get(0))?,
        })
    }

    /// Deletes finished runs that started before the cutoff with their log
    /// files, then any day or schedule log folder left empty. Queued and
    /// running runs are never touched. Returns (removed, freed bytes).
    pub fn prune_history(&self, cutoff_ms: i64, schedule_name: Option<&str>) -> Result<(i64, i64)> {
        let stale: Vec<(String, Option<String>)> = {
            let mut statement = self.db.prepare(
                "SELECT id, log_pointer FROM runs
                 WHERE status NOT IN ('running', 'queued') AND started_at < ?1
                   AND (?2 IS NULL OR schedule_name = ?2)",
            )?;
            let rows = statement.query_map(params![cutoff_ms, schedule_name], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        self.transaction(false, || {
            for (id, _) in &stale {
                self.db.execute("DELETE FROM runs WHERE id = ?", [id])?;
            }
            Ok(())
        })?;
        let mut freed = 0_i64;
        let mut emptied: Vec<PathBuf> = Vec::new();
        for pointer in stale.iter().filter_map(|(_, pointer)| pointer.as_ref()) {
            let path = PathBuf::from(pointer);
            if let Ok(metadata) = std::fs::metadata(&path) {
                freed += i64::try_from(metadata.len()).unwrap_or(i64::MAX);
            }
            let _ = std::fs::remove_file(&path);
            if let Some(parent) = path.parent()
                && !emptied.iter().any(|known| known == parent)
            {
                emptied.push(parent.to_path_buf());
            }
        }
        for directory in emptied {
            // Not empty yet is fine; a later prune gets it.
            let _ = std::fs::remove_dir(&directory);
            if let Some(parent) = directory.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
        Ok((i64::try_from(stale.len()).unwrap_or(i64::MAX), freed))
    }

    pub fn read_daemon(&self) -> Result<Option<DaemonInfo>> {
        Ok(self
            .db
            .query_row(
                "SELECT pid, version, started_at, heartbeat_at FROM daemon WHERE id = 1",
                [],
                |row| {
                    Ok(DaemonInfo {
                        pid: row.get(0)?,
                        version: row.get(1)?,
                        started_at: row.get(2)?,
                        heartbeat_at: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// The single-instance lock: a daemon takes the row unless another live
    /// daemon holds it. Returns the holder on conflict.
    pub fn claim_daemon(
        &self,
        pid: i64,
        version: &str,
        started_at: i64,
        is_live: impl Fn(&DaemonInfo) -> bool,
    ) -> Result<Option<DaemonInfo>> {
        self.transaction(true, || {
            if let Some(holder) = self.read_daemon()?
                && holder.pid != pid
                && is_live(&holder)
            {
                return Ok(Some(holder));
            }
            self.db.execute(
                "INSERT INTO daemon (id, pid, version, started_at, heartbeat_at)
                 VALUES (1, ?, ?, ?, ?)
                 ON CONFLICT (id) DO UPDATE
                   SET pid = excluded.pid,
                       version = excluded.version,
                       started_at = excluded.started_at,
                       heartbeat_at = excluded.heartbeat_at",
                params![pid, version, started_at, now_ms()],
            )?;
            Ok(None)
        })
    }

    /// Refreshes the holder's heartbeat. False means the lock was taken over.
    pub fn heartbeat(&self, pid: i64) -> Result<bool> {
        let changed = self.db.execute(
            "UPDATE daemon SET heartbeat_at = ? WHERE id = 1 AND pid = ?",
            params![now_ms(), pid],
        )?;
        Ok(changed == 1)
    }

    pub fn clear_daemon(&self, pid: i64) -> Result<()> {
        self.db
            .execute("DELETE FROM daemon WHERE id = 1 AND pid = ?", [pid])?;
        Ok(())
    }

    /// On daemon start: runs whose owner is gone become interrupted (their
    /// process groups are handed back to terminate), and one-shot jobs a
    /// crashed daemon fired but never removed are swept.
    pub fn recover(&self) -> Result<(Recovered, Vec<i64>)> {
        let mut interrupted = 0;
        let mut orphan_groups = Vec::new();
        for run in self.active_runs()? {
            if !is_pid_alive(run.owner_pid) {
                self.finish_run(&run.id, "interrupted", run.gate_exit, run.action_exit)?;
                interrupted += 1;
                if let Some(pgid) = run.pgid {
                    orphan_groups.push(pgid);
                }
            }
        }
        let swept: i64 = self.db.query_row(
            "SELECT COUNT(*) AS total FROM schedules
             WHERE kind = 'once' AND next_fire_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        self.db.execute(
            "DELETE FROM schedules WHERE kind = 'once' AND next_fire_at IS NULL",
            [],
        )?;
        Ok((
            Recovered {
                interrupted,
                swept: usize::try_from(swept).unwrap_or(0),
            },
            orphan_groups,
        ))
    }
}

#[cfg(test)]
pub(crate) mod tests;
