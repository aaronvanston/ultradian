//! Carrying a 0.1 database (user_version 0, before schemas were versioned)
//! into the first versioned shape in place, schedules, run history and
//! daemon lock included, exactly as 0.2.1's upgradeLegacy did.

use std::collections::HashSet;

use rusqlite::Connection;

pub fn table_columns(db: &Connection, table: &str) -> rusqlite::Result<HashSet<String>> {
    let mut statement = db.prepare("SELECT name FROM pragma_table_info(?)")?;
    let names = statement.query_map([table], |row| row.get::<_, String>(0))?;
    names.collect()
}

/// A column the table has, or a fallback expression when it lacks one.
fn column_or<'a>(columns: &HashSet<String>, name: &'a str, fallback: &'a str) -> &'a str {
    if columns.contains(name) {
        name
    } else {
        fallback
    }
}

/// The old tables step aside, step 1 builds the new ones, rows are copied
/// across with defaults for what the old shape lacked, and the old tables
/// go. Runs inside the caller's immediate transaction.
pub fn upgrade(db: &Connection) -> rusqlite::Result<()> {
    let schedules = table_columns(db, "schedules")?;
    let runs = table_columns(db, "runs")?;
    let daemon = table_columns(db, "daemon")?;
    db.execute_batch("DROP INDEX IF EXISTS runs_by_schedule")?;
    db.execute_batch("ALTER TABLE schedules RENAME TO legacy_schedules")?;
    if !runs.is_empty() {
        db.execute_batch("ALTER TABLE runs RENAME TO legacy_runs")?;
    }
    if !daemon.is_empty() {
        db.execute_batch("ALTER TABLE daemon RENAME TO legacy_daemon")?;
    }
    // 0.1 kept an interval as the duration typed ('15m'); now it is seconds.
    let interval_seconds = "CAST(substr(trigger_value, 1, length(trigger_value) - 1) AS INTEGER) *
    CASE substr(trigger_value, -1) WHEN 'd' THEN 86400 WHEN 'h' THEN 3600
      WHEN 'm' THEN 60 ELSE 1 END";
    super::schema::create_tables(db)?;
    db.execute_batch(&format!(
        "
    INSERT INTO schedules
      (id, name, kind, schedule_group, trigger_kind, trigger_value, gate,
       command, working_directory, status, timeout_ms, created_at,
       updated_at, next_fire_at)
    SELECT id, name, {kind},
      {group},
      CASE trigger_kind WHEN 'interval' THEN 'every' ELSE trigger_kind END,
      CASE trigger_kind WHEN 'interval' THEN {interval_seconds}
        ELSE trigger_value END,
      gate, command, working_directory, status,
      {timeout}, created_at, created_at,
      next_fire_at
    FROM legacy_schedules",
        kind = column_or(&schedules, "kind", "'schedule'"),
        group = column_or(&schedules, "schedule_group", "NULL"),
        timeout = column_or(&schedules, "timeout_ms", "NULL"),
    ))?;
    if !runs.is_empty() {
        // Copied oldest first, so the revision triggers number history in the
        // order it happened and a follower starting from zero reads it in
        // order.
        let working_directory = if runs.contains("working_directory") {
            "working_directory"
        } else {
            "(SELECT working_directory FROM legacy_schedules s WHERE s.id = r.schedule_id)"
        };
        db.execute_batch(&format!(
            "
      INSERT INTO runs
        (id, schedule_id, schedule_name, machine_id, working_directory,
         executor, trigger, status, gate_exit, action_exit, started_at,
         finished_at, log_pointer, owner_pid)
      SELECT id, schedule_id, schedule_name, machine_id, {working_directory},
        executor, trigger, status, gate_exit, action_exit, started_at,
        finished_at, log_pointer, owner_pid
      FROM legacy_runs r ORDER BY started_at, rowid"
        ))?;
        db.execute_batch("DROP TABLE legacy_runs")?;
    }
    if !daemon.is_empty() {
        // A 0.1 daemon may still be running. Keeping its row keeps the
        // single-instance lock on it, so a newer daemon refuses to start
        // beside it and 'daemon stop' or 'restart' can still reach it.
        db.execute_batch(
            "
      INSERT INTO daemon (id, pid, version, started_at, heartbeat_at)
      SELECT id, pid, '0.1', started_at, heartbeat_at FROM legacy_daemon",
        )?;
        db.execute_batch("DROP TABLE legacy_daemon")?;
    }
    db.execute_batch("DROP TABLE legacy_schedules")
}
