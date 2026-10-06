//! The foreground daemon loop: heartbeat, claim queued and due fires, run
//! them concurrently, sweep stalled runs, prune by retention. Named run_loop
//! because `loop` is a Rust keyword.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::daemon_is_live;
use crate::errors::{AppError, exit};
use crate::output::{iso_ms, now_ms};
use crate::runner::{KILL_GRACE, execute_fire, group_is_alive, terminate_group};
use crate::store::{Run, Schedule, Store, is_pid_alive};

const DAEMON_LOG_ROTATE_BYTES: usize = 5_000_000;
const DAEMON_LOG_KEEP: u32 = 3;
const TICK: Duration = Duration::from_secs(1);
/// A third of HEARTBEAT_STALE_MS, so a tick slowed by a busy store still
/// beats in time.
const HEARTBEAT_EVERY_MS: i64 = 5_000;
const STALL_SWEEP_MS: i64 = 30_000;
/// How long a stalled run must stay that way before it is reaped.
const STALL_CONFIRM_MS: i64 = 60_000;
const RETENTION_SWEEP_MS: i64 = 86_400_000;

/// A line logger shared by the loop and its fires.
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

struct DaemonLog {
    file: PathBuf,
    size: usize,
    mirror: bool,
}

/// The daemon owns its log and rotates it by size, so no supervisor's open
/// descriptor ever points at a rotated file: daemon.log holds lifecycle
/// lines, daemon.log.1 to .3 older ones. Mirrors to stderr when a person is
/// watching a foreground daemon.
pub fn open_daemon_log(home: &Path, mirror: bool) -> Log {
    let file = home.join("daemon.log");
    let size =
        std::fs::metadata(&file).map_or(0, |metadata| usize::try_from(metadata.len()).unwrap_or(0));
    let state = Mutex::new(DaemonLog { file, size, mirror });
    Arc::new(move |line: &str| {
        let Ok(mut state) = state.lock() else { return };
        let text = format!("{} {line}\n", iso_ms(now_ms()));
        // 0.2.1 counted JavaScript string length, UTF-16 units.
        let length = text.encode_utf16().count();
        if state.size + length > DAEMON_LOG_ROTATE_BYTES {
            let numbered = |index: u32| PathBuf::from(format!("{}.{index}", state.file.display()));
            for index in (1..DAEMON_LOG_KEEP).rev() {
                if numbered(index).exists() {
                    let _ = std::fs::rename(numbered(index), numbered(index + 1));
                }
            }
            if state.file.exists() {
                let _ = std::fs::rename(&state.file, numbered(1));
            }
            state.size = 0;
        }
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&state.file)
        {
            let _ = file.write_all(text.as_bytes());
        }
        state.size += length;
        if state.mirror {
            let _ = std::io::stderr().write_all(text.as_bytes());
        }
    })
}

/// Reaps active runs whose process evidence says they can never finish:
/// the owning process is gone (a killed manual run), or the child the run
/// waits on is gone while its owner survives. A run is reaped only after a
/// second sweep confirms it stayed that way past STALL_CONFIRM_MS, so a run
/// finishing normally is never raced. Runs with no recorded child stay
/// untouched unless their owner dies.
pub fn sweep_stalled_runs(
    store: &Store,
    suspects: &mut HashMap<String, i64>,
    now: i64,
    daemon_pid: i64,
    log: &Log,
) -> Result<usize, AppError> {
    let active = store.active_runs()?;
    suspects.retain(|id, _| active.iter().any(|run| &run.id == id));
    let mut reaped = 0;
    for run in active {
        let owner_gone = run.owner_pid != daemon_pid && !is_pid_alive(run.owner_pid);
        let child_gone = run.pgid.is_some_and(|pgid| !is_pid_alive(pgid));
        if !owner_gone && !child_gone {
            suspects.remove(&run.id);
            continue;
        }
        let Some(first_seen) = suspects.get(&run.id).copied() else {
            suspects.insert(run.id.clone(), now);
            continue;
        };
        if now - first_seen < STALL_CONFIRM_MS {
            continue;
        }
        store.finish_run(&run.id, "interrupted", run.gate_exit, run.action_exit)?;
        suspects.remove(&run.id);
        reaped += 1;
        log(&format!(
            "reap {} {}: {} pid gone",
            run.schedule_name,
            run.id,
            if owner_gone { "owner" } else { "child" }
        ));
    }
    Ok(reaped)
}

