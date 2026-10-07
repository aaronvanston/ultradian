//! Ported from 0.2.1's store.test.ts.

use super::*;

/// A store in its own folder under the system temp directory, removed
/// when dropped. Shared by every test that needs a store.
pub(crate) struct TempStore {
    store: Option<Store>,
    pub(crate) home: PathBuf,
}

impl TempStore {
    pub(crate) fn new() -> Self {
        let mut temp = Self::empty();
        temp.store = Some(Store::open(&temp.home).expect("opens"));
        temp
    }

    /// The folder alone, for tests that shape a database before opening it.
    pub(crate) fn empty() -> Self {
        let base = std::env::temp_dir().canonicalize().expect("temp dir");
        let mut bytes = [0_u8; 8];
        let _ = getrandom::fill(&mut bytes);
        let name: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let home = base.join(format!("ultradian-store-{name}"));
        std::fs::create_dir_all(&home).expect("temp home");
        assert!(home.starts_with(&base));
        Self { store: None, home }
    }

    pub(crate) fn store(&self) -> &Store {
        self.store.as_ref().expect("open")
    }

    pub(crate) fn raw(&self) -> Connection {
        Connection::open(self.home.join("ultradian.db")).expect("raw open")
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        self.store.take();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// A manual schedule running `command` in /tmp, to adjust with `..`.
pub(crate) fn new_schedule(name: &str, trigger: Trigger, command: &[&str]) -> NewSchedule {
    NewSchedule {
        name: name.into(),
        group: None,
        trigger,
        gate: None,
        gate_mode: "output".into(),
        command: command.iter().map(|part| (*part).to_owned()).collect(),
        working_directory: "/tmp".into(),
        timeout_ms: None,
        catch_up_ms: 0,
    }
}

pub(crate) fn begin(store: &Store, schedule: &Schedule, trigger: &str) -> Run {
    store
        .begin_run(schedule, trigger, false)
        .expect("begins")
        .expect("not busy")
}

fn add(store: &Store, name: &str, trigger: Trigger, catch_up_ms: i64) -> Schedule {
    store
        .add_schedule(NewSchedule {
            catch_up_ms,
            ..new_schedule(name, trigger, &["echo", "ok"])
        })
        .expect("adds")
}

fn raw(home: &Path) -> Connection {
    Connection::open(home.join("ultradian.db")).expect("raw open")
}

fn orphan_run(home: &Path, id: &str, schedule: &Schedule, trigger: &str) {
    raw(home)
        .execute(
            "INSERT INTO runs
               (id, schedule_id, schedule_name, machine_id, executor,
                trigger, status, gate_exit, action_exit, started_at, finished_at,
                log_pointer, owner_pid)
             VALUES (?, ?, ?, ?, NULL, ?, 'running', NULL, NULL, ?, NULL, NULL, ?)",
            params![
                id,
                schedule.id,
                schedule.name,
                "test",
                trigger,
                now_ms(),
                3_999_999
            ],
        )
        .expect("orphan run");
}

#[test]
fn a_fresh_database_lands_on_the_latest_version_and_reopens_cleanly() {
    let temp = TempStore::new();
    let home = temp.home.clone();
    assert_eq!(user_version(&raw(&home)).expect("version"), SCHEMA_VERSION);
    Store::open(&home).expect("reopens");
    let mode = |path: &Path| {
        std::fs::metadata(path)
            .expect("exists")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(&home), 0o700);
    assert_eq!(mode(&home.join("logs")), 0o700);
    assert_eq!(mode(&home.join("ultradian.db")), 0o600);
    let journal: String = raw(&home)
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("mode");
    assert_eq!(journal, "wal");
}

#[test]
fn refuses_a_database_written_by_a_newer_release() {
    let home = TempStore::empty();
    raw(&home.home)
        .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
        .expect("bump");
    let Err(error) = Store::open(&home.home) else {
        panic!("opened a newer database")
    };
    assert_eq!(error.code, "database_too_new");
    assert_eq!(error.exit_code, exit::CONFIG);
    assert!(error.message.contains("newer than this release"));
    // Refusing changes nothing: the version stays where the newer build put it.
    assert_eq!(
        user_version(&raw(&home.home)).expect("version"),
        SCHEMA_VERSION + 1
    );
}

/// The shape 0.1.x wrote, before schemas were versioned; `extra` adds the
/// columns later 0.1 builds grew.
fn legacy_database(schedules_extra: &str, runs_extra: &str) -> TempStore {
    let home = TempStore::empty();
    let db = raw(&home.home);
    db.execute_batch(&format!(
        "CREATE TABLE schedules (
          id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE,
          trigger_kind TEXT NOT NULL, trigger_value TEXT, gate TEXT,
          command TEXT NOT NULL, working_directory TEXT NOT NULL,
          status TEXT NOT NULL DEFAULT 'active', created_at INTEGER NOT NULL,
          next_fire_at INTEGER{schedules_extra});
        CREATE TABLE runs (
          id TEXT PRIMARY KEY, schedule_id TEXT NOT NULL,
          schedule_name TEXT NOT NULL, machine_id TEXT NOT NULL, executor TEXT,
          trigger TEXT NOT NULL, status TEXT NOT NULL, gate_exit INTEGER,
          action_exit INTEGER, started_at INTEGER NOT NULL, finished_at INTEGER,
          log_pointer TEXT, owner_pid INTEGER NOT NULL{runs_extra});
        CREATE INDEX runs_by_schedule ON runs (schedule_name, started_at DESC);
        CREATE TABLE daemon (id INTEGER PRIMARY KEY CHECK (id = 1),
          pid INTEGER NOT NULL, started_at INTEGER NOT NULL,
          heartbeat_at INTEGER NOT NULL);"
    ))
    .expect("legacy shape");
    home
}

#[test]
fn carries_a_0_1_database_over_with_its_schedules_and_run_history() {
    let home = legacy_database(
        ", timeout_ms INTEGER, schedule_group TEXT, kind TEXT NOT NULL DEFAULT 'schedule'",
        ", working_directory TEXT, child_pid INTEGER",
    );
    raw(&home.home)
        .execute_batch(
            "INSERT INTO schedules (id, name, trigger_kind, trigger_value,
              gate, command, working_directory, status, timeout_ms, schedule_group,
              created_at, next_fire_at, kind) VALUES
              ('s1', 'tick', 'cron', '0 * * * *', 'true', '[\"echo\",\"ok\"]', '/tmp',
               'paused', 60000, 'ops', 1000, 5000, 'schedule'),
              ('s3', 'daily', 'interval', '1d', NULL, '[\"true\"]', '/srv',
               'active', NULL, NULL, 1000, 3000, 'schedule');
            INSERT INTO runs (id, schedule_id, schedule_name, machine_id,
              executor, trigger, status, gate_exit, action_exit, started_at,
              finished_at, log_pointer, owner_pid, working_directory, child_pid)
              VALUES
              ('r2', 's1', 'tick', 'box', 'echo', 'manual', 'failed', 0, 1, 3000,
               3100, '/logs/r2.log', 42, '/tmp', 7),
              ('r1', 's1', 'tick', 'box', 'echo', 'scheduled', 'succeeded', 0, 0,
               2000, 2100, '/logs/r1.log', 42, '/tmp', NULL);
            INSERT INTO daemon VALUES (1, 4242, 900, 950);",
        )
        .expect("legacy rows");

