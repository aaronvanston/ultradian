//! The scheduling daemon: its loop, its lifecycle commands, and the
//! launchd and systemd service files that supervise it.

pub mod control;
pub mod run_loop;
pub mod service;

use crate::output::now_ms;
use crate::store::{DaemonInfo, is_pid_alive};

/// A heartbeat older than this means the daemon is gone.
pub const HEARTBEAT_STALE_MS: i64 = 15_000;

/// The lock row names a process that exists and has beaten recently.
pub fn daemon_is_live(info: Option<&DaemonInfo>) -> bool {
    info.is_some_and(|info| {
        is_pid_alive(info.pid) && now_ms() - info.heartbeat_at < HEARTBEAT_STALE_MS
    })
}