pub struct LoopOptions<'a> {
    pub store: &'a Store,
    pub stop: &'a AtomicBool,
    pub version: &'a str,
    pub retention_ms: Option<i64>,
    pub log: Log,
}

/// A fire running on its own thread with its own connection to the store.
fn launch(
    home: PathBuf,
    schedule: Schedule,
    run: Run,
    shutdown: Arc<AtomicBool>,
    log: Log,
) -> JoinHandle<()> {
    log(&format!(
        "fire {} {} trigger={}",
        schedule.name, run.id, run.trigger
    ));
    thread::spawn(move || {
        let store = match Store::open(&home) {
            Ok(store) => store,
            Err(error) => {
                log(&format!(
                    "fire {} failed: AppError: {}",
                    schedule.name, error.message
                ));
                return;
            }
        };
        match execute_fire(&store, &schedule, &run, &shutdown) {
            Ok(finished) => log(&format!(
                "done {} {} status={}",
                schedule.name, run.id, finished.status
            )),
            Err(error) => log(&format!(
                "fire {} failed: AppError: {}",
                schedule.name, error.message
            )),
        }
        // The job row is consumed by its one fire; the run record survives.
        if schedule.kind == "once" {
            let _ = store.remove_job(&schedule.id);
        }
    })
}

/// Sleeps one tick in a single wait that a signal cuts short, so an idle
/// daemon wakes once a tick rather than polling `stop`, and SIGTERM still
/// stops it at once. (std's sleep resumes after a signal.)
fn sleep_until_signal(duration: Duration) {
    let request = libc::timespec {
        tv_sec: libc::time_t::try_from(duration.as_secs()).unwrap_or(1),
        tv_nsec: libc::c_long::from(duration.subsec_nanos()),
    };
    // SAFETY: nanosleep only reads `request`; the remainder isn't wanted.
    unsafe { libc::nanosleep(&request, std::ptr::null_mut()) };
}