    let store = Store::open(&home.home).expect("upgrades");
    let schedule = store.get_schedule("tick").expect("reads").expect("kept");
    assert_eq!(schedule.command, ["echo", "ok"]);
    assert_eq!(schedule.gate.as_deref(), Some("true"));
    assert_eq!(schedule.group.as_deref(), Some("ops"));
    assert_eq!(schedule.id, "s1");
    assert_eq!(schedule.next_fire_at, Some(5000));
    assert_eq!(schedule.status, "paused");
    assert_eq!(schedule.timeout_ms, Some(60_000));
    assert_eq!(schedule.updated_at, 1000);
    assert_eq!(
        schedule.trigger,
        Trigger::Cron {
            expression: "0 * * * *".into(),
            timezone: None
        }
    );
    assert_eq!(schedule.working_directory, "/tmp");
    // 0.1 stored the interval as typed; a day is 86,400 seconds.
    let daily = store.get_schedule("daily").expect("reads").expect("kept");
    assert_eq!(daily.trigger, Trigger::Every { seconds: 86_400 });
    let runs = store.export_runs(0, None).expect("runs");
    assert_eq!(
        runs.iter().map(|run| run.id.as_str()).collect::<Vec<_>>(),
        ["r1", "r2"]
    );
    assert_eq!(runs[1].action_exit, Some(1));
    assert_eq!(runs[1].log_pointer.as_deref(), Some("/logs/r2.log"));
    assert_eq!(
        (runs[1].status.as_str(), runs[1].trigger.as_str()),
        ("failed", "manual")
    );
    let daemon = store.read_daemon().expect("reads").expect("kept");
    assert_eq!((daemon.pid, daemon.version.as_str()), (4242, "0.1"));
    drop(store);

