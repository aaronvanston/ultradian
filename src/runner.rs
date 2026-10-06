//! One fire of a schedule, as 0.2.1's runner.ts ran it: the gate under
//! `/bin/sh -c`, then the action as argv, each leading its own session and
//! process group; the gate's stdout piped to the action's stdin; one
//! timeout across both; a cancel noticed by reading the store once a
//! second; SIGTERM to the group, SIGKILL ten seconds later; and a captured
//! log with marker lines that always land.
//!
//! The fire runs on the calling thread, which polls the child, the clock,
//! the store and the shutdown flag; two reader threads per child drain its
//! stdout and stderr into the log so a chatty process never blocks.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::errors::AppError;
use crate::output::{iso_ms, now_ms};
use crate::store::{Run, Schedule, Store};

/// How long a process group gets between SIGTERM and SIGKILL.
pub const KILL_GRACE: Duration = Duration::from_secs(10);
/// How often the owner of a fire reads the store for a cancellation.
const CANCEL_POLL_MS: i64 = 1000;
/// A run log keeps at most this much captured output. Past it the output
/// is still drained, so the process never blocks, but it is dropped.
pub const RUN_LOG_CAP_BYTES: usize = 10_000_000;
/// How often the fire's loop looks at its child, clock and flags.
const TICK: Duration = Duration::from_millis(20);

fn stamp() -> String {
    iso_ms(now_ms())
}

/// Sends a signal to a whole process group. True when the group exists
/// (EPERM counts: it exists, it just isn't ours to signal).
fn signal_group(pgid: i64, signal: libc::c_int) -> bool {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return false;
    };
    if pgid <= 1 {
        // Never let a missing or bogus pgid turn into a signal to everyone.
        return false;
    }
    // SAFETY: kill with a negative pid signals that process group only.
    if unsafe { libc::kill(-pgid, signal) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn group_is_alive(pgid: i64) -> bool {
    signal_group(pgid, 0)
}

/// SIGTERM the group, then SIGKILL whatever is left after the grace.
/// `reap` runs on each poll so a leader that has exited is collected
/// rather than counted alive as a zombie.
fn terminate_group_reaping(pgid: i64, grace: Duration, mut reap: impl FnMut()) {
    if !signal_group(pgid, libc::SIGTERM) {
        return;
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        reap();
        if !group_is_alive(pgid) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    signal_group(pgid, libc::SIGKILL);
    reap();
}

/// Stops a group this process doesn't parent, such as one a dead daemon
/// left behind.
pub fn terminate_group(pgid: i64, grace: Duration) {
    terminate_group_reaping(pgid, grace, || {});
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Timeout,
    Canceled,
    Shutdown,
}

/// The run log: output up to the cap, then a truncation marker, and marker
/// lines whatever the cap.
struct RunLog {
    file: File,
    captured: usize,
    truncated: bool,
}

impl RunLog {
    fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        Ok(Self {
            file,
            captured: 0,
            truncated: false,
        })
    }

    fn write(&mut self, chunk: &[u8]) {
        if self.truncated {
            return;
        }
        let room = RUN_LOG_CAP_BYTES - self.captured;
        if chunk.len() <= room {
            let _ = self.file.write_all(chunk);
            self.captured += chunk.len();
            return;
        }
        let _ = self.file.write_all(&chunk[..room]);
        self.captured = RUN_LOG_CAP_BYTES;
        self.truncated = true;
        self.line(&format!(
            "\n# output truncated at {RUN_LOG_CAP_BYTES} bytes; the rest was discarded"
        ));
    }

    fn line(&mut self, text: &str) {
        let _ = self.file.write_all(format!("{text}\n").as_bytes());
    }
}

type SharedLog = Arc<Mutex<RunLog>>;

fn log_line(log: &SharedLog, text: &str) {
    if let Ok(mut log) = log.lock() {
        log.line(text);
    }
}

/// Drains one stream into the log, keeping the bytes when asked.
fn pump(
    mut stream: impl Read + Send + 'static,
    log: SharedLog,
    collect: bool,
) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = vec![0_u8; 65_536];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if let Ok(mut log) = log.lock() {
                        log.write(&buffer[..read]);
                    }
                    if collect {
                        kept.extend_from_slice(&buffer[..read]);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        kept
    })
}

