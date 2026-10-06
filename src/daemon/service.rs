//! The launchd plist and systemd unit, rendered byte for byte as 0.2.1 did
//! so reinstalling leaves an existing service unchanged, and the PATH a
//! supervised daemon runs with.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::store::user_home;

pub const LAUNCHD_LABEL: &str = "com.ultradian.daemon";

/// What a supervised daemon runs with. PATH is spelled out because launchd
/// and systemd start services with a bare system PATH, and actions such as
/// `claude` or `codex` usually live elsewhere.
pub struct ServiceSpec {
    pub program: Vec<String>,
    pub home: PathBuf,
    /// In insertion order: PATH, ULTRADIAN_HOME, then ULTRADIAN_RETENTION.
    pub environment: Vec<(String, String)>,
}

impl ServiceSpec {
    pub fn environment_json(&self) -> Value {
        let mut map = Map::new();
        for (key, value) in &self.environment {
            map.insert(key.clone(), Value::from(value.as_str()));
        }
        Value::Object(map)
    }
}

pub fn daemon_output_path(home: &Path) -> PathBuf {
    home.join("daemon.out.log")
}

pub fn launchd_plist_path() -> PathBuf {
    user_home()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

pub fn systemd_unit_path() -> PathBuf {
    user_home()
        .join(".config")
        .join("systemd")
        .join("user")
        .join("ultradian.service")
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn render_launchd_plist(spec: &ServiceSpec) -> String {
    let program = spec
        .program
        .iter()
        .map(|argument| format!("    <string>{}</string>", xml_escape(argument)))
        .collect::<Vec<_>>()
        .join("\n");
    let environment = spec
        .environment
        .iter()
        .map(|(key, value)| {
            format!(
                "    <key>{}</key>\n    <string>{}</string>",
                xml_escape(key),
                xml_escape(value)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let log_path = xml_escape(&daemon_output_path(&spec.home).to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{program}
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ExitTimeOut</key>
  <integer>30</integer>
  <key>EnvironmentVariables</key>
  <dict>
{environment}
  </dict>
  <key>StandardOutPath</key>
  <string>{log_path}</string>
  <key>StandardErrorPath</key>
  <string>{log_path}</string>
</dict>
</plist>
"#
    )
}

/// systemd expands % specifiers everywhere and $ variables in ExecStart,
/// and unquotes C-style escapes inside double quotes, so every value is
/// quoted and each of those characters is escaped.
fn systemd_quote(value: &str, expands_variables: bool) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    let escaped = if expands_variables {
        escaped.replace('$', "$$")
    } else {
        escaped
    };
    format!("\"{escaped}\"")
}

pub fn render_systemd_unit(spec: &ServiceSpec) -> String {
    let exec_start = spec
        .program
        .iter()
        .map(|argument| systemd_quote(argument, true))
        .collect::<Vec<_>>()
        .join(" ");
    let environment = spec
        .environment
        .iter()
        .map(|(key, value)| {
            format!(
                "Environment={}",
                systemd_quote(&format!("{key}={value}"), false)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "[Unit]
Description=Ultradian scheduling daemon

[Service]
ExecStart={exec_start}
{environment}
Restart=on-failure
TimeoutStopSec=30

[Install]
WantedBy=default.target
"
    )
}

/// A PATH for a long-lived service: each folder once, in order, and only
/// folders that exist now. Version managers such as fnm add per-shell
/// folders that vanish later; those already gone are dropped here.
pub fn tidy_path(value: &str) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for folder in value.split(':') {
        if folder.is_empty() || seen.contains(&folder) {
            continue;
        }
        seen.push(folder);
    }
    seen.into_iter()
        .filter(|folder| Path::new(folder).exists())
        .collect::<Vec<_>>()
        .join(":")
}

/// The PATH a login shell sets up, where people install their tools. Gives
/// up after five seconds, stopping only the shell it started.
pub fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut child = Command::new(&shell)
        .args(["-lc", "printf %s \"$PATH\""])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut output = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    use std::io::Read;
                    let _ = stdout.read_to_string(&mut output);
                }
                let value = tidy_path(output.trim());
                return (status.success() && !value.is_empty()).then_some(value);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(program: &[&str]) -> ServiceSpec {
        ServiceSpec {
            program: program.iter().map(|part| (*part).to_owned()).collect(),
            home: PathBuf::from("/Users/casey/state 100%"),
            environment: vec![
                ("PATH".into(), "/opt/tools & more/bin:/usr/bin".into()),
                ("ULTRADIAN_HOME".into(), "/Users/casey/state 100%".into()),
            ],
        }
    }

    #[test]
    fn a_plist_escapes_every_value_it_embeds() {
        let plist = render_launchd_plist(&spec(&["/tmp/<odd> \"name\"/udian", "daemon", "run"]));
        assert!(plist.contains("<string>/tmp/&lt;odd&gt; &quot;name&quot;/udian</string>"));
        assert!(plist.contains("<string>/opt/tools &amp; more/bin:/usr/bin</string>"));
        assert!(plist.contains("<key>PATH</key>"));
        assert!(!plist.contains("& more"));
        assert!(plist.contains("<string>/Users/casey/state 100%/daemon.out.log</string>"));
    }

    #[test]
    fn a_unit_quotes_arguments_and_escapes_systemd_s_specifiers_and_variables() {
        let unit = render_systemd_unit(&spec(&["/srv/a dir/udi$an \"x\"", "daemon", "run"]));
        assert!(unit.contains("ExecStart=\"/srv/a dir/udi$$an \\\"x\\\"\" \"daemon\" \"run\""));
        assert!(unit.contains("Environment=\"ULTRADIAN_HOME=/Users/casey/state 100%%\""));
        assert!(unit.contains("Environment=\"PATH=/opt/tools & more/bin:/usr/bin\""));
    }

    #[test]
    fn keeps_each_existing_folder_once_in_order() {
        let home = crate::store::tests::temp_home();
        let home_text = home.to_string_lossy().into_owned();
        assert_eq!(
            tidy_path(&format!(
                "/usr/bin::{home_text}/gone:/bin:/usr/bin:{home_text}"
            )),
            format!("/usr/bin:/bin:{home_text}")
        );
        let _ = std::fs::remove_dir_all(home);
    }
}