    let reopened = raw(&home.home);
    assert_eq!(user_version(&reopened).expect("version"), SCHEMA_VERSION);
    let leftovers: i64 = reopened
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'legacy_%'",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(leftovers, 0);
}

#[test]
fn carries_over_the_earliest_0_1_shape_filling_what_it_lacked() {
    let home = legacy_database("", "");
    raw(&home.home)
        .execute_batch(
            "INSERT INTO schedules (id, name, trigger_kind, trigger_value,
              gate, command, working_directory, created_at, next_fire_at) VALUES
              ('s1', 'tick', 'interval', '15m', NULL, '[\"true\"]', '/srv', 1000, 2000);
            INSERT INTO runs (id, schedule_id, schedule_name, machine_id,
              trigger, status, started_at, owner_pid) VALUES
              ('r1', 's1', 'tick', 'box', 'scheduled', 'clean', 1500, 42);",
        )
        .expect("legacy rows");
    let store = Store::open(&home.home).expect("upgrades");
    let schedule = store.get_schedule("tick").expect("reads").expect("kept");
    assert_eq!(schedule.group, None);
    assert_eq!(schedule.status, "active");
    assert_eq!(schedule.timeout_ms, None);
    assert_eq!(schedule.trigger, Trigger::Every { seconds: 900 });
    assert_eq!(schedule.catch_up_ms, 0);
    assert_eq!(schedule.gate_mode, "output");
    let run = store.get_run("r1").expect("reads").expect("kept");
    assert_eq!(run.working_directory.as_deref(), Some("/srv"));
    // The insert trigger's own UPDATE fires the update trigger too, so a new
    // run takes two revisions; 0.2.1 numbers them the same way.
    assert_eq!(run.revision, 2);
    assert_eq!(store.read_daemon().expect("reads"), None);

    // The very first builds had no runs or daemon table at all.
    let home = TempStore::empty();
    raw(&home.home)
        .execute_batch(
            "CREATE TABLE schedules (
              id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE,
              trigger_kind TEXT NOT NULL, trigger_value TEXT, gate TEXT,
              command TEXT NOT NULL, working_directory TEXT NOT NULL,
              status TEXT NOT NULL DEFAULT 'active', created_at INTEGER NOT NULL,
              next_fire_at INTEGER);
            INSERT INTO schedules (id, name, trigger_kind, trigger_value, command,
              working_directory, created_at) VALUES
              ('s1', 'hand', 'manual', NULL, '[\"true\"]', '/srv', 10);",
        )
        .expect("legacy rows");
    let store = Store::open(&home.home).expect("upgrades");
    assert_eq!(store.list_schedules().expect("lists").len(), 1);
    assert_eq!(store.count_runs(None).expect("counts"), 0);
}

#[test]
fn claims_a_due_schedule_exactly_once_per_fire() {
    let temp = TempStore::new();
    let store = temp.store();
    add(store, "tick", Trigger::Every { seconds: 1 }, 0);
    let fire_time = now_ms() + 1500;
    let (first, _) = store.claim_due(fire_time).expect("claims");
    assert_eq!(
        first
            .iter()
            .map(|schedule| schedule.name.as_str())
            .collect::<Vec<_>>(),
        ["tick"]
    );
    assert!(store.claim_due(fire_time).expect("claims").0.is_empty());
    let advanced = store.get_schedule("tick").expect("reads").expect("kept");
    assert_eq!(advanced.next_fire_at, Some(fire_time + 1000));
}

#[test]
fn paused_schedules_never_fire_and_resume_reschedules() {
    let temp = TempStore::new();
    let store = temp.store();
    add(store, "tick", Trigger::Every { seconds: 1 }, 0);
    let paused = store.set_schedule_status("tick", "paused").expect("pauses");
    assert_eq!(paused.next_fire_at, None);
    assert!(
        store
            .claim_due(now_ms() + 60_000)
            .expect("claims")
            .0
            .is_empty()
    );
    let resumed = store
        .set_schedule_status("tick", "active")
        .expect("resumes");
    assert!(resumed.next_fire_at.is_some());
}

