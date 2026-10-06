//! `daemon start|stop|restart|install|uninstall` and `self install`.
//!
//! Every service-manager call goes through [`control`], which runs
//! `launchctl` or `systemctl` by name from PATH exactly as 0.2.1 did. The
//! contract and service tests put stubs first on PATH, so nothing here
//! reaches a real service manager under test.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::Value;

use super::daemon_is_live;
use super::service::{
    LAUNCHD_LABEL, ServiceSpec, daemon_output_path, launchd_plist_path, login_shell_path,
    render_launchd_plist, render_systemd_unit, systemd_unit_path,
};
use crate::errors::{AppError, exit};
use crate::output::now_ms;
use crate::runner::KILL_GRACE;
use crate::store::{DaemonInfo, Store, is_pid_alive, resolve_path};

fn io_error(error: std::io::Error) -> AppError {
    AppError::new("unexpected_error", error.to_string())
}

/// This binary, symlinks resolved, as Bun's process.execPath reported it.
pub fn self_command() -> Vec<String> {
    let exe = std::env::current_exe().unwrap_or_default();
    let exe = exe.canonicalize().unwrap_or(exe);
    vec![exe.to_string_lossy().into_owned()]
}

pub fn is_launchd() -> bool {
    cfg!(target_os = "macos")
}

fn platform_name() -> &'static str {
    if is_launchd() { "launchd" } else { "systemd" }
}

fn launchd_domain() -> String {
    // SAFETY: getuid cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

/// Runs a service-manager command. A failure is daemon_install_failed
/// unless tolerated; returns whether it exited 0.
fn control(command: &[&str], tolerate_failure: bool) -> Result<bool, AppError> {
    let output = Command::new(command[0])
        .args(&command[1..])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| AppError::new("unexpected_error", error.to_string()))?;
    let success = output.status.success();
    if !success && !tolerate_failure {
        let code = output
            .status
            .code()
            .map_or_else(|| "null".to_owned(), |code| code.to_string());
        return Err(AppError::new(
            "daemon_install_failed",
            format!("'{}' failed with exit {code}.", command.join(" ")),
        )
        .exit(exit::TEMPFAIL)
        .details(Value::from(String::from_utf8_lossy(&output.stderr).trim())));
    }
    Ok(success)
}

fn sleep_ms(ms: u64) {
    thread::sleep(Duration::from_millis(ms));
}

pub fn start_daemon(store: &Store) -> Result<DaemonInfo, AppError> {
    if let Some(existing) = store
        .read_daemon()?
        .filter(|info| daemon_is_live(Some(info)))
    {
        return Err(AppError::new(
            "daemon_already_running",
            format!("A daemon is already running (pid {}).", existing.pid),
        )
        .exit(exit::TEMPFAIL)
        .hint("Check it with 'status' or stop it with 'daemon stop'."));
    }
    let output = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(daemon_output_path(&store.home))
        .map_err(io_error)?;
    let program = self_command();
    let mut command = Command::new(&program[0]);
    command
        .args(&program[1..])
        .args(["daemon", "run"])
        .env("ULTRADIAN_HOME", &store.home)
        .stdin(Stdio::null())
        .stdout(output.try_clone().map_err(io_error)?)
        .stderr(output);
    // Its own session, so the daemon outlives the terminal or SSH session
    // that started it instead of dying with that session's hangup.
    // SAFETY: setsid is async-signal-safe and touches only the child.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(io_error)?;
    let child_pid = i64::from(child.id());
    // Not waited on: the daemon outlives this command.
    drop(child);
    for _ in 0..50 {
        if let Some(info) = store.read_daemon()? {
            if info.pid == child_pid && daemon_is_live(Some(&info)) {
                return Ok(info);
            }
        }
        sleep_ms(100);
    }
    Err(AppError::new(
        "daemon_start_timeout",
        "The daemon did not report a heartbeat within 5 seconds.",
    )
    .exit(exit::TEMPFAIL)
    .hint(format!(
        "Check {} for details.",
        store.home.join("daemon.log").display()
    )))
}