/// TextDecoder's reading of bytes: lossy UTF-8 with a leading BOM dropped.
fn decode(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned()
}

/// JavaScript's String.prototype.trim().
fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// Bun's exit code: the status, or 128 plus the signal that ended it.
fn exit_code(status: ExitStatus) -> i64 {
    status
        .code()
        .map_or_else(|| 128 + i64::from(status.signal().unwrap_or(0)), i64::from)
}

/// Bun's wording when a spawn fails, which lands in the run log.
fn spawn_error(program: &str, cwd: &str, error: &std::io::Error) -> String {
    let missing_cwd = !Path::new(cwd).is_dir();
    if error.kind() == std::io::ErrorKind::NotFound && !program.contains('/') && !missing_cwd {
        return format!("Executable not found in $PATH: \"{program}\"");
    }
    let (code, words) = match error.raw_os_error() {
        Some(libc::ENOENT) => ("ENOENT", "no such file or directory"),
        Some(libc::EACCES) => ("EACCES", "permission denied"),
        Some(libc::ENOTDIR) => ("ENOTDIR", "not a directory"),
        Some(libc::ENOEXEC) => ("ENOEXEC", "exec format error"),
        _ => return error.to_string(),
    };
    format!("{code}: {words}, posix_spawn '{program}'")
}

/// What a fire's loop watches besides its child.
struct Watch<'a> {
    store: &'a Store,
    run_id: &'a str,
    deadline: Option<i64>,
    next_cancel_check: i64,
    shutdown: &'a AtomicBool,
    stopped: Option<StopReason>,
}

impl Watch<'_> {
    /// Updates and returns the stop reason, the way 0.2.1's timer, cancel
    /// poll and shutdown listener would have set it by now.
    fn poll(&mut self) -> Option<StopReason> {
        if self.stopped.is_some() {
            return self.stopped;
        }
        let now = now_ms();
        if self.shutdown.load(Ordering::SeqCst) {
            self.stopped = Some(StopReason::Shutdown);
        } else if self.deadline.is_some_and(|deadline| now >= deadline) {
            self.stopped = Some(StopReason::Timeout);
        } else if now >= self.next_cancel_check {
            while self.next_cancel_check <= now {
                self.next_cancel_check += CANCEL_POLL_MS;
            }
            let running = self
                .store
                .get_run(self.run_id)
                .ok()
                .flatten()
                .is_some_and(|run| run.status == "running");
            if !running {
                self.stopped = Some(StopReason::Canceled);
            }
        }
        self.stopped
    }
}

struct PhaseResult {
    exit: Option<i64>,
    stdout: String,
    stopped: Option<StopReason>,
}

struct Phase<'a> {
    argv: &'a [String],
    cwd: &'a str,
    environment: Vec<(&'static str, String)>,
    stdin: Option<Vec<u8>>,
}