#[test]
fn a_late_fire_runs_once_inside_its_catch_up_window_and_is_missed_beyond_it() {
    let temp = TempStore::new();
    let store = temp.store();
    let strict = add(store, "strict", Trigger::Every { seconds: 3600 }, 0);
    add(
        store,
        "forgiving",
        Trigger::Every { seconds: 3600 },
        4 * 3_600_000,
    );
    // Asleep for three hours: three fires slept through for each.
    let wake = strict.created_at + 3 * 3_600_000 + 60_000;
    let (due, missed) = store.claim_due(wake).expect("claims");
    assert_eq!(
        due.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["forgiving"]
    );
    assert_eq!(
        missed.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["strict"]
    );
    let runs = store.list_runs(Some("strict"), 10).expect("lists");
    assert_eq!(runs.len(), 1);
    assert_eq!(
        (runs[0].status.as_str(), runs[0].trigger.as_str()),
        ("missed", "scheduled")
    );
    assert_eq!(runs[0].log_pointer, None);
    assert!(runs[0].finished_at.is_some());
    // Both skip forward to one fire from now, never a burst.
    for name in ["strict", "forgiving"] {
        assert_eq!(
            store
                .get_schedule(name)
                .expect("reads")
                .expect("kept")
                .next_fire_at,
            Some(wake + 3_600_000)
        );
    }
    assert!(store.claim_due(wake + 1000).expect("claims").0.is_empty());
    // Ordinary tick latency is never a miss, even with no catch-up.
    let (mut on_time, _) = store.claim_due(wake + 3_600_000 + 2000).expect("claims");
    on_time.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(
        on_time.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["forgiving", "strict"]
    );

    // At the boundary: late by exactly its window (or the 30 s floor) still
    // runs.
    {
        let temp = TempStore::new();
        let store = temp.store();
        // 0.2.1 misses a fire only when it is later than max(catch-up, 30 s).
        add(store, "strict", Trigger::Every { seconds: 3600 }, 0);
        add(store, "wide", Trigger::Every { seconds: 3600 }, 120_000);
        let names = |schedules: &[Schedule]| {
            let mut names: Vec<String> = schedules.iter().map(|s| s.name.clone()).collect();
            names.sort();
            names
        };
        let first = store
            .get_schedule("strict")
            .expect("reads")
            .expect("kept")
            .next_fire_at
            .expect("next");
        let (due, missed) = store.claim_due(first + ON_TIME_MS).expect("claims");
        assert_eq!(
            (names(&due), names(&missed)),
            (vec!["strict".to_owned(), "wide".to_owned()], vec![])
        );
        let second = first + ON_TIME_MS + 3_600_000;
        let (due, missed) = store.claim_due(second + 120_000).expect("claims");
        assert_eq!(
            (names(&due), names(&missed)),
            (vec!["wide".to_owned()], vec!["strict".to_owned()])
        );
    }
}

#[test]
fn a_schedule_never_has_two_runs_in_flight_queued_or_running() {
    let temp = TempStore::new();
    let store = temp.store();
    let schedule = add(store, "tick", Trigger::Manual, 0);
    let queued = store
        .begin_run(&schedule, "manual", true)
        .expect("begins")
        .expect("not busy");
    assert_eq!(queued.status, "queued");
    assert!(
        queued
            .log_pointer
            .as_deref()
            .is_some_and(|pointer| pointer.ends_with(&format!("{}.log", queued.id)))
    );
    assert!(
        store
            .begin_run(&schedule, "manual", false)
            .expect("begins")
            .is_none()
    );
    let claimed = store.claim_queued(4242).expect("claims");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].0.id, queued.id);
    assert_eq!(
        store
            .get_run(&queued.id)
            .expect("reads")
            .expect("kept")
            .owner_pid,
        4242
    );
    assert!(store.claim_queued(4242).expect("claims").is_empty());
    assert!(
        store
            .begin_run(&schedule, "manual", false)
            .expect("begins")
            .is_none()
    );
    store
        .finish_run(&queued.id, "succeeded", None, None)
        .expect("finishes");
    let next = store
        .begin_run(&schedule, "manual", false)
        .expect("begins")
        .expect("not busy");
    assert_eq!(next.status, "running");
}