/// Runs until `stop` is set or another daemon takes the lock. Fires run
/// concurrently; a schedule with a run still in flight is skipped and the
/// skip recorded. On shutdown every fire in flight has its process group
/// stopped and is recorded as interrupted.
pub fn run_daemon_loop(options: LoopOptions) -> Result<(), AppError> {
    let LoopOptions {
        store,
        stop,
        version,
        retention_ms,
        log,
    } = options;
    let pid = i64::from(std::process::id());
    let started_at = now_ms();
    if let Some(holder) = store.claim_daemon(pid, version, started_at, |holder| {
        daemon_is_live(Some(holder))
    })? {
        return Err(AppError::new(
            "daemon_already_running",
            format!("A daemon is already running (pid {}).", holder.pid),
        )
        .exit(exit::TEMPFAIL)
        .hint("Stop it with 'daemon stop' first."));
    }

    let (recovered, orphan_groups) = store.recover()?;
    for pgid in orphan_groups {
        if group_is_alive(pgid) {
            log(&format!("terminating orphaned process group {pgid}"));
            // 0.2.1 didn't wait for this either; the loop starts at once.
            thread::spawn(move || terminate_group(pgid, KILL_GRACE));
        }
    }
    log(&format!(
        "daemon started pid={pid} version={version} interrupted={} swept={}",
        recovered.interrupted, recovered.swept
    ));

    let shutdown = Arc::new(AtomicBool::new(false));
    let mut inflight: Vec<JoinHandle<()>> = Vec::new();
    let mut suspects: HashMap<String, i64> = HashMap::new();
    let mut last_sweep_at = started_at;
    let mut last_prune_at = 0_i64;
    let mut seen_version: Option<i64> = None;
    let mut next_due: Option<i64> = None;

    let mut last_beat_at = started_at;

    let mut tick = |inflight: &mut Vec<JoinHandle<()>>| -> Result<bool, AppError> {
        // Between ticks, only another process (a CLI command, a fire's own
        // connection, a rival daemon) can change the store, and any commit
        // changes the data version. A rival can only take the lock that
        // way, so the lock is checked on every change; otherwise the
        // heartbeat is refreshed every HEARTBEAT_EVERY_MS, well inside the
        // stale window.
        let version = store.data_version()?;
        if seen_version != Some(version) || now_ms() - last_beat_at >= HEARTBEAT_EVERY_MS {
            if !store.heartbeat(pid)? {
                log("another daemon took over the lock; stopping");
                return Ok(false);
            }
            last_beat_at = now_ms();
        }
        if now_ms() - last_sweep_at >= STALL_SWEEP_MS {
            last_sweep_at = now_ms();
            sweep_stalled_runs(store, &mut suspects, last_sweep_at, pid, &log)?;
        }
        if let Some(retention) = retention_ms
            && now_ms() - last_prune_at >= RETENTION_SWEEP_MS
        {
            last_prune_at = now_ms();
            let (removed, freed) = store.prune_history(last_prune_at - retention, None)?;
            if removed > 0 {
                log(&format!(
                    "pruned {removed} run(s) older than {retention}ms, freed {freed} bytes"
                ));
            }
        }
        // Without a change, nothing is due before the earliest next fire,
        // so the claims are skipped.
        let now = now_ms();
        if seen_version == Some(version) && next_due.is_none_or(|due| due > now) {
            return Ok(true);
        }
        for (run, schedule) in store.claim_queued(pid)? {
            inflight.push(launch(
                store.home.clone(),
                schedule,
                run,
                Arc::clone(&shutdown),
                Arc::clone(&log),
            ));
        }
        let (due, missed) = store.claim_due(now_ms())?;
        for schedule in missed {
            log(&format!(
                "missed {}: beyond its catch-up window, skipped forward",
                schedule.name
            ));
        }
        for schedule in due {
            let trigger = if schedule.kind == "once" {
                "once"
            } else {
                "scheduled"
            };
            match store.begin_run(&schedule, trigger, false)? {
                Some(run) => {
                    inflight.push(launch(
                        store.home.clone(),
                        schedule,
                        run,
                        Arc::clone(&shutdown),
                        Arc::clone(&log),
                    ));
                }
                None => {
                    store.record_run(&schedule, trigger, "skipped")?;
                    log(&format!(
                        "skip {}: previous run still in flight",
                        schedule.name
                    ));
                }
            }
        }
        next_due = store.next_due_at()?;
        seen_version = Some(version);
        Ok(true)
    };

    let mut holding = true;
    while !stop.load(Ordering::SeqCst) && holding {
        match tick(&mut inflight) {
            Ok(still_holding) => holding = still_holding,
            Err(error) => log(&format!("tick failed: {}", error.message)),
        }
        inflight.retain(|fire| !fire.is_finished());
        if !stop.load(Ordering::SeqCst) {
            sleep_until_signal(TICK);
        }
    }

    inflight.retain(|fire| !fire.is_finished());
    if !inflight.is_empty() {
        log(&format!(
            "shutting down, interrupting {} run(s)",
            inflight.len()
        ));
        shutdown.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + KILL_GRACE + Duration::from_secs(5);
        while Instant::now() < deadline && inflight.iter().any(|fire| !fire.is_finished()) {
            thread::sleep(Duration::from_millis(20));
        }
    }
    if holding {
        store.clear_daemon(pid)?;
    }
    log("daemon stopped");
    Ok(())
}

#[cfg(test)]
mod tests;