/// Stops the daemon the store's lock names. Only its pid exiting counts as
/// stopped; a wedged daemon can lose its row while the process survives.
pub fn stop_daemon(store: &Store) -> Result<(Option<i64>, bool), AppError> {
    let Some(info) = store.read_daemon()? else {
        return Ok((None, false));
    };
    if info.pid <= 1 || !is_pid_alive(info.pid) {
        store.clear_daemon(info.pid)?;
        return Ok((Some(info.pid), false));
    }
    let pid = libc::pid_t::try_from(info.pid).unwrap_or(0);
    // SAFETY: a positive pid other than init, taken from the lock row.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(AppError::new(
            "unexpected_error",
            std::io::Error::last_os_error().to_string(),
        ));
    }
    // As 0.2.1: the grace a fire gets, plus five seconds; longer for a
    // daemon that has released its lock (see wait_for_exit).
    if wait_for_exit(
        store,
        info.pid,
        KILL_GRACE + Duration::from_secs(5),
        Duration::from_secs(30),
    )? {
        return Ok((Some(info.pid), true));
    }
    Err(AppError::new(
        "daemon_stop_timeout",
        format!("The daemon (pid {}) did not stop in time.", info.pid),
    )
    .exit(exit::TEMPFAIL)
    .hint(format!(
        "Inspect the process manually: kill -9 {}",
        info.pid
    )))
}

/// Waits for a signaled daemon's pid to exit, up to `grace`, or up to
/// `released_grace` once the daemon has released its lock row: it has
/// finished shutting down, and 0.2.1 daemons then linger up to 15 s on a
/// leftover timer (always when a run was in flight), which would otherwise
/// fail an upgrade's `daemon install`. Only the pid exiting counts.
fn wait_for_exit(
    store: &Store,
    pid: i64,
    grace: Duration,
    released_grace: Duration,
) -> Result<bool, AppError> {
    let started = std::time::Instant::now();
    loop {
        if !is_pid_alive(pid) {
            return Ok(true);
        }
        let released = store.read_daemon()?.is_none_or(|holder| holder.pid != pid);
        if started.elapsed() >= if released { released_grace } else { grace } {
            return Ok(false);
        }
        sleep_ms(100);
    }
}

/// The service file this machine would get.
pub struct DaemonService {
    pub platform: &'static str,
    pub path: PathBuf,
    pub spec: ServiceSpec,
    pub content: String,
}

pub fn plan_daemon_service(
    store: &Store,
    path: Option<String>,
    retention: Option<String>,
) -> DaemonService {
    let path = path
        .or_else(login_shell_path)
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_else(|| "/usr/bin:/bin".into());
    let mut environment = vec![
        ("PATH".to_owned(), path),
        (
            "ULTRADIAN_HOME".to_owned(),
            store.home.to_string_lossy().into_owned(),
        ),
    ];
    if let Some(retention) = retention {
        environment.push(("ULTRADIAN_RETENTION".into(), retention));
    }
    let mut program = self_command();
    program.extend(["daemon".to_owned(), "run".to_owned()]);
    let spec = ServiceSpec {
        program,
        home: store.home.clone(),
        environment,
    };
    if is_launchd() {
        DaemonService {
            platform: "launchd",
            path: launchd_plist_path(),
            content: render_launchd_plist(&spec),
            spec,
        }
    } else {
        DaemonService {
            platform: "systemd",
            path: systemd_unit_path(),
            content: render_systemd_unit(&spec),
            spec,
        }
    }
}

/// Waits for a daemon other than the one that was running before.
fn wait_for_live(
    store: &Store,
    since: i64,
    previous_pid: Option<i64>,
) -> Result<DaemonInfo, AppError> {
    for _ in 0..100 {
        if let Some(info) = store.read_daemon()? {
            if Some(info.pid) != previous_pid
                && info.heartbeat_at >= since
                && daemon_is_live(Some(&info))
            {
                return Ok(info);
            }
        }
        sleep_ms(200);
    }
    Err(AppError::new(
        "daemon_start_timeout",
        "The supervised daemon did not report a heartbeat in time.",
    )
    .exit(exit::TEMPFAIL)
    .hint(format!(
        "Check {} for details.",
        store.home.join("daemon.log").display()
    )))
}

/// Registers the daemon with the user's service manager. Any manually
/// started daemon is stopped first so the supervised one can take over.
pub fn install_daemon(store: &Store, service: &DaemonService) -> Result<DaemonInfo, AppError> {
    stop_daemon(store)?;
    let installed_at = now_ms();
    if let Some(folder) = service.path.parent() {
        std::fs::create_dir_all(folder).map_err(io_error)?;
    }
    std::fs::write(&service.path, &service.content).map_err(io_error)?;
    let path = service.path.to_string_lossy().into_owned();
    if service.platform == "launchd" {
        let domain = launchd_domain();
        control(
            &["launchctl", "bootout", &format!("{domain}/{LAUNCHD_LABEL}")],
            true,
        )?;
        control(&["launchctl", "bootstrap", &domain, &path], false)?;
    } else {
        control(&["systemctl", "--user", "daemon-reload"], false)?;
        control(
            &[
                "systemctl",
                "--user",
                "enable",
                "--now",
                "ultradian.service",
            ],
            false,
        )?;
    }
    wait_for_live(store, installed_at, None)
}