#[test]
fn the_runs_cursor_sees_every_change_in_commit_order_late_finishers_included() {
    let temp = TempStore::new();
    let store = temp.store();
    let slow = begin(store, &add(store, "slow", Trigger::Manual, 0), "manual");
    let quick = begin(store, &add(store, "quick", Trigger::Manual, 0), "manual");
    store
        .finish_run(&quick.id, "succeeded", None, Some(0))
        .expect("finishes");
    let first_page = store.export_runs(0, None).expect("exports");
    let ids = |runs: &[Run]| runs.iter().map(|run| run.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&first_page), [slow.id.clone(), quick.id.clone()]);
    let cursor = first_page.last().map_or(0, |run| run.revision);
    assert!(store.export_runs(cursor, None).expect("exports").is_empty());

    // The run that started first finishes last: a started_at cursor would
    // never show it again, the revision cursor does.
    store
        .finish_run(&slow.id, "failed", None, Some(1))
        .expect("finishes");
    let changed = store.export_runs(cursor, None).expect("exports");
    assert_eq!(changed.len(), 1);
    assert_eq!(
        (changed[0].id.as_str(), changed[0].status.as_str()),
        (slow.id.as_str(), "failed")
    );
    assert_eq!(
        ids(&store.export_runs(0, Some(1)).expect("exports")),
        [quick.id]
    );
}

#[test]
fn a_one_shot_job_is_claimed_once_and_never_lists_as_a_schedule() {
    let temp = TempStore::new();
    let store = temp.store();
    let job = store
        .add_once(
            "once-echo-abcdef".into(),
            vec!["echo".into()],
            "/tmp".into(),
            None,
        )
        .expect("adds");
    assert!(job.id.starts_with("job_"));
    assert!(store.list_schedules().expect("lists").is_empty());
    assert!(store.get_schedule(&job.name).expect("reads").is_none());
    let (claimed, _) = store.claim_due(now_ms()).expect("claims");
    assert_eq!(
        claimed.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
        [job.id]
    );
    assert_eq!(claimed[0].kind, "once");
    assert!(
        store
            .claim_due(now_ms() + 60_000)
            .expect("claims")
            .0
            .is_empty()
    );
}

#[test]
fn recovery_interrupts_dead_owners_runs_and_sweeps_only_spent_jobs() {
    let temp = TempStore::new();
    let store = temp.store();
    // A scheduled run owned by a process that no longer exists, written the
    // way a crashed daemon would have left it.
    let tick = add(store, "tick", Trigger::Every { seconds: 3600 }, 0);
    orphan_run(&temp.home, "run_orphan", &tick, "scheduled");
    let spent = store
        .add_once(
            "once-spent-abcdef".into(),
            vec!["echo".into()],
            "/tmp".into(),
            None,
        )
        .expect("adds");
    // The daemon claimed and started that job, then died mid-run.
    store.claim_due(spent.created_at).expect("claims");
    store
        .add_once(
            "once-pending-abcdef".into(),
            vec!["echo".into()],
            "/tmp".into(),
            None,
        )
        .expect("adds");
    orphan_run(&temp.home, "run_once", &spent, "once");
    let (recovered, _) = store.recover().expect("recovers");
    assert_eq!((recovered.interrupted, recovered.swept), (2, 1));
    for id in ["run_orphan", "run_once"] {
        let run = store.get_run(id).expect("reads").expect("kept");
        assert_eq!(run.status, "interrupted", "{id}");
    }
    // The unclaimed job survives, still due, so the daemon runs it now.
    let (pending, _) = store.claim_due(now_ms()).expect("claims");
    assert_eq!(
        pending.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["once-pending-abcdef"]
    );
}

#[test]
fn a_run_owned_by_a_live_process_survives_recovery() {
    let temp = TempStore::new();
    let store = temp.store();
    begin(store, &add(store, "tick", Trigger::Manual, 0), "manual");
    let (recovered, _) = store.recover().expect("recovers");
    assert_eq!(recovered.interrupted, 0);
    assert_eq!(store.active_runs().expect("lists").len(), 1);
}

#[test]
fn names_are_unique_and_lookups_take_ids_or_names() {
    let temp = TempStore::new();
    let store = temp.store();
    let tick = add(store, "tick", Trigger::Manual, 0);
    let Err(error) = store.add_schedule(NewSchedule {
        name: "tick".into(),
        group: None,
        trigger: Trigger::Manual,
        gate: None,
        gate_mode: "output".into(),
        command: vec![],
        working_directory: "/".into(),
        timeout_ms: None,
        catch_up_ms: 0,
    }) else {
        panic!("added a duplicate")
    };
    assert_eq!(
        (error.code.as_str(), error.exit_code),
        ("schedule_exists", exit::USAGE)
    );
    assert_eq!(
        store.require_schedule(&tick.id).expect("by id").name,
        "tick"
    );
    let missing = store.require_schedule("nobody").unwrap_err();
    assert_eq!(
        (missing.code.as_str(), missing.message.as_str()),
        ("schedule_not_found", "No schedule named \"nobody\".")
    );
}