/// Runs one process as the leader of a new session and process group and
/// waits for the whole group: when the leader exits, whatever it left
/// behind is terminated so the run never hangs on a grandchild holding its
/// pipes, and when the fire is stopped the group is terminated mid-flight.
fn run_phase(
    phase: Phase,
    log: &SharedLog,
    watch: &mut Watch,
    on_spawn: impl FnOnce(i64),
) -> Result<PhaseResult, String> {
    let (program, arguments) = phase
        .argv
        .split_first()
        .ok_or_else(|| "cmd must not be empty".to_owned())?;
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(phase.cwd)
        .envs(
            phase
                .environment
                .iter()
                .map(|(key, value)| (*key, value.as_str())),
        )
        .stdin(if phase.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setsid is async-signal-safe and touches only the child.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child: Child = command
        .spawn()
        .map_err(|error| spawn_error(program, phase.cwd, &error))?;
    let pgid = i64::from(child.id());
    on_spawn(pgid);
    let stdout = child
        .stdout
        .take()
        .map(|stream| pump(stream, Arc::clone(log), true));
    let stderr = child
        .stderr
        .take()
        .map(|stream| pump(stream, Arc::clone(log), false));
    let feeder = match (child.stdin.take(), phase.stdin) {
        (Some(mut input), Some(bytes)) => Some(thread::spawn(move || {
            // A child that never reads its stdin closes the pipe; that's fine.
            let _ = input.write_all(&bytes);
        })),
        _ => None,
    };

    let mut status: Option<ExitStatus> = None;
    let stopped = loop {
        if let Ok(Some(exited)) = child.try_wait() {
            status = Some(exited);
            break None;
        }
        if let Some(reason) = watch.poll() {
            break Some(reason);
        }
        thread::sleep(TICK);
    };
    let mut reap = || {
        if status.is_none() {
            if let Ok(Some(exited)) = child.try_wait() {
                status = Some(exited);
            }
        }
    };
    match stopped {
        None => {
            if group_is_alive(pgid) {
                log_line(
                    log,
                    &format!("# terminating processes left behind {}", stamp()),
                );
                terminate_group_reaping(pgid, KILL_GRACE, &mut reap);
            }
        }
        Some(_) => terminate_group_reaping(pgid, KILL_GRACE, &mut reap),
    }
    let status = match status {
        Some(status) => status,
        None => child.wait().map_err(|error| error.to_string())?,
    };
    let kept = stdout
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    if let Some(reader) = stderr {
        let _ = reader.join();
    }
    if let Some(feeder) = feeder {
        let _ = feeder.join();
    }
    Ok(PhaseResult {
        exit: Some(exit_code(status)),
        stdout: decode(&kept),
        stopped,
    })
}

/// Node's path.basename.
fn basename(value: &str) -> String {
    value
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// Runs one fire and records how it ended. Gate contract in `output`
/// mode: exit 0 with empty stdout closes the gate (a clean pass), exit 0
/// with stdout opens it; in `exit` mode exit 0 alone opens it. Either way
/// the gate's stdout becomes the action's stdin, and a nonzero exit is a
/// gate failure. The schedule's timeout bounds gate and action together.
pub fn execute_fire(
    store: &Store,
    schedule: &Schedule,
    run: &Run,
    shutdown: &AtomicBool,
) -> Result<Run, AppError> {
    let run_id = run.id.as_str();
    let pointer = run.log_pointer.as_ref().map_or_else(
        || {
            store
                .home
                .join("logs")
                .join(&schedule.name)
                .join(format!("{run_id}.log"))
        },
        PathBuf::from,
    );
    if let Some(folder) = pointer.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(folder)
            .map_err(|error| AppError::new("unexpected_error", error.to_string()))?;
    }
    let log: SharedLog =
        Arc::new(Mutex::new(RunLog::open(&pointer).map_err(|error| {
            AppError::new("unexpected_error", error.to_string())
        })?));
    let settle =
        |status: &str, gate_exit: Option<i64>, action_exit: Option<i64>| -> Result<Run, AppError> {
            store.finish_run(run_id, status, gate_exit, action_exit)?;
            Ok(store.get_run(run_id)?.unwrap_or_else(|| run.clone()))
        };

    log_line(&log, &format!("# {} {run_id}", schedule.name));
    log_line(
        &log,
        &format!("# started {} trigger={}", stamp(), run.trigger),
    );

    let started = now_ms();
    let mut watch = Watch {
        store,
        run_id,
        deadline: schedule.timeout_ms.map(|timeout| started + timeout),
        next_cancel_check: started + CANCEL_POLL_MS,
        shutdown,
        stopped: shutdown
            .load(Ordering::SeqCst)
            .then_some(StopReason::Shutdown),
    };
    let stopped_status = |reason: StopReason| -> &'static str {
        match reason {
            StopReason::Timeout => {
                let timeout = schedule.timeout_ms.unwrap_or_default();
                log_line(&log, &format!("# timed out after {timeout}ms {}", stamp()));
                "timed_out"
            }
            StopReason::Canceled => {
                log_line(&log, &format!("# canceled {}", stamp()));
                "canceled"
            }
            StopReason::Shutdown => {
                log_line(&log, &format!("# interrupted {}", stamp()));
                "interrupted"
            }
        }
    };
    let environment = vec![
        ("ULTRADIAN_RUN_ID", run_id.to_owned()),
        ("ULTRADIAN_SCHEDULE", schedule.name.clone()),
    ];
    let on_spawn = |pgid: i64| {
        let _ = store.set_run_process_group(run_id, pgid);
    };

    let mut gate_exit: Option<i64> = None;
    let result = (|| -> Result<Run, String> {
        let mut context = String::new();
        if let Some(gate) = &schedule.gate {
            log_line(&log, &format!("# gate: {gate}"));
            let argv = ["/bin/sh".to_owned(), "-c".to_owned(), gate.clone()];
            let phase = Phase {
                argv: &argv,
                cwd: &schedule.working_directory,
                environment: environment.clone(),
                stdin: None,
            };
            let outcome = run_phase(phase, &log, &mut watch, on_spawn)?;
            gate_exit = if outcome.stopped.is_none() {
                outcome.exit
            } else {
                None
            };
            if let Some(reason) = outcome.stopped {
                return settle(stopped_status(reason), gate_exit, None)
                    .map_err(|error| error.message);
            }
            let exit = outcome.exit.unwrap_or_default();
            if exit != 0 {
                log_line(&log, &format!("# gate failed exit={exit} {}", stamp()));
                return settle("gate_failed", gate_exit, None).map_err(|error| error.message);
            }
            if schedule.gate_mode == "output" && js_trim(&outcome.stdout).is_empty() {
                log_line(&log, &format!("# gate clean {}", stamp()));
                return settle("clean", gate_exit, None).map_err(|error| error.message);
            }
            context = outcome.stdout;
            let length = context.encode_utf16().count();
            log_line(
                &log,
                &format!("# gate open ({length} bytes of context) {}", stamp()),
            );
        }
        if let Some(reason) = watch.stopped {
            return settle(stopped_status(reason), gate_exit, None).map_err(|error| error.message);
        }

        let executor = basename(schedule.command.first().map_or("command", String::as_str));
        store
            .set_run_executor(run_id, &executor)
            .map_err(|error| error.message)?;
        log_line(&log, &format!("# action: {}", schedule.command.join(" ")));
        let mut action_environment = environment.clone();
        action_environment.push(("ULTRADIAN_SESSION_ID", run_id.to_owned()));
        let phase = Phase {
            argv: &schedule.command,
            cwd: &schedule.working_directory,
            environment: action_environment,
            stdin: schedule.gate.as_ref().map(|_| context.into_bytes()),
        };
        let outcome = run_phase(phase, &log, &mut watch, |pgid| {
            let _ = store.set_run_process_group(run_id, pgid);
        })?;
        if let Some(reason) = outcome.stopped {
            return settle(stopped_status(reason), gate_exit, None).map_err(|error| error.message);
        }
        let exit = outcome.exit.unwrap_or_default();
        log_line(&log, &format!("# finished exit={exit} {}", stamp()));
        let status = if exit == 0 { "succeeded" } else { "failed" };
        settle(status, gate_exit, outcome.exit).map_err(|error| error.message)
    })();
    match result {
        Ok(run) => Ok(run),
        Err(message) => {
            log_line(&log, &format!("# error {message}"));
            settle("failed", gate_exit, None)
        }
    }
}

/// Set by SIGINT or SIGTERM while a foreground fire runs, so it stops its
/// process group and records the run as interrupted.
pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Routes the first SIGINT and SIGTERM to [`SHUTDOWN`]; a second one acts
/// as it normally would, as 0.2.1's `process.once` listeners did.
pub fn watch_for_shutdown() {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the handler only stores to an atomic; SA_RESETHAND makes
        // it one-shot.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
            action.sa_flags = libc::SA_RESETHAND;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

#[cfg(test)]
mod tests;
