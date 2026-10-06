//! Table definitions and the ordered migrations that bring a database to
//! user_version 1, the only version this release writes. The SQL text is
//! 0.2.1's byte for byte, so sqlite_master reads the same whichever build
//! created the file.

use rusqlite::Connection;

/// Step 1, from user_version 0 to 1. Steps are append-only once released:
/// a change to the shape is a new step at the end, never an edit.
pub fn create_tables(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "
      CREATE TABLE schedules (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL UNIQUE,
        kind TEXT NOT NULL DEFAULT 'schedule',
        schedule_group TEXT,
        trigger_kind TEXT NOT NULL,
        trigger_value TEXT,
        timezone TEXT,
        gate TEXT,
        gate_mode TEXT NOT NULL DEFAULT 'output',
        command TEXT NOT NULL,
        working_directory TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'active',
        timeout_ms INTEGER,
        catch_up_ms INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        next_fire_at INTEGER
      )",
    )?;
    db.execute_batch(
        "
      CREATE TABLE runs (
        id TEXT PRIMARY KEY,
        schedule_id TEXT NOT NULL,
        schedule_name TEXT NOT NULL,
        machine_id TEXT NOT NULL,
        working_directory TEXT,
        executor TEXT,
        trigger TEXT NOT NULL,
        status TEXT NOT NULL,
        gate_exit INTEGER,
        action_exit INTEGER,
        started_at INTEGER NOT NULL,
        finished_at INTEGER,
        log_pointer TEXT,
        owner_pid INTEGER NOT NULL,
        pgid INTEGER,
        revision INTEGER NOT NULL DEFAULT 0
      )",
    )?;
    db.execute_batch("CREATE INDEX runs_by_schedule ON runs (schedule_name, started_at DESC)")?;
    // Every insert or change to a run stamps it with the next value of one
    // counter, inside the writing transaction. Writers are serialized, so
    // revisions commit in order and 'runs --since <revision>' can never miss
    // a change that commits after a reader saw a higher one.
    db.execute_batch("CREATE TABLE counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL)")?;
    db.execute_batch("INSERT INTO counters (name, value) VALUES ('runs', 0)")?;
    db.execute_batch("CREATE INDEX runs_by_revision ON runs (revision)")?;
    for event in ["INSERT", "UPDATE"] {
        db.execute_batch(&format!(
            "
        CREATE TRIGGER runs_revision_{}
        AFTER {event} ON runs
        BEGIN
          UPDATE counters SET value = value + 1 WHERE name = 'runs';
          UPDATE runs SET revision = (SELECT value FROM counters WHERE name = 'runs')
          WHERE rowid = NEW.rowid;
        END",
            event.to_lowercase()
        ))?;
    }
    db.execute_batch(
        "
      CREATE TABLE daemon (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        pid INTEGER NOT NULL,
        version TEXT NOT NULL,
        started_at INTEGER NOT NULL,
        heartbeat_at INTEGER NOT NULL
      )",
    )
}

/// The steps, in order; the schema version is how many there are.
pub const MIGRATIONS: [fn(&Connection) -> rusqlite::Result<()>; 1] = [create_tables];

pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;