#[test]
fn cancel_finishes_a_run_once_and_rm_cancels_queued_fires() {
    let temp = TempStore::new();
    let store = temp.store();
    let schedule = add(store, "tick", Trigger::Manual, 0);
    let run = store
        .begin_run(&schedule, "manual", true)
        .expect("begins")
        .expect("not busy");
    let canceled = store.cancel_run(&run.id).expect("cancels");
    assert_eq!(canceled.status, "canceled");
    assert!(canceled.finished_at.is_some() && canceled.revision > run.revision);
    assert_eq!(store.cancel_run(&run.id).unwrap_err().code, "run_finished");
    assert_eq!(
        store.cancel_run("run_nope").unwrap_err().code,
        "run_not_found"
    );

    let queued = store
        .begin_run(&schedule, "manual", true)
        .expect("begins")
        .expect("not busy");
    store.remove_schedule("tick").expect("removes");
    assert_eq!(
        store
            .get_run(&queued.id)
            .expect("reads")
            .expect("kept")
            .status,
        "canceled"
    );
    assert_eq!(store.count_runs(Some("tick")).expect("counts"), 2);
}

#[test]
fn prune_deletes_old_finished_runs_and_their_logs() {
    let temp = TempStore::new();
    let store = temp.store();
    let schedule = add(store, "tick", Trigger::Manual, 0);
    let old = begin(store, &schedule, "manual");
    let pointer = PathBuf::from(old.log_pointer.clone().expect("has a log"));
    std::fs::create_dir_all(pointer.parent().expect("day folder")).expect("log folder");
    std::fs::write(&pointer, "12345").expect("log");
    store
        .finish_run(&old.id, "succeeded", None, Some(0))
        .expect("finishes");
    let running = begin(store, &add(store, "other", Trigger::Manual, 0), "manual");
    let (removed, freed) = store.prune_history(now_ms() + 1, None).expect("prunes");
    assert_eq!((removed, freed), (1, 5));
    assert!(!pointer.exists());
    assert!(!temp.home.join("logs").join("tick").exists());
    assert!(store.get_run(&running.id).expect("reads").is_some());
}

#[test]
fn resolves_paths_like_node() {
    let base = Path::new("/srv/work");
    assert_eq!(
        resolve_path(base, Path::new("d")),
        PathBuf::from("/srv/work/d")
    );
    assert_eq!(
        resolve_path(base, Path::new("../x/./y/")),
        PathBuf::from("/srv/x/y")
    );
    assert_eq!(
        resolve_path(base, Path::new("/a/../../b")),
        PathBuf::from("/b")
    );
    assert_eq!(
        resolve_path(base, Path::new(".")),
        PathBuf::from("/srv/work")
    );
}

#[test]
fn group_ids_are_never_mistaken_for_live_processes() {
    assert!(is_pid_alive(i64::from(std::process::id())));
    assert!(!is_pid_alive(0));
    assert!(!is_pid_alive(-1));
}

