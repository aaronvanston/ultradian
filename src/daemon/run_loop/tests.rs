//! Ported from 0.2.1's daemon.test.ts (the service-file and PATH tests
//! live beside the renderers), with the loop's timing shortened.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::Command;

use super::*;
use crate::runner::group_is_alive;
use crate::store::NewSchedule;
use crate::store::tests::temp_home;
use crate::triggers::Trigger;

struct Home {
    path: PathBuf,
    store: Option<Store>,
}

impl Home {
    fn new() -> Self {
        let path = temp_home();
        let store = Store::open(&path).expect("opens");
        Self {
            path,
            store: Some(store),
        }
    }

    fn store(&self) -> &Store {
        self.store.as_ref().expect("open")
    }

    fn add(&self, name: &str, command: &[&str], trigger: Trigger) -> Schedule {
        self.store()
            .add_schedule(NewSchedule {
                name: name.into(),
                group: None,
                trigger,
                gate: None,
                gate_mode: "output".into(),
                command: command.iter().map(|part| (*part).to_owned()).collect(),
                working_directory: "/tmp".into(),
                timeout_ms: None,
                catch_up_ms: 0,
            })
            .expect("adds")
    }

    fn begin(&self, schedule: &Schedule, trigger: &str) -> Run {
        self.store()
            .begin_run(schedule, trigger, false)
            .expect("begins")
            .expect("not busy")
    }