pub fn service_installed() -> bool {
    if is_launchd() {
        launchd_plist_path().exists()
    } else {
        systemd_unit_path().exists()
    }
}

/// Restarts through the service manager when the daemon is installed as a
/// service, so the supervisor keeps tracking it, and directly otherwise.
/// Either way the new daemon runs whatever binary the program path names
/// now, which is what makes `self install` followed by this an upgrade.
pub fn restart_daemon(store: &Store) -> Result<DaemonInfo, AppError> {
    let previous = store.read_daemon()?;
    let previous_pid = previous
        .filter(|info| daemon_is_live(Some(info)))
        .map(|info| info.pid);
    let since = now_ms();
    if service_installed() {
        if is_launchd() {
            let domain = launchd_domain();
            let target = format!("{domain}/{LAUNCHD_LABEL}");
            if !control(&["launchctl", "kickstart", "-k", &target], true)? {
                let plist = launchd_plist_path().to_string_lossy().into_owned();
                control(&["launchctl", "bootstrap", &domain, &plist], false)?;
            }
        } else {
            control(
                &["systemctl", "--user", "restart", "ultradian.service"],
                false,
            )?;
        }
        return wait_for_live(store, since, previous_pid);
    }
    stop_daemon(store)?;
    start_daemon(store)
}

pub fn uninstall_daemon(store: &Store) -> Result<(&'static str, PathBuf), AppError> {
    if is_launchd() {
        let plist = launchd_plist_path();
        control(
            &[
                "launchctl",
                "bootout",
                &format!("{}/{LAUNCHD_LABEL}", launchd_domain()),
            ],
            true,
        )?;
        let _ = std::fs::remove_file(&plist);
        stop_daemon(store)?;
        return Ok((platform_name(), plist));
    }
    let unit = systemd_unit_path();
    if unit.exists() {
        control(
            &[
                "systemctl",
                "--user",
                "disable",
                "--now",
                "ultradian.service",
            ],
            true,
        )?;
        let _ = std::fs::remove_file(&unit);
        control(&["systemctl", "--user", "daemon-reload"], true)?;
    }
    stop_daemon(store)?;
    Ok((platform_name(), unit))
}

/// Copies this binary to a stable path atomically: a sibling temp file,
/// made executable, renamed over the target. A daemon running the old
/// binary keeps its open inode and moves on its next restart.
pub fn install_self(target: &Path) -> Result<(PathBuf, bool), AppError> {
    let cwd = std::env::current_dir().map_err(io_error)?;
    let destination = resolve_path(&cwd, target);
    let folder = destination
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder)
        .map_err(io_error)?;
    let replaced = destination.exists();
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let staging = folder.join(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        // A byte copy, not a clone, so no quarantine attribute travels along.
        let bytes = std::fs::read(std::env::current_exe()?)?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o755)
            .open(&staging)?;
        file.write_all(&bytes)?;
        drop(file);
        std::fs::rename(&staging, &destination)
    })();
    let _ = std::fs::remove_file(&staging);
    result.map_err(io_error)?;
    Ok((destination, replaced))
}

#[cfg(test)]
mod tests {
    use std::process::Child;

    use super::*;
    use crate::store::tests::temp_home;

    /// A stand-in daemon this test spawns: it ignores SIGTERM and exits by
    /// itself after `seconds`, like a 0.2.1 daemon on its leftover timer.
    fn lingering(seconds: &str) -> Child {
        Command::new("/bin/sh")
            .args(["-c", &format!("trap '' TERM; sleep {seconds}")])
            .spawn()
            .expect("spawns")
    }

    fn hold_lock(store: &Store, pid: i64) {
        store
            .claim_daemon(pid, "0.2.1", now_ms(), |_| false)
            .expect("claims");
    }

    #[test]
    fn a_daemon_that_released_its_lock_gets_longer_to_exit() {
        let home = temp_home();
        let store = Store::open(&home).expect("opens");
        let mut child = lingering("1");
        let pid = i64::from(child.id());
        store.clear_daemon(pid).expect("released");
        let reaper = thread::spawn(move || child.wait());
        let exited = wait_for_exit(
            &store,
            pid,
            Duration::from_millis(300),
            Duration::from_secs(5),
        )
        .expect("waits");
        let _ = reaper.join();
        assert!(exited);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_daemon_still_holding_its_lock_gets_the_usual_grace() {
        let home = temp_home();
        let store = Store::open(&home).expect("opens");
        let mut child = lingering("2");
        let pid = i64::from(child.id());
        hold_lock(&store, pid);
        let exited = wait_for_exit(
            &store,
            pid,
            Duration::from_millis(300),
            Duration::from_secs(5),
        )
        .expect("waits");
        assert!(!exited);
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(home);
    }
}