/// sqlite_master of a version 1 database, as 0.2.1 through 0.3.1 created
/// it: (type, name, table, sql). The SQL text is kept byte for byte,
/// indentation included.
const SCHEMA_V1: [(&str, &str, &str, Option<&str>); 12] = [
    (
        "table",
        "counters",
        "counters",
        Some("CREATE TABLE counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL)"),
    ),
    (
        "table",
        "daemon",
        "daemon",
        Some(
            r"CREATE TABLE daemon (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        pid INTEGER NOT NULL,
        version TEXT NOT NULL,
        started_at INTEGER NOT NULL,
        heartbeat_at INTEGER NOT NULL
      )",
        ),
    ),
    (
        "table",
        "runs",
        "runs",
        Some(
            r"CREATE TABLE runs (
        id TEXT PRIMARY KEY,
        schedule_id TEXT NOT NULL,
        schedule_name TEXT NOT NULL,
        machine_id TEXT NOT NULL,
        working_directory TEXT,
        executor TEXT,
        trigger TEXT NOT NULL,
        status TEXT NOT NULL,
        gate_exit INTEGER,
        action_exit INTEGER,
        started_at INTEGER NOT NULL,
        finished_at INTEGER,
        log_pointer TEXT,
        owner_pid INTEGER NOT NULL,
        pgid INTEGER,
        revision INTEGER NOT NULL DEFAULT 0
      )",
        ),
    ),
    (
        "index",
        "runs_by_revision",
        "runs",
        Some("CREATE INDEX runs_by_revision ON runs (revision)"),
    ),
    (
        "index",
        "runs_by_schedule",
        "runs",
        Some("CREATE INDEX runs_by_schedule ON runs (schedule_name, started_at DESC)"),
    ),
    (
        "trigger",
        "runs_revision_insert",
        "runs",
        Some(
            r"CREATE TRIGGER runs_revision_insert
        AFTER INSERT ON runs
        BEGIN
          UPDATE counters SET value = value + 1 WHERE name = 'runs';
          UPDATE runs SET revision = (SELECT value FROM counters WHERE name = 'runs')
          WHERE rowid = NEW.rowid;
        END",
        ),
    ),
    (
        "trigger",
        "runs_revision_update",
        "runs",
        Some(
            r"CREATE TRIGGER runs_revision_update
        AFTER UPDATE ON runs
        BEGIN
          UPDATE counters SET value = value + 1 WHERE name = 'runs';
          UPDATE runs SET revision = (SELECT value FROM counters WHERE name = 'runs')
          WHERE rowid = NEW.rowid;
        END",
        ),
    ),
    (
        "table",
        "schedules",
        "schedules",
        Some(
            r"CREATE TABLE schedules (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL UNIQUE,
        kind TEXT NOT NULL DEFAULT 'schedule',
        schedule_group TEXT,
        trigger_kind TEXT NOT NULL,
        trigger_value TEXT,
        timezone TEXT,
        gate TEXT,
        gate_mode TEXT NOT NULL DEFAULT 'output',
        command TEXT NOT NULL,
        working_directory TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'active',
        timeout_ms INTEGER,
        catch_up_ms INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        next_fire_at INTEGER
      )",
        ),
    ),
    ("index", "sqlite_autoindex_counters_1", "counters", None),
    ("index", "sqlite_autoindex_runs_1", "runs", None),
    ("index", "sqlite_autoindex_schedules_1", "schedules", None),
    ("index", "sqlite_autoindex_schedules_2", "schedules", None),
];

