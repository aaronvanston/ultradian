//! The installed command line: the built binary run in a throwaway HOME and
//! ULTRADIAN_HOME, with launchctl, systemctl and loginctl first on PATH as
//! stubs that only record the call and fail. Expected outputs come from
//! what 0.2.1 printed for the same commands.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct Sandbox {
    root: PathBuf,
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Sandbox {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let base = std::env::temp_dir().canonicalize().expect("temp dir");
        let root = base.join(format!(
            "ultradian-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        assert!(root.starts_with(&base));
        let _ = std::fs::remove_dir_all(&root);
        for folder in ["home", "work/automation", "guard"] {
            std::fs::create_dir_all(root.join(folder)).expect("folder");
        }
        for tool in ["launchctl", "systemctl", "loginctl"] {
            let stub = root.join("guard").join(tool);
            let log = root.join("guard.log");
            let script = format!(
                "#!/bin/sh\necho \"{tool} $*\" >>'{}'\nexit 97\n",
                log.display()
            );
            std::fs::write(&stub, script).expect("stub");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("mode");
        }
        Self { root }
    }

    fn run(&self, args: &[&str]) -> Out {
        let output = Command::new(env!("CARGO_BIN_EXE_ultradian"))
            .args(args)
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("ULTRADIAN_HOME", self.root.join("state"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("guard").display()),
            )
            .env("TZ", "UTC")
            .current_dir(self.root.join("work"))
            .stdin(Stdio::null())
            .output()
            .expect("runs");
        Out {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// `line` split on spaces, keeping 'single quoted' words whole.
    fn line(&self, line: &str) -> Out {
        let work = self.root.join("work").display().to_string();
        let line = line.replace("$WORK", &work);
        let mut words = Vec::new();
        for (index, part) in line.split('\'').enumerate() {
            if index % 2 == 1 {
                words.push(part.to_owned());
            } else {
                words.extend(
                    part.split(' ')
                        .filter(|word| !word.is_empty())
                        .map(str::to_owned),
                );
            }
        }
        let args: Vec<&str> = words.iter().map(String::as_str).collect();
        self.run(&args)
    }

    /// The `data` of a command that must succeed.
    fn ok(&self, line: &str) -> Value {
        let out = self.line(line);
        assert_eq!(out.code, 0, "{line}: {}", out.stderr);
        let envelope: Value = serde_json::from_str(&out.stdout).expect("one JSON document");
        envelope["data"].clone()
    }

    /// The exit code and error code of a command that must fail.
    fn error(&self, line: &str) -> (i32, String) {
        let out = self.line(line);
        // Commander's own lines may come first; the envelope starts a line.
        let start = out.stderr.find("\n{").map_or(0, |index| index + 1);
        let envelope: Value = serde_json::from_str(&out.stderr[start..])
            .unwrap_or_else(|_| panic!("{line}: no envelope in {:?}", out.stderr));
        assert_eq!(envelope["ok"], false);
        let code = envelope["error"]["code"].as_str().unwrap_or_default();
        (out.code, code.to_owned())
    }

    /// Masks what changes from run to run, as the 0.2.1 recordings were.
    fn mask(&self, text: &str, values: &[(&Value, &str)]) -> String {
        let mut text = text.replace(&self.root.display().to_string(), "<TMP>");
        for (value, label) in values {
            if let Some(value) = value.as_str() {
                text = text.replace(&format!("\"{value}\""), &format!("\"{label}\""));
            }
        }
        text
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Stops any daemon a test started here, failed or not.
        let _ = self.run(&["daemon", "stop", "--json"]);
        let calls = std::fs::read_to_string(self.root.join("guard.log")).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&self.root);
        if !std::thread::panicking() {
            assert_eq!(calls, "", "a service manager was reached");
        }
    }
}

/// Every key of `expected` is present in `actual` and matches, recursively
/// through objects and same-length lists; other keys in `actual` are
/// ignored. A key expected as null must be there as null.
#[track_caller]
fn has(actual: &Value, expected: Value) {
    match (actual, &expected) {
        (Value::Object(actual), Value::Object(fields)) => {
            for (key, value) in fields {
                let found = actual
                    .get(key)
                    .unwrap_or_else(|| panic!("missing {key} in {actual:?}"));
                has(found, value.clone());
            }
        }
        (Value::Array(actual), Value::Array(items)) => {
            assert_eq!(actual.len(), items.len(), "{actual:?}");
            for (actual, item) in actual.iter().zip(items) {
                has(actual, item.clone());
            }
        }
        _ => assert_eq!(actual, &expected),
    }
}

fn names(list: &Value) -> Vec<&str> {
    let items = list.as_array().expect("a list");
    items
        .iter()
        .map(|item| item["name"].as_str().unwrap_or_default())
        .collect()
}

fn is_time(value: &Value) -> bool {
    let text = value.as_str().unwrap_or_default();
    text.len() == 24 && text.ends_with('Z') && text.as_bytes()[19] == b'.'
}

const ADD_X: &str = "add job-x --cron '0 9 * * *' --tz Australia/Sydney --timeout 6h --catch-up 30m --gate 'sh \"$WORK/automation/gate.sh\"' --gate-mode exit --group batch --cwd $WORK/automation --yes --json -- /bin/sh $WORK/automation/run.sh";

/// The schedule record and its envelope, exactly as 0.2.1 printed them.
const ADDED: &str = r#"{
  "command": "add",
  "data": {
    "mode": "applied",
    "schedule": {
      "active": true,
      "catch_up_seconds": 1800,
      "command": [
        "/bin/sh",
        "<TMP>/work/automation/run.sh"
      ],
      "created_at": "<TIME>",
      "cwd": "<TMP>/work/automation",
      "gate": "sh \"<TMP>/work/automation/gate.sh\"",
      "gate_mode": "exit",
      "group": "batch",
      "id": "<schedule:1>",
      "name": "job-x",
      "next_fire_at": "<TIME>",
      "timeout_seconds": 21600,
      "trigger": {
        "expression": "0 9 * * *",
        "kind": "cron",
        "timezone": "Australia/Sydney"
      },
      "updated_at": "<TIME>"
    }
  },
  "ok": true,
  "schemaVersion": 2,
  "hint": "The daemon is not running; start it with 'ultradian daemon start'."
}
"#;

/// The run record `run --detach` returns, exactly as 0.2.1 printed it.
const QUEUED: &str = r#"{
  "command": "run",
  "data": {
    "action_exit": null,
    "agent_session_id": null,
    "cwd": "<TMP>/work/automation",
    "executor": null,
    "finished_at": null,
    "gate_exit": null,
    "log_pointer": "<TMP>/state/logs/job-x/<DATE>/<run:1>.log",
    "machine_id": "<HOST>",
    "pgid": null,
    "run_id": "<run:1>",
    "schedule": "job-x",
    "schedule_id": "<schedule:1>",
    "started_at": "<TIME>",
    "status": "queued",
    "trigger": "manual"
  },
  "ok": true,
  "schemaVersion": 2,
  "hint": "The daemon is not running; this run waits until it starts with 'ultradian daemon start'."
}
"#;