    fn raw(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.path.join("ultradian.db")).expect("raw")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        self.store.take();
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn quiet() -> Log {
    Arc::new(|_: &str| {})
}

/// The pid of a process that has come and gone (this test's own child).
fn dead_pid() -> i64 {
    let mut child = Command::new("true").spawn().expect("spawns");
    let _ = child.wait();
    i64::from(child.id())
}

fn me() -> i64 {
    i64::from(std::process::id())
}

fn sweep(home: &Home, suspects: &mut HashMap<String, i64>, now: i64) -> usize {
    sweep_stalled_runs(home.store(), suspects, now, 60_000, me(), &quiet()).expect("sweeps")
}

#[test]
fn reaps_a_run_whose_child_died_but_only_after_a_confirming_sweep() {
    let home = Home::new();
    let schedule = home.add("tick", &["echo", "ok"], Trigger::Every { seconds: 1 });
    let run = home.begin(&schedule, "scheduled");
    home.store()
        .set_run_process_group(&run.id, dead_pid())
        .expect("sets");
    let mut suspects = HashMap::new();
    let base = now_ms();
    assert_eq!(sweep(&home, &mut suspects, base), 0);
    assert_eq!(
        home.store()
            .get_run(&run.id)
            .expect("reads")
            .expect("kept")
            .status,
        "running"
    );
    assert_eq!(sweep(&home, &mut suspects, base + 30_000), 0);
    assert_eq!(sweep(&home, &mut suspects, base + 61_000), 1);
    let reaped = home.store().get_run(&run.id).expect("reads").expect("kept");
    assert_eq!(reaped.status, "interrupted");
    assert!(reaped.finished_at.is_some());
    assert!(!home.store().has_active_run(&schedule.id).expect("checks"));
}

#[test]
fn a_live_child_is_never_suspected() {
    let home = Home::new();
    let schedule = home.add("tick", &["echo", "ok"], Trigger::Every { seconds: 1 });
    let run = home.begin(&schedule, "scheduled");
    home.store()
        .set_run_process_group(&run.id, me())
        .expect("sets");
    let mut suspects = HashMap::new();
    let base = now_ms();
    for offset in [0, 61_000, 122_000] {
        sweep(&home, &mut suspects, base + offset);
    }
    assert_eq!(
        home.store()
            .get_run(&run.id)
            .expect("reads")
            .expect("kept")
            .status,
        "running"
    );
    assert!(suspects.is_empty());
}

#[test]
fn a_run_with_no_recorded_child_owned_by_the_live_daemon_stays_untouched() {
    let home = Home::new();
    let schedule = home.add("tick", &["echo", "ok"], Trigger::Every { seconds: 1 });
    let run = home.begin(&schedule, "scheduled");
    let mut suspects = HashMap::new();
    let base = now_ms();
    for offset in [0, 61_000] {
        sweep(&home, &mut suspects, base + offset);
    }
    assert_eq!(
        home.store()
            .get_run(&run.id)
            .expect("reads")
            .expect("kept")
            .status,
        "running"
    );
}

#[test]
fn a_suspect_that_finishes_normally_between_sweeps_is_cleared_never_reaped() {
    let home = Home::new();
    let schedule = home.add("tick", &["echo", "ok"], Trigger::Every { seconds: 1 });
    let run = home.begin(&schedule, "scheduled");
    home.store()
        .set_run_process_group(&run.id, dead_pid())
        .expect("sets");
    let mut suspects = HashMap::new();
    let base = now_ms();
    sweep(&home, &mut suspects, base);
    assert_eq!(suspects.len(), 1);
    home.store()
        .finish_run(&run.id, "succeeded", None, Some(0))
        .expect("finishes");
    sweep(&home, &mut suspects, base + 61_000);
    assert_eq!(
        home.store()
            .get_run(&run.id)
            .expect("reads")
            .expect("kept")
            .status,
        "succeeded"
    );
    assert!(suspects.is_empty());
}

#[test]
fn reaps_a_manual_run_whose_owning_process_died() {
    let home = Home::new();
    let schedule = home.add("tick", &["echo", "ok"], Trigger::Every { seconds: 1 });
    let run = home.begin(&schedule, "manual");
    home.raw()
        .execute(
            "UPDATE runs SET owner_pid = ? WHERE id = ?",
            rusqlite::params![dead_pid(), run.id],
        )
        .expect("orphans");
    let mut suspects = HashMap::new();
    let base = now_ms();
    sweep(&home, &mut suspects, base);
    assert_eq!(sweep(&home, &mut suspects, base + 61_000), 1);
    assert_eq!(
        home.store()
            .get_run(&run.id)
            .expect("reads")
            .expect("kept")
            .status,
        "interrupted"
    );
}

#[test]
fn a_reaped_run_keeps_its_status_when_the_wedged_finish_arrives_late() {
    let home = Home::new();
    let schedule = home.add("tick", &["echo", "ok"], Trigger::Every { seconds: 1 });
    let run = home.begin(&schedule, "scheduled");
    home.store()
        .set_run_process_group(&run.id, dead_pid())
        .expect("sets");
    let mut suspects = HashMap::new();
    let base = now_ms();
    for offset in [0, 61_000] {
        sweep(&home, &mut suspects, base + offset);
    }
    assert_eq!(
        home.store()
            .get_run(&run.id)
            .expect("reads")
            .expect("kept")
            .status,
        "interrupted"
    );
    // The wedged fire finally returns and tries to record success.
    home.store()
        .finish_run(&run.id, "succeeded", None, Some(0))
        .expect("finishes");
    assert_eq!(
        home.store()
            .get_run(&run.id)
            .expect("reads")
            .expect("kept")
            .status,
        "interrupted"
    );
}

#[test]
fn only_one_daemon_holds_the_lock_and_a_displaced_one_learns_it() {
    let home = Home::new();
    let store = home.store();
    let live = |holder: &crate::store::DaemonInfo| daemon_is_live(Some(holder));
    assert_eq!(
        store
            .claim_daemon(me(), "test", now_ms(), live)
            .expect("claims"),
        None
    );
    let rival = i64::from(std::os::unix::process::parent_id());
    assert_eq!(
        store
            .claim_daemon(rival, "test", now_ms(), live)
            .expect("claims")
            .map(|holder| holder.pid),
        Some(me())
    );
    // A holder whose heartbeat went stale can be displaced, and then its own
    // heartbeat fails, which is its cue to stop.
    assert_eq!(
        store
            .claim_daemon(rival, "test", now_ms(), |_| false)
            .expect("claims"),
        None
    );
    assert!(!store.heartbeat(me()).expect("beats"));
    assert!(store.heartbeat(rival).expect("beats"));
}

fn fast() -> Timing {
    Timing {
        tick: Duration::from_millis(50),
        ..Timing::default()
    }
}

/// Runs the loop on its own thread and connection until `stop` is set.
fn spawn_loop(
    path: PathBuf,
    stop: Arc<AtomicBool>,
    retention_ms: Option<i64>,
) -> JoinHandle<Result<(), AppError>> {
    thread::spawn(move || {
        let store = Store::open(&path).expect("opens");
        run_daemon_loop(LoopOptions {
            store: &store,
            stop: &stop,
            version: "test",
            retention_ms,
            log: quiet(),
            timing: fast(),
        })
    })
}

fn stop_after(stop: &Arc<AtomicBool>, delay: Duration) {
    let stop = Arc::clone(stop);
    thread::spawn(move || {
        thread::sleep(delay);
        stop.store(true, Ordering::SeqCst);
    });
}

#[test]
fn recovery_terminates_the_process_group_a_dead_daemon_left_running() {
    let home = Home::new();
    let schedule = home.add("orphaned", &["sleep", "30"], Trigger::Manual);
    let run = home.begin(&schedule, "scheduled");
    let mut leftover = Command::new("sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .expect("spawns");
    let pgid = i64::from(leftover.id());
    home.store()
        .set_run_process_group(&run.id, pgid)
        .expect("sets");
    home.raw()
        .execute(
            "UPDATE runs SET owner_pid = ? WHERE id = ?",
            rusqlite::params![dead_pid(), run.id],
        )
        .expect("orphans");
    let stop = Arc::new(AtomicBool::new(false));
    stop_after(&stop, Duration::from_millis(1500));
    spawn_loop(home.path.clone(), Arc::clone(&stop), None)
        .join()
        .expect("joins")
        .expect("runs");
    assert_eq!(
        home.store()
            .get_run(&run.id)
            .expect("reads")
            .expect("kept")
            .status,
        "interrupted"
    );
    let _ = leftover.wait();
    assert!(!group_is_alive(pgid));
}

#[test]
fn shutdown_interrupts_runs_in_flight_and_leaves_no_processes_behind() {
    let home = Home::new();
    home.add("long", &["sleep", "30"], Trigger::Every { seconds: 1 });
    let stop = Arc::new(AtomicBool::new(false));
    let running = spawn_loop(home.path.clone(), Arc::clone(&stop), None);
    let deadline = Instant::now() + Duration::from_secs(5);
    let in_flight = loop {
        let active = home.store().active_runs().expect("lists");
        if let Some(run) = active.into_iter().find(|run| run.pgid.is_some()) {
            break run;
        }
        assert!(Instant::now() < deadline, "the daemon never fired");
        thread::sleep(Duration::from_millis(50));
    };
    stop.store(true, Ordering::SeqCst);
    running.join().expect("joins").expect("runs");
    assert_eq!(
        home.store()
            .get_run(&in_flight.id)
            .expect("reads")
            .expect("kept")
            .status,
        "interrupted"
    );
    assert!(!group_is_alive(in_flight.pgid.expect("has a group")));
    assert_eq!(home.store().read_daemon().expect("reads"), None);
}

#[test]
fn the_daemon_prunes_history_past_its_retention() {
    let home = Home::new();
    let schedule = home.add("old", &["true"], Trigger::Manual);
    let stale = home.begin(&schedule, "manual");
    home.store()
        .finish_run(&stale.id, "succeeded", None, Some(0))
        .expect("finishes");
    let fresh = home.begin(&schedule, "manual");
    home.store()
        .finish_run(&fresh.id, "succeeded", None, Some(0))
        .expect("finishes");
    home.raw()
        .execute(
            "UPDATE runs SET started_at = ? WHERE id = ?",
            rusqlite::params![now_ms() - 40 * 86_400_000, stale.id],
        )
        .expect("ages");
    let stop = Arc::new(AtomicBool::new(false));
    stop_after(&stop, Duration::from_millis(500));
    spawn_loop(home.path.clone(), Arc::clone(&stop), Some(30 * 86_400_000))
        .join()
        .expect("joins")
        .expect("runs");
    assert_eq!(home.store().get_run(&stale.id).expect("reads"), None);
    assert_eq!(
        home.store()
            .get_run(&fresh.id)
            .expect("reads")
            .expect("kept")
            .status,
        "succeeded"
    );
}

#[test]
fn a_second_daemon_refuses_while_the_first_is_live_and_overlaps_are_skipped() {
    let home = Home::new();
    let schedule = home.add("busy", &["sleep", "30"], Trigger::Every { seconds: 1 });
    // A run already in flight under this (live) process: every fire the
    // daemon claims for the schedule is recorded as skipped instead.
    home.begin(&schedule, "manual");
    let stop = Arc::new(AtomicBool::new(false));
    stop_after(&stop, Duration::from_millis(2600));
    spawn_loop(home.path.clone(), Arc::clone(&stop), None)
        .join()
        .expect("joins")
        .expect("runs");
    let runs = home.store().list_runs(Some("busy"), 10).expect("lists");
    assert!(
        runs.iter()
            .any(|run| run.status == "skipped" && run.trigger == "scheduled")
    );
    home.raw()
        .execute(
            "INSERT INTO daemon (id, pid, version, started_at, heartbeat_at) VALUES (1, ?, 'x', 0, ?)",
            rusqlite::params![i64::from(std::os::unix::process::parent_id()), now_ms()],
        )
        .expect("a live rival holds the lock");
    // The rival is this test's parent process: alive, beating, not us.
    let refused = run_daemon_loop(LoopOptions {
        store: home.store(),
        stop: &AtomicBool::new(true),
        version: "test",
        retention_ms: None,
        log: quiet(),
        timing: fast(),
    })
    .unwrap_err();
    assert_eq!(
        (refused.code.as_str(), refused.exit_code),
        ("daemon_already_running", 75)
    );
}

#[test]
fn the_daemon_log_rotates_by_size_and_keeps_three_old_files() {
    let home = Home::new();
    let log = open_daemon_log(&home.path, false);
    let line = "x".repeat(100_000);
    for _ in 0..260 {
        log(&line);
    }
    let file = home.path.join("daemon.log");
    assert!(std::fs::metadata(&file).expect("exists").len() <= 5_000_000);
    assert_eq!(
        std::fs::metadata(&file)
            .expect("exists")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    for suffix in [".1", ".2", ".3"] {
        assert!(PathBuf::from(format!("{}{suffix}", file.display())).exists());
    }
    assert!(!PathBuf::from(format!("{}.4", file.display())).exists());
}
