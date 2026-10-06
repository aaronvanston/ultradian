//! Ported from 0.2.1's runner.test.ts, plus the gate contract, the
//! environment, exit codes, spawn failures and the kill grace.

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::AtomicBool;

use super::*;
use crate::store::tests::temp_home;
use crate::store::{NewSchedule, is_pid_alive};
use crate::triggers::Trigger;

/// A store in its own temporary home, removed when dropped.
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

    fn add(
        &self,
        name: &str,
        gate: Option<&str>,
        gate_mode: &str,
        command: &[&str],
        timeout_ms: Option<i64>,
    ) -> Schedule {
        self.store()
            .add_schedule(NewSchedule {
                name: name.into(),
                group: None,
                trigger: Trigger::Manual,
                gate: gate.map(str::to_owned),
                gate_mode: gate_mode.into(),
                command: command.iter().map(|part| (*part).to_owned()).collect(),
                working_directory: self.path.to_string_lossy().into_owned(),
                timeout_ms,
                catch_up_ms: 0,
            })
            .expect("adds")
    }

    fn shell(&self, script: &str, timeout_ms: Option<i64>) -> Schedule {
        self.add(
            "job",
            None,
            "output",
            &["/bin/sh", "-c", script],
            timeout_ms,
        )
    }

    fn fire_with(&self, schedule: &Schedule, shutdown: &AtomicBool) -> Run {
        let run = self
            .store()
            .begin_run(schedule, "manual", false)
            .expect("begins")
            .expect("not busy");
        execute_fire(self.store(), schedule, &run, shutdown).expect("fires")
    }

    fn fire(&self, schedule: &Schedule) -> Run {
        self.fire_with(schedule, &AtomicBool::new(false))
    }

    fn log(run: &Run) -> String {
        std::fs::read_to_string(run.log_pointer.as_deref().expect("has a log")).unwrap_or_default()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        self.store.take();
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn read_pid(path: &Path) -> i64 {
    // The script writes the pid before it blocks, so it is there by now.
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .parse()
        .expect("a pid")
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("exists")
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn a_timeout_terminates_the_whole_group_grandchildren_included() {
    let home = Home::new();
    let pid_file = home.path.join("grandchild.pid");
    let schedule = home.shell(
        &format!("sleep 30 & echo $! > {}; sleep 30", pid_file.display()),
        Some(1000),
    );
    let started = Instant::now();
    let run = home.fire(&schedule);
    assert_eq!(run.status, "timed_out");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!((run.gate_exit, run.action_exit), (None, None));
    let pgid = run.pgid.expect("recorded its group");
    assert!(!group_is_alive(pgid));
    assert!(!is_pid_alive(read_pid(&pid_file)));
    assert!(Home::log(&run).contains("# timed out after 1000ms "));
}

#[test]
fn an_action_that_exits_leaves_nothing_running_and_never_hangs_on_its_pipes() {
    let home = Home::new();
    let pid_file = home.path.join("grandchild.pid");
    let schedule = home.shell(
        &format!("sleep 30 & echo $! > {}; exit 0", pid_file.display()),
        None,
    );
    let started = Instant::now();
    let run = home.fire(&schedule);
    assert_eq!(
        (run.status.as_str(), run.action_exit),
        ("succeeded", Some(0))
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!is_pid_alive(read_pid(&pid_file)));
    assert!(Home::log(&run).contains("# terminating processes left behind "));
}

#[test]
fn a_shutdown_signal_interrupts_the_fire_and_its_group() {
    let home = Home::new();
    let schedule = home.shell("sleep 30", None);
    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&shutdown);
    let setter = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        flag.store(true, Ordering::SeqCst);
    });
    let run = home.fire_with(&schedule, &shutdown);
    setter.join().expect("set");
    assert_eq!(run.status, "interrupted");
    assert!(!group_is_alive(run.pgid.expect("recorded its group")));
    assert!(Home::log(&run).contains("# interrupted "));
}