#[test]
fn a_fresh_database_has_0_2_1s_schema_and_the_agent_session_column() {
    let temp = TempStore::new();
    let db = temp.raw();
    let mut query = db
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY name")
        .expect("query");
    let rows: Vec<(String, String, String, Option<String>)> = query
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("reads")
        .collect::<std::result::Result<_, _>>()
        .expect("rows");
    // SQLite records an added column by editing the table's SQL in place.
    let want: Vec<(String, String, String, Option<String>)> = SCHEMA_V1
        .iter()
        .map(|(kind, name, table, sql)| {
            let sql = sql.map(|sql| match *name {
                "runs" => sql.replace("\n      )", "\n      , agent_session_id TEXT)"),
                _ => sql.to_owned(),
            });
            ((*kind).into(), (*name).into(), (*table).into(), sql)
        })
        .collect();
    assert_eq!(rows, want);
    let counter: (String, i64) = db
        .query_row("SELECT name, value FROM counters", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .expect("one counter");
    assert_eq!(counter, ("runs".to_owned(), 0));
}

/// Rows 0.2.1 (and 0.3.x, which kept its shape) wrote for a cron schedule with a gate, an interval with an
/// awkward command, a paused manual one, a one-shot job, a canceled run and
/// a queued one.
const ROWS_0_2_1: &str = r#"
INSERT INTO schedules VALUES('schedule_0muwagwb6ibl669rdzg','tick','schedule','batch','cron','0 9 * * 1-5','Australia/Sydney','sh "gate.sh"','exit','["/bin/sh","run.sh"]','/home/casey/work/automation','active',21600000,1800000,1791267582354,1791267582354,1791324000000);
INSERT INTO schedules VALUES('schedule_0muwagwd9qn2908po22','hourly','schedule','batch','every','3600',NULL,NULL,'output','["echo","quote \" and ü"]','/home/casey/work','active',NULL,0,1791267582429,1791267582429,1791271182429);
INSERT INTO schedules VALUES('schedule_0muwagwf14c1q9gki7w','hand','schedule',NULL,'manual',NULL,NULL,NULL,'output','["true"]','/home/casey/work','paused',NULL,0,1791267582493,1791267582557,NULL);
INSERT INTO schedules VALUES('job_0muwagwocru1dc14c6q','once-import-ak3kjp','once',NULL,'manual',NULL,NULL,NULL,'output','["./import.sh"]','/home/casey/work','active',NULL,0,1791267582828,1791267582828,1791267582828);
INSERT INTO runs VALUES('run_0muwagwin000yo2gzxl','schedule_0muwagwb6ibl669rdzg','tick','casey-mbp','/home/casey/work/automation',NULL,'manual','canceled',NULL,NULL,1791267582623,1791267582762,'/home/casey/state/logs/tick/2026-10-06/run_0muwagwin000yo2gzxl.log',15849,NULL,5);
INSERT INTO runs VALUES('run_0muwagwki0iie4he5ki','schedule_0muwagwd9qn2908po22','hourly','casey-mbp','/home/casey/work',NULL,'manual','queued',NULL,NULL,1791267582690,NULL,'/home/casey/state/logs/hourly/2026-10-06/run_0muwagwki0iie4he5ki.log',15851,NULL,4);
INSERT INTO counters VALUES('runs',5);
"#;

#[test]
fn carries_a_version_1_database_over_reading_its_rows_as_written() {
    let temp = TempStore::empty();
    let db = temp.raw();
    let sql = |kind: &'static str| {
        SCHEMA_V1
            .iter()
            .filter(move |row| row.0 == kind)
            .filter_map(|row| row.3)
    };
    for statement in sql("table") {
        db.execute_batch(statement).expect("table");
    }
    // Rows go in before the triggers, which would renumber revisions.
    db.execute_batch(ROWS_0_2_1).expect("rows");
    for statement in sql("index").chain(sql("trigger")) {
        db.execute_batch(statement).expect("index or trigger");
    }
    db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA user_version = 1;")
        .expect("version");
    drop(db);

    let store = Store::open(&temp.home).expect("opens");
    let names: Vec<String> = store
        .list_schedules()
        .expect("lists")
        .into_iter()
        .map(|schedule| schedule.name)
        .collect();
    assert_eq!(names, ["hand", "hourly", "tick"]);
    let tick = store.get_schedule("tick").expect("reads").expect("kept");
    assert_eq!(
        tick.trigger,
        Trigger::Cron {
            expression: "0 9 * * 1-5".into(),
            timezone: Some("Australia/Sydney".into())
        }
    );
    assert_eq!(tick.gate.as_deref(), Some("sh \"gate.sh\""));
    assert_eq!(
        (tick.gate_mode.as_str(), tick.group.as_deref()),
        ("exit", Some("batch"))
    );
    assert_eq!(
        (tick.timeout_ms, tick.catch_up_ms, tick.next_fire_at),
        (Some(21_600_000), 1_800_000, Some(1_791_324_000_000))
    );
    assert_eq!(tick.command, ["/bin/sh", "run.sh"]);
    assert_eq!(tick.working_directory, "/home/casey/work/automation");
    let hourly = store.get_schedule("hourly").expect("reads").expect("kept");
    assert_eq!(hourly.trigger, Trigger::Every { seconds: 3600 });
    assert_eq!(hourly.command, ["echo", "quote \" and ü"]);
    let hand = store.get_schedule("hand").expect("reads").expect("kept");
    assert_eq!(
        (hand.status.as_str(), &hand.trigger),
        ("paused", &Trigger::Manual)
    );

    let runs = store.export_runs(0, None).expect("runs");
    let seen: Vec<(&str, &str, i64)> = runs
        .iter()
        .map(|run| {
            (
                run.schedule_name.as_str(),
                run.status.as_str(),
                run.revision,
            )
        })
        .collect();
    assert_eq!(seen, [("hourly", "queued", 4), ("tick", "canceled", 5)]);
    // Runs from before the agent session column carry it as null.
    assert!(runs.iter().all(|run| run.agent_session_id.is_none()));
    assert_eq!(user_version(&temp.raw()).expect("version"), SCHEMA_VERSION);
    assert_eq!(runs[1].finished_at, Some(1_791_267_582_762));
    assert_eq!(
        runs[1].log_pointer.as_deref(),
        Some("/home/casey/state/logs/tick/2026-10-06/run_0muwagwin000yo2gzxl.log")
    );
    // The queued run and the one-shot job are still there for a daemon.
    assert_eq!(store.claim_queued(4242).expect("claims").len(), 1);
    let (due, _) = store.claim_due(1_791_267_600_000).expect("claims");
    assert_eq!(
        due.iter().map(|job| job.kind.as_str()).collect::<Vec<_>>(),
        ["once"]
    );
}
