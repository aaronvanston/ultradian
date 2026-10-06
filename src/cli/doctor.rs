//! `doctor` and `completion`.

use serde::Serialize;

use super::{ArgValue, Context, Done, NAME, VERSION};
use crate::catalog;
use crate::daemon::{control, daemon_is_live};
use crate::errors::{AppError, exit};
use crate::store::{Store, resolve_home};

#[derive(Serialize)]
struct Check {
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fix: Option<String>,
    name: &'static str,
    status: &'static str,
}

fn check(
    name: &'static str,
    status: &'static str,
    detail: impl Into<String>,
    fix: Option<&str>,
) -> Check {
    Check {
        detail: detail.into(),
        fix: fix.map(str::to_owned),
        name,
        status,
    }
}

fn writable(path: &std::path::Path) -> bool {
    let Ok(text) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: access only reads the path it is given.
    unsafe { libc::access(text.as_ptr(), libc::W_OK) == 0 }
}

fn data_directory() -> Check {
    let home = resolve_home();
    let shown = home.to_string_lossy().into_owned();
    match Store::open(&home) {
        Ok(_) if writable(&home) => check("Data directory", "pass", shown, None),
        Ok(_) => check(
            "Data directory",
            "fail",
            format!("{shown}: EACCES: permission denied, access '{shown}'"),
            Some("Set ULTRADIAN_HOME to a writable directory."),
        ),
        Err(error) => check(
            "Data directory",
            "fail",
            format!("{shown}: {}", error.message),
            Some("Set ULTRADIAN_HOME to a writable directory."),
        ),
    }
}

fn folder_bytes(folder: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => folder_bytes(&path),
                _ => std::fs::metadata(&path).map_or(0, |metadata| metadata.len()),
            }
        })
        .sum()
}

fn run_history(store: &Store) -> Result<Check, AppError> {
    let total = store.count_runs(None)?;
    let bytes = folder_bytes(&store.home.join("logs"));
    let size = if bytes > 1_000_000_000 {
        format!("{:.1}GB", bytes as f64 / 1_000_000_000.0)
    } else {
        format!("{:.1}MB", bytes as f64 / 1_000_000.0)
    };
    let detail = format!("{total} run(s), {size} of captured logs");
    Ok(if bytes > 1_000_000_000 {
        check(
            "Run history",
            "warn",
            detail,
            Some("Reclaim space with 'prune --older-than 30d'."),
        )
    } else {
        check("Run history", "pass", detail, None)
    })
}