#[test]
fn a_schedule_lives_through_add_set_pause_run_cancel_and_rm() {
    let sandbox = Sandbox::new();
    has(
        &sandbox.ok("status --json"),
        json!({"daemon": {"live": false}, "schedules": {"total": 0}}),
    );

    let added = sandbox.line(ADD_X);
    assert_eq!(added.code, 0, "{}", added.stderr);
    let record: Value = serde_json::from_str(&added.stdout).expect("JSON");
    let schedule = &record["data"]["schedule"];
    let mut masks = vec![(&schedule["id"], "<schedule:1>")];
    for field in ["created_at", "next_fire_at", "updated_at"] {
        assert!(is_time(&schedule[field]), "{field}");
        masks.push((&schedule[field], "<TIME>"));
    }
    assert_eq!(sandbox.mask(&added.stdout, &masks), ADDED);

    let every = sandbox.ok("add job-y --every 15m --timeout 6h --catch-up 0m --group batch --cwd $WORK/automation --yes --json -- /bin/sh run.sh");
    has(
        &every["schedule"],
        json!({"trigger": {"kind": "every", "seconds": 900}, "catch_up_seconds": 0}),
    );
    let hours =
        sandbox.ok("add job-w --every 2h --catch-up 90m --group batch --yes --json -- echo hi");
    has(&hours["schedule"], json!({"catch_up_seconds": 5400}));
    let listed = sandbox.ok("ls --json");
    let listed: Vec<&str> = listed
        .as_array()
        .expect("list")
        .iter()
        .map(|row| row["schedule"]["name"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(listed, ["job-w", "job-x", "job-y"]);

    let set = sandbox.ok("set job-x --cron '30 8 * * 1-5' --tz America/New_York --timeout 2h --catch-up 1h --no-gate --json");
    has(
        &set,
        json!({"trigger": {"timezone": "America/New_York"}, "gate": null, "timeout_seconds": 7200, "catch_up_seconds": 3600}),
    );
    let gated = sandbox.ok("set job-y --every 1h --gate 'sh gate.sh' --gate-mode exit --json");
    has(&gated, json!({"gate": "sh gate.sh", "gate_mode": "exit"}));
    let berlin = sandbox.ok("set job-x --tz Europe/Berlin --json");
    has(
        &berlin,
        json!({"trigger": {"expression": "30 8 * * 1-5", "timezone": "Europe/Berlin"}}),
    );
    // A cron with no zone reads in the machine's local time (TZ=UTC here).
    let local = sandbox.ok("set job-x --tz local --json");
    has(&local, json!({"trigger": {"timezone": null}}));
    assert!(
        local["next_fire_at"]
            .as_str()
            .unwrap_or_default()
            .ends_with("T08:30:00.000Z")
    );
    has(
        &sandbox.ok("set job-y --no-timeout --json"),
        json!({"timeout_seconds": null}),
    );
    has(
        &sandbox.ok("set job-y --no-group --json"),
        json!({"group": null}),
    );
    has(
        &sandbox.ok("set job-y --group loops --json"),
        json!({"group": "loops"}),
    );
    let manual = sandbox.ok("set job-w --manual --json");
    has(
        &manual,
        json!({"trigger": {"kind": "manual"}, "next_fire_at": null}),
    );

    for _ in 0..2 {
        has(
            &sandbox.ok("pause job-x --json")[0],
            json!({"active": false, "next_fire_at": null}),
        );
    }
    assert!(is_time(
        &sandbox.ok("resume job-x --json")[0]["next_fire_at"]
    ));
    let paused = sandbox.ok("pause --group batch --json");
    assert_eq!(names(&paused), ["job-w", "job-x"]);
    has(&paused, json!([{"active": false}, {"active": false}]));
    let resumed = sandbox.ok("resume --group batch --json");
    assert!(
        resumed
            .as_array()
            .expect("list")
            .iter()
            .all(|row| row["active"] == true)
    );

    let detached = sandbox.line("run job-x --detach --json");
    assert_eq!(detached.code, 0, "{}", detached.stderr);
    let run: Value = serde_json::from_str(&detached.stdout).expect("exactly one JSON document");
    let run = &run["data"];
    let run_id = run["run_id"].as_str().expect("run id").to_owned();
    let date = run["started_at"]
        .as_str()
        .unwrap_or_default()
        .get(..10)
        .unwrap_or_default();
    let masks = [
        (&run["started_at"], "<TIME>"),
        (&run["schedule_id"], "<schedule:1>"),
        (&run["machine_id"], "<HOST>"),
    ];
    let masked = sandbox
        .mask(&detached.stdout, &masks)
        .replace(&run_id, "<run:1>")
        .replace(&format!("/{date}/"), "/<DATE>/");
    assert_eq!(masked, QUEUED);
    assert_eq!(
        sandbox.error("run job-x --detach --json"),
        (75, "run_in_flight".into())
    );
    sandbox.ok("run job-y --detach --json");

    // Each new run takes two revisions, so two runs leave the cursor at 4.
    let page = sandbox.ok("runs --limit 500 --json");
    assert_eq!(
        (&page["cursor"], names_of_runs(&page)),
        (&json!("4"), vec!["job-x", "job-y"])
    );
    has(
        &sandbox.ok("runs --since 4 --limit 500 --json"),
        json!({"cursor": "4", "runs": []}),
    );
    has(
        &sandbox.ok(&format!("cancel {run_id} --json")),
        json!({"status": "canceled"}),
    );
    assert_eq!(
        sandbox.error(&format!("cancel {run_id} --json")),
        (1, "run_finished".into())
    );
    let changed = sandbox.ok("runs --since 4 --limit 500 --json");
    has(
        &changed,
        json!({"cursor": "5", "runs": [{"run_id": run_id, "status": "canceled"}]}),
    );
    has(
        &sandbox.ok("runs --since 0 --limit 1 --json"),
        json!({"cursor": "4", "runs": [{"schedule": "job-y"}]}),
    );
    has(
        &sandbox.ok("runs --since 4 --limit 1 --json"),
        json!({"cursor": "5", "runs": [{"schedule": "job-x"}]}),
    );
    let jsonl = sandbox.line("runs --since 0 --limit 500 --jsonl").stdout;
    assert!(jsonl.starts_with("{\"timestamp\":\"") && jsonl.lines().count() == 1);
    assert!(
        jsonl.contains("\",\"type\":\"result\",\"command\":\"runs\",\"data\":{\"cursor\":\"5\"")
    );
    let compact = sandbox.line("runs --json --compact").stdout;
    assert!(compact.starts_with(
        "{\"command\":\"runs\",\"data\":{\"cursor\":\"5\",\"runs\":[{\"action_exit\":null,"
    ));
    let status = sandbox.ok("status --json");
    has(
        &status,
        json!({"active_runs": [{"schedule": "job-y", "status": "queued"}], "schedules": {"total": 3}}),
    );

    let once = sandbox.ok("once --name import --timeout 30m --json -- ./import.sh");
    has(
        &once,
        json!({"command": ["./import.sh"], "timeout_seconds": 1800}),
    );
    assert!(
        once["job"]
            .as_str()
            .unwrap_or_default()
            .starts_with("once-import-")
    );
    has(
        &sandbox.ok("run job-w --json"),
        json!({"status": "succeeded", "action_exit": 0, "executor": "echo"}),
    );
    has(
        &sandbox.ok("logs job-w --limit 5 --json"),
        json!({"total_runs": 1}),
    );
    assert_eq!(
        sandbox.error("prune --older-than 1d --json"),
        (2, "action_required".into())
    );
    has(
        &sandbox.ok("prune --older-than 1d --yes --json"),
        json!({"removed_runs": 0, "freed_bytes": 0}),
    );

    has(
        &sandbox.ok("rm job-y --yes --json"),
        json!({"removed": "job-y"}),
    );
    assert_eq!(
        sandbox.error("rm job-y --yes --json"),
        (1, "schedule_not_found".into())
    );
    assert_eq!(
        sandbox.error("rm job-x --json"),
        (2, "action_required".into())
    );
}

fn names_of_runs(page: &Value) -> Vec<&str> {
    let runs = page["runs"].as_array().expect("runs");
    runs.iter()
        .map(|run| run["schedule"].as_str().unwrap_or_default())
        .collect()
}

/// Exit code, error code, then the command line, as 0.2.1 answered them.
const ERRORS: &str = "
2 schedule_exists            add base --every 5m --yes --json -- echo hi
2 schedule_exists            add base --every 5m --yes -- echo --json
2 action_required            add other --every 5m --json -- echo hi
2 invalid_schedule_name      add 'bad name' --every 5m --yes --json -- echo hi
2 invalid_schedule_name      add g1 --every 5m --group 'a b' --yes --json -- echo hi
2 invalid_cron               add c1 --cron 'not a cron' --yes --json -- echo hi
2 invalid_timezone           add c3 --cron '0 9 * * *' --tz Mars/Olympus --yes --json -- echo hi
2 timezone_requires_cron     add c4 --every 5m --tz UTC --yes --json -- echo hi
2 conflicting_triggers       add c5 --cron '0 9 * * *' --every 5m --yes --json -- echo hi
2 invalid_duration           add d1 --every 10 --yes --json -- echo hi
2 invalid_duration           add d3 --every 0m --yes --json -- echo hi
2 invalid_duration           add d4 --every 5m --timeout 0s --yes --json -- echo hi
2 invalid_duration           add d5 --every 5m --catch-up soon --yes --json -- echo hi
2 invalid_working_directory  add d8 --every 5m --cwd nope --yes --json -- echo hi
1 schedule_not_found         set nobody --every 5m --json
2 nothing_to_set             set base --json
2 conflicting_triggers       set base --manual --every 5m --json
2 timezone_requires_cron     set base --tz UTC --json
1 group_not_found            pause --group nobody --json
2 conflicting_targets        pause base --group ops --json
2 invalid_schedule_name      pause --json
1 schedule_not_found         run nobody --detach --json
1 run_not_found              cancel run_nope --json
2 invalid_cursor             runs --since abc --json
2 invalid_cursor             runs --since -1 --json
2 invalid_options            runs --limit 0 --json
2 invalid_options            runs --limit 1.5 --json
1 run_not_found              logs --run run_nope --json
2 invalid_options            logs --limit 500 --json
2 invalid_duration           prune --older-than 0d --yes --json
1 schedule_not_found         rm nobody --yes --jsonl
2 unsupported_shell          completion tcsh --json
2 command_not_found          describe nothing here --json
2 command_not_found          describe ls --json
2 invalid_usage              bogus --json
2 invalid_usage              list extra --json
2 invalid_usage              once --json
2 invalid_usage              add x --every 5m --yes --json
2 invalid_usage              add x --gate-mode maybe --json -- echo
2 invalid_usage              version -- --json
2 invalid_usage              daemon --json
2 conflicting_output_modes   --json --jsonl version
";

#[test]
fn errors_exit_with_their_code_and_an_envelope() {
    let sandbox = Sandbox::new();
    sandbox.ok("add base --every 5m --yes --json -- echo hi");
    for row in ERRORS.lines().filter(|row| !row.is_empty()) {
        let (exit, rest) = row.split_once(' ').expect("exit");
        let (code, line) = rest.trim_start().split_once(' ').expect("code");
        let exit: i32 = exit.parse().expect("a number");
        assert_eq!(
            sandbox.error(line.trim_start()),
            (exit, code.to_owned()),
            "{row}"
        );
    }
    let compact = sandbox.line("rm nobody --yes --json --compact");
    assert_eq!(
        compact.stderr,
        "{\"error\":{\"code\":\"schedule_not_found\",\"message\":\"No schedule named \\\"nobody\\\".\",\"hint\":\"List schedules with 'list'.\"},\"ok\":false,\"schemaVersion\":2}\n"
    );
}

#[test]
fn the_parser_reports_and_answers_like_0_2_1() {
    let sandbox = Sandbox::new();
    let out = sandbox.line("list --bogus --json");
    let envelope = "{\n  \"error\": {\n    \"code\": \"invalid_usage\",\n    \"message\": \"error: unknown option '--bogus'\"\n  },\n  \"ok\": false,\n  \"schemaVersion\": 2\n}\n";
    let usage = "(run 'ultradian <command> --help' for details)\n";
    assert_eq!(
        (out.code, out.stderr),
        (
            2,
            format!("error: unknown option '--bogus'\n{usage}{envelope}")
        )
    );
    let out = sandbox.line("bogus");
    assert_eq!(
        (out.code, out.stderr),
        (
            2,
            format!("error: unknown command 'bogus'\n(Did you mean logs?)\n{usage}")
        )
    );
    let two = sandbox.line("lst --json").stderr;
    assert!(two.starts_with("error: unknown command 'lst'\n(Did you mean one of list, ls?)\n"));
    let flag = sandbox.line("list --jsno").stderr;
    assert_eq!(
        flag,
        format!("error: unknown option '--jsno'\n(Did you mean --json?)\n{usage}")
    );

    let version = env!("CARGO_PKG_VERSION");
    let out = sandbox.line("version --json --compact");
    assert!(
        out.stdout
            .starts_with("{\"command\":\"version\",\"data\":{\"arch\":")
    );
    let tail = format!(
        "\"runtime\":\"rust\",\"version\":\"{version}\"}},\"ok\":true,\"schemaVersion\":2}}\n"
    );
    assert!(
        out.code == 0 && out.stdout.ends_with(&tail),
        "{}",
        out.stdout
    );
    for line in ["--version", "-V", "list --version --json", "-qV"] {
        let out = sandbox.line(line);
        assert_eq!(
            (out.code, out.stdout),
            (0, format!("{version}\n")),
            "{line}"
        );
    }
    has(
        &sandbox.ok("describe add --json"),
        json!({"path": ["add"], "kind": "write"}),
    );
}

/// Polls until the run has finished, up to fifteen seconds.
fn finished(sandbox: &Sandbox, run_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let page = sandbox.ok("runs --limit 500 --json");
        let runs = page["runs"].as_array().expect("runs");
        if let Some(run) = runs
            .iter()
            .find(|run| run["run_id"] == run_id && !run["finished_at"].is_null())
        {
            return run.clone();
        }
        assert!(Instant::now() < deadline, "{run_id} never finished");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_daemon_started_directly_runs_gated_fires_and_stops() {
    let sandbox = Sandbox::new();
    let folder = sandbox.root.join("work/automation");
    for (name, script) in [
        ("open.sh", "echo \"gate context\"\n"),
        ("clean.sh", "exit 0\n"),
        (
            "run.sh",
            "echo \"run $ULTRADIAN_SCHEDULE\"\ncat\necho \"session $1\"\n",
        ),
    ] {
        std::fs::write(folder.join(name), script).expect("script");
    }
    sandbox.ok(
        "add open --every 1h --gate 'sh open.sh' --cwd automation --yes --json -- /bin/sh -c 'sh run.sh \"$ULTRADIAN_AGENT_SESSION_ID\"'",
    );
    sandbox.ok(
        "add clean --every 1h --gate 'sh clean.sh' --cwd automation --yes --json -- /bin/sh run.sh",
    );

    // No service file in this home, so restart starts the daemon directly.
    let pid = sandbox.ok("daemon restart --json")["pid"]
        .as_i64()
        .expect("a pid");
    has(
        &sandbox.ok("status --json"),
        json!({"daemon": {"live": true, "pid": pid}}),
    );

    let open = sandbox.ok("run open --detach --json");
    let open = finished(&sandbox, open["run_id"].as_str().expect("id"));
    has(
        &open,
        json!({"status": "succeeded", "gate_exit": 0, "action_exit": 0, "executor": "sh"}),
    );
    let log = std::fs::read_to_string(open["log_pointer"].as_str().expect("log")).expect("reads");
    assert!(log.contains("run open\ngate context\n"), "{log}");
    // The command names the agent session variable, so the run records the
    // id the action was handed.
    let session = open["agent_session_id"].as_str().expect("a session id");
    assert!(log.contains(&format!("\nsession {session}\n")), "{log}");
    let clean = sandbox.ok("run clean --detach --json");
    let clean = finished(&sandbox, clean["run_id"].as_str().expect("id"));
    has(
        &clean,
        json!({"status": "clean", "gate_exit": 0, "action_exit": null, "executor": null, "agent_session_id": null}),
    );

    assert_eq!(
        sandbox.ok("daemon stop --json"),
        json!({"pid": pid, "stopped": true})
    );
    has(
        &sandbox.ok("status --json"),
        json!({"daemon": {"live": false, "pid": null}}),
    );
    assert_eq!(
        sandbox.ok("daemon stop --json"),
        json!({"pid": null, "stopped": false})
    );
}

/// `daemon install --dry-run` as 0.2.1 printed it on each platform.
#[cfg(target_os = "macos")]
const INSTALL_PLAN: &str = r#"{
  "command": "daemon install",
  "data": {
    "environment": {
      "PATH": "/opt/tools/bin:/usr/bin:/bin",
      "ULTRADIAN_HOME": "<TMP>/state"
    },
    "path": "<TMP>/home/Library/LaunchAgents/com.ultradian.daemon.plist",
    "platform": "launchd",
    "program": [
      "<BIN>",
      "daemon",
      "run"
    ],
    "content": "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>com.ultradian.daemon</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string><BIN></string>\n    <string>daemon</string>\n    <string>run</string>\n  </array>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>\n  <key>ExitTimeOut</key>\n  <integer>30</integer>\n  <key>EnvironmentVariables</key>\n  <dict>\n    <key>PATH</key>\n    <string>/opt/tools/bin:/usr/bin:/bin</string>\n    <key>ULTRADIAN_HOME</key>\n    <string><TMP>/state</string>\n  </dict>\n  <key>StandardOutPath</key>\n  <string><TMP>/state/daemon.out.log</string>\n  <key>StandardErrorPath</key>\n  <string><TMP>/state/daemon.out.log</string>\n</dict>\n</plist>\n",
    "mode": "plan"
  },
  "ok": true,
  "schemaVersion": 2
}
"#;
#[cfg(target_os = "linux")]
const INSTALL_PLAN: &str = r#"{
  "command": "daemon install",
  "data": {
    "environment": {
      "PATH": "/opt/tools/bin:/usr/bin:/bin",
      "ULTRADIAN_HOME": "<TMP>/state"
    },
    "path": "<TMP>/home/.config/systemd/user/ultradian.service",
    "platform": "systemd",
    "program": [
      "<BIN>",
      "daemon",
      "run"
    ],
    "content": "[Unit]\nDescription=Ultradian scheduling daemon\n\n[Service]\nExecStart=\"<BIN>\" \"daemon\" \"run\"\nEnvironment=\"PATH=/opt/tools/bin:/usr/bin:/bin\"\nEnvironment=\"ULTRADIAN_HOME=<TMP>/state\"\nRestart=on-failure\nTimeoutStopSec=30\n\n[Install]\nWantedBy=default.target\n",
    "mode": "plan"
  },
  "ok": true,
  "schemaVersion": 2
}
"#;

#[test]
fn dry_runs_show_the_plan_and_write_nothing() {
    let sandbox = Sandbox::new();
    let plan = sandbox.ok("add plan --cron '0 2 * * *' --tz UTC --dry-run --json -- ./backup.sh");
    has(
        &plan,
        json!({"mode": "plan", "schedule": {"id": "schedule_preview", "name": "plan", "cwd": sandbox.root.join("work").display().to_string()}}),
    );
    assert_eq!(sandbox.ok("list --json"), json!([]));

    let out = sandbox.line("daemon install --dry-run --path /opt/tools/bin:/usr/bin:/bin --json");
    assert_eq!(out.code, 0, "{}", out.stderr);
    let record: Value = serde_json::from_str(&out.stdout).expect("JSON");
    let binary = record["data"]["program"][0]
        .as_str()
        .expect("the binary")
        .to_owned();
    let masked = sandbox.mask(&out.stdout, &[]).replace(&binary, "<BIN>");
    assert_eq!(masked, INSTALL_PLAN);
    let home = std::fs::read_dir(sandbox.root.join("home")).expect("home");
    assert_eq!(home.count(), 0, "nothing is written under HOME");
}