#[test]
fn a_cancel_written_to_the_store_stops_the_fire_within_a_second_or_so() {
    let home = Home::new();
    let schedule = home.shell("sleep 30", None);
    let run = home
        .store()
        .begin_run(&schedule, "manual", false)
        .expect("begins")
        .expect("not busy");
    let path = home.path.clone();
    let run_id = run.id.clone();
    // Another process's `cancel`: its own connection to the same file.
    let canceler = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        Store::open(&path)
            .expect("opens")
            .cancel_run(&run_id)
            .expect("cancels");
    });
    let started = Instant::now();
    let finished =
        execute_fire(home.store(), &schedule, &run, &AtomicBool::new(false)).expect("fires");
    canceler.join().expect("canceled");
    assert_eq!(finished.status, "canceled");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!group_is_alive(finished.pgid.expect("recorded its group")));
    assert_eq!(
        home.store().cancel_run(&run.id).unwrap_err().code,
        "run_finished"
    );
    assert!(Home::log(&finished).contains("# canceled "));
}

#[test]
fn a_run_log_is_private_and_capped_and_still_records_how_the_run_ended() {
    let home = Home::new();
    let schedule = home.shell("head -c 11000000 /dev/zero", None);
    let run = home.fire(&schedule);
    assert_eq!(run.status, "succeeded");
    let pointer = PathBuf::from(run.log_pointer.clone().expect("has a log"));
    let size = std::fs::metadata(&pointer).expect("exists").len();
    assert!(size < (RUN_LOG_CAP_BYTES + 4096) as u64);
    assert_eq!(mode(&pointer), 0o600);
    let bytes = std::fs::read(&pointer).expect("reads");
    let tail = String::from_utf8_lossy(&bytes[bytes.len() - 400..]).into_owned();
    assert!(tail.contains(&format!(
        "\n# output truncated at {RUN_LOG_CAP_BYTES} bytes; the rest was discarded\n"
    )));
    assert!(tail.contains("# finished exit=0"));
    assert_eq!(mode(&home.path), 0o700);
    assert_eq!(mode(&home.path.join("ultradian.db")), 0o600);
    assert_eq!(mode(pointer.parent().expect("day folder")), 0o700);
}

#[test]
fn the_gate_decides_and_hands_its_output_to_the_action() {
    let home = Home::new();
    let env = "printf '%s|%s|%s\\n' \"$ULTRADIAN_RUN_ID\" \"$ULTRADIAN_SCHEDULE\" \"${ULTRADIAN_SESSION_ID-none}\"";
    let open = home.add(
        "open",
        Some(&format!("{env}; printf 'ctx é'")),
        "output",
        &["/bin/sh", "-c", &format!("cat; echo; {env}")],
        None,
    );
    let run = home.fire(&open);
    assert_eq!(
        (run.status.as_str(), run.gate_exit, run.action_exit),
        ("succeeded", Some(0), Some(0))
    );
    assert_eq!(run.executor.as_deref(), Some("sh"));
    let log = Home::log(&run);
    // The gate sees the run and schedule but no session id; the action sees
    // all three, and its stdin is the gate's whole stdout.
    let gate_line = format!("{}|open|none", run.id);
    let action_line = format!("{0}|open|{0}", run.id);
    assert!(log.contains(&format!("{gate_line}\nctx é")), "{log}");
    assert!(
        log.contains(&format!(
            "# gate open ({} bytes of context) ",
            gate_line.len() + 1 + "ctx é".chars().count()
        )),
        "{log}"
    );
    assert!(
        log.contains(&format!("{gate_line}\nctx é\n{action_line}\n")),
        "{log}"
    );
    assert!(log.starts_with(&format!("# open {}\n# started ", run.id)));
    assert!(log.contains(&format!("# gate: {env}; printf 'ctx é'\n")));
    assert!(log.contains("# action: /bin/sh -c cat; echo; "));
    assert!(log.contains("# finished exit=0 "));

    let clean = home.add(
        "clean",
        Some("printf ' \\n\\t'"),
        "output",
        &["/bin/echo", "no"],
        None,
    );
    let run = home.fire(&clean);
    assert_eq!(
        (
            run.status.as_str(),
            run.gate_exit,
            run.action_exit,
            run.executor.clone()
        ),
        ("clean", Some(0), None, None)
    );
    assert!(Home::log(&run).contains("# gate clean "));

    let exit_mode = home.add(
        "exit-mode",
        Some("true"),
        "exit",
        &["/bin/echo", "yes"],
        None,
    );
    let run = home.fire(&exit_mode);
    assert_eq!(
        (run.status.as_str(), run.action_exit),
        ("succeeded", Some(0))
    );
    assert!(Home::log(&run).contains("# gate open (0 bytes of context) "));

    let failed = home.add(
        "gate-failed",
        Some("echo why >&2; exit 3"),
        "exit",
        &["/bin/echo", "never"],
        None,
    );
    let run = home.fire(&failed);
    assert_eq!(
        (run.status.as_str(), run.gate_exit, run.action_exit),
        ("gate_failed", Some(3), None)
    );
    let log = Home::log(&run);
    assert!(log.contains("why\n# gate failed exit=3 ") && !log.contains("never"));
}