/// systemd's lingering for this user, when loginctl can say.
fn systemd_linger() -> Option<bool> {
    let user = std::env::var("USER")
        .ok()
        .or_else(|| std::env::var("LOGNAME").ok())?;
    let output = std::process::Command::new("loginctl")
        .args(["show-user", &user, "--property=Linger", "--value"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim() == "yes")
}

fn login_service() -> Check {
    if !control::service_installed() {
        return check(
            "Login service",
            "pass",
            "not installed; 'daemon start' runs until logout or reboot",
            None,
        );
    }
    if control::is_launchd() {
        return check("Login service", "pass", "installed with launchd", None);
    }
    match systemd_linger() {
        Some(true) => check(
            "Login service",
            "pass",
            "installed with systemd, lingering enabled",
            None,
        ),
        Some(false) => check(
            "Login service",
            "warn",
            "installed with systemd, but lingering is off: the daemon stops when your last session ends",
            Some("Run 'loginctl enable-linger $USER'."),
        ),
        None => check(
            "Login service",
            "warn",
            "installed with systemd; lingering could not be checked",
            Some("Run 'loginctl enable-linger $USER'."),
        ),
    }
}

fn daemon(store: &Store) -> Result<Check, AppError> {
    let info = store.read_daemon()?;
    Ok(match info.filter(|info| daemon_is_live(Some(info))) {
        Some(info) => check(
            "Daemon",
            "pass",
            format!("running (pid {})", info.pid),
            None,
        ),
        None => check(
            "Daemon",
            "warn",
            "not running",
            Some("Start it with 'daemon start'."),
        ),
    })
}

fn platform() -> String {
    let os = if std::env::consts::OS == "macos" {
        "darwin"
    } else {
        std::env::consts::OS
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    format!("{os}-{arch}")
}

/// Fast, offline checks, in 0.2.1's order. The Bun runtime check is now
/// the build's own.
pub fn doctor(context: &Context) -> Result<Done, AppError> {
    let mut checks = vec![data_directory()];
    match Store::open(&resolve_home()) {
        Ok(store) => {
            checks.push(run_history(&store)?);
            checks.push(login_service());
            checks.push(daemon(&store)?);
        }
        Err(error) => return Err(error),
    }
    checks.push(check(
        "Runtime",
        "pass",
        format!("Rust build of {NAME} {VERSION}"),
        None,
    ));
    let supported = matches!(std::env::consts::OS, "macos" | "linux");
    checks.push(if supported {
        check("Operating system", "pass", platform(), None)
    } else {
        check(
            "Operating system",
            "warn",
            platform(),
            Some("Use macOS or Linux, or add and test a platform target."),
        )
    });
    let status = if checks.iter().any(|check| check.status == "fail") {
        "fail"
    } else if checks.iter().any(|check| check.status == "warn") {
        "warn"
    } else {
        "pass"
    };
    let ui = &context.ui;
    let rows: Vec<Vec<String>> = checks
        .iter()
        .map(|check| {
            let symbol = match check.status {
                "pass" => ui.success(ui.symbols.success),
                "warn" => ui.warning(ui.symbols.warning),
                _ => ui.danger(ui.symbols.error),
            };
            let detail = match &check.fix {
                Some(fix) => format!("{} {}", check.detail, ui.muted(&format!("Fix: {fix}"))),
                None => check.detail.clone(),
            };
            vec![symbol, check.name.to_owned(), detail]
        })
        .collect();
    #[derive(Serialize)]
    struct Report {
        checks: Vec<Check>,
        status: &'static str,
    }
    let human = ui.table(&["", "Check", "Detail"], &rows);
    let mut done = Done::new(&Report { checks, status }, human);
    if status == "fail" {
        done.outcome.exit_code = exit::ERROR;
        done.outcome.hint = Some("Resolve failed checks, then run the doctor again.".into());
    }
    Ok(done)
}

/// Every word of every command path, once, sorted.
fn tokens() -> Vec<String> {
    let mut words: Vec<String> = catalog::catalog()
        .commands
        .iter()
        .flat_map(|command| command.path.iter().cloned())
        .collect();
    words.sort();
    words.dedup();
    words
}

pub fn completion_script(shell: &str) -> Option<String> {
    let words = tokens().join(" ");
    Some(match shell {
        "bash" => format!(
            "_{NAME}_completion() {{\n  local current=\"${{COMP_WORDS[COMP_CWORD]}}\"\n  COMPREPLY=( $(compgen -W \"{words}\" -- \"$current\") )\n}}\ncomplete -F _{NAME}_completion {NAME}\n"
        ),
        "zsh" => format!(
            "#compdef {NAME}\n_{NAME}() {{\n  local -a commands\n  commands=({words})\n  _describe '{NAME} commands' commands\n}}\ncompdef _{NAME} {NAME}\n"
        ),
        "fish" => {
            let lines: Vec<String> = tokens()
                .iter()
                .map(|token| format!("complete -c {NAME} -a '{token}'"))
                .collect();
            format!("complete -c {NAME} -f\n{}\n", lines.join("\n"))
        }
        _ => return None,
    })
}

pub fn completion(context: &Context) -> Result<Done, AppError> {
    let shell = match context.arguments.first() {
        Some(ArgValue::One(Some(shell))) => shell.clone(),
        _ => String::new(),
    };
    let Some(script) = completion_script(&shell) else {
        return Err(AppError::usage(
            "unsupported_shell",
            format!("Unsupported shell \"{shell}\"."),
        )
        .hint("Choose bash, zsh, or fish."));
    };
    let human = script.trim_end().to_owned();
    Ok(Done::new(&script, human))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completions_list_every_command_word_once_in_order() {
        let zsh = completion_script("zsh").expect("zsh");
        assert!(zsh.starts_with("#compdef ultradian\n_ultradian() {\n  local -a commands\n  commands=(add cancel completion daemon describe doctor install list logs once pause prune"));
        assert!(
            completion_script("bash")
                .expect("bash")
                .ends_with("complete -F _ultradian_completion ultradian\n")
        );
        let fish = completion_script("fish").expect("fish");
        assert_eq!(
            fish.matches("complete -c ultradian -a '").count(),
            tokens().len()
        );
        assert!(completion_script("tcsh").is_none());
    }
}
