//! The SQLite store at `$ULTRADIAN_HOME/ultradian.db`, the only channel
//! between the CLI and the daemon: schedules, runs, revision counters and
//! the daemon lock. (Phase 2.)

mod legacy;
mod schema;