#[test]
fn exit_codes_are_recorded_as_bun_reported_them() {
    let home = Home::new();
    let failing = home.shell("exit 4", None);
    let run = home.fire(&failing);
    assert_eq!((run.status.as_str(), run.action_exit), ("failed", Some(4)));
    home.store().remove_schedule("job").expect("removes");
    // A signal the action brings on itself is 128 plus the signal, not a stop.
    let signaled = home.shell("kill -TERM $$", None);
    let run = home.fire(&signaled);
    assert_eq!(
        (run.status.as_str(), run.action_exit),
        ("failed", Some(143))
    );
    assert!(Home::log(&run).contains("# finished exit=143 "));
}

#[test]
fn a_command_that_cannot_start_fails_the_run_with_bun_s_words() {
    let home = Home::new();
    let missing = home.add(
        "missing",
        None,
        "output",
        &["no-such-program-ultradian"],
        None,
    );
    let run = home.fire(&missing);
    assert_eq!((run.status.as_str(), run.action_exit), ("failed", None));
    assert_eq!(run.executor.as_deref(), Some("no-such-program-ultradian"));
    assert!(
        Home::log(&run)
            .contains("# error Executable not found in $PATH: \"no-such-program-ultradian\"\n")
    );

    let gate_timeout = home.add(
        "slow-gate",
        Some("sleep 30"),
        "output",
        &["/bin/echo", "never"],
        Some(500),
    );
    let run = home.fire(&gate_timeout);
    assert_eq!(
        (run.status.as_str(), run.gate_exit, run.executor.clone()),
        ("timed_out", None, None)
    );
}

#[test]
fn a_group_that_ignores_sigterm_is_killed_after_the_grace() {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "trap '' TERM; while :; do sleep 0.05; done"])
        .process_group(0)
        .spawn()
        .expect("spawns");
    let pgid = i64::from(child.id());
    thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    terminate_group_reaping(pgid, Duration::from_millis(400), || {
        let _ = child.try_wait();
    });
    let _ = child.wait();
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert!(started.elapsed() < Duration::from_secs(3));
    thread::sleep(Duration::from_millis(100));
    assert!(!group_is_alive(pgid));
}

#[test]
fn never_signals_a_bogus_group() {
    assert!(!signal_group(0, 0));
    assert!(!signal_group(1, 0));
    assert!(!signal_group(-5, 0));
}

#[test]
fn reads_output_the_way_textdecoder_does() {
    assert_eq!(decode(b"\xef\xbb\xbfhi"), "hi");
    assert_eq!(decode(b"a\xffb"), "a\u{fffd}b");
    assert_eq!(js_trim("\u{feff} \n"), "");
    assert_eq!(exit_code(ExitStatus::from_raw(9)), 137);
}
