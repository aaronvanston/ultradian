//! Ported from 0.2.1's store.test.ts.

use super::*;

/// A store in its own temporary home, removed when dropped.
struct TempStore {
    store: Option<Store>,
    home: PathBuf,
}

impl TempStore {
    fn new() -> Self {
        let home = temp_home();
        let store = Store::open(&home).expect("opens");
        Self {
            store: Some(store),
            home,
        }
    }

    fn store(&self) -> &Store {
        self.store.as_ref().expect("open")
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        self.store.take();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// A fresh folder under the system temp directory, never anywhere else.
pub(crate) fn temp_home() -> PathBuf {
    let base = std::env::temp_dir().canonicalize().expect("temp dir");
    let mut bytes = [0_u8; 8];
    let _ = getrandom::fill(&mut bytes);
    let name: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let home = base.join(format!("ultradian-store-{name}"));
    std::fs::create_dir_all(&home).expect("temp home");
    assert!(home.starts_with(&base));
    home
}

struct TempHome(PathBuf);

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn add(store: &Store, name: &str, trigger: Trigger, catch_up_ms: i64) -> Schedule {
    store
        .add_schedule(NewSchedule {
            name: name.into(),
            group: None,
            trigger,
            gate: None,
            gate_mode: "output".into(),
            command: vec!["echo".into(), "ok".into()],
            working_directory: "/tmp".into(),
            timeout_ms: None,
            catch_up_ms,
        })
        .expect("adds")
}

fn begin(store: &Store, schedule: &Schedule) -> Run {
    store
        .begin_run(schedule, "manual", false)
        .expect("begins")
        .expect("not busy")
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
    let home = TempHome(temp_home());
    raw(&home.0)
        .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
        .expect("bump");
    let Err(error) = Store::open(&home.0) else {
        panic!("opened a newer database")
    };
    assert_eq!(error.code, "database_too_new");
    assert_eq!(error.exit_code, exit::CONFIG);
    assert!(error.message.contains("newer than this release"));
    // Refusing changes nothing: the version stays where the newer build put it.
    assert_eq!(
        user_version(&raw(&home.0)).expect("version"),
        SCHEMA_VERSION + 1
    );
}

/// The shape 0.1.x wrote, before schemas were versioned; `extra` adds the
/// columns later 0.1 builds grew.
fn legacy_database(schedules_extra: &str, runs_extra: &str) -> TempHome {
    let home = TempHome(temp_home());
    let db = raw(&home.0);
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
    raw(&home.0)
        .execute_batch(
            "INSERT INTO schedules (id, name, trigger_kind, trigger_value,
              gate, command, working_directory, status, timeout_ms, schedule_group,
              created_at, next_fire_at, kind) VALUES
              ('s1', 'tick', 'cron', '0 * * * *', 'true', '[\"echo\",\"ok\"]', '/tmp',
               'paused', 60000, 'ops', 1000, 5000, 'schedule');
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

    let store = Store::open(&home.0).expect("upgrades");
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

    let reopened = raw(&home.0);
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
    raw(&home.0)
        .execute_batch(
            "INSERT INTO schedules (id, name, trigger_kind, trigger_value,
              gate, command, working_directory, created_at, next_fire_at) VALUES
              ('s1', 'tick', 'interval', '15m', NULL, '[\"true\"]', '/srv', 1000, 2000);
            INSERT INTO runs (id, schedule_id, schedule_name, machine_id,
              trigger, status, started_at, owner_pid) VALUES
              ('r1', 's1', 'tick', 'box', 'scheduled', 'clean', 1500, 42);",
        )
        .expect("legacy rows");
    let store = Store::open(&home.0).expect("upgrades");
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
}

#[test]
fn a_0_1_database_with_no_runs_or_daemon_table_still_upgrades() {
    let home = TempHome(temp_home());
    raw(&home.0)
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
    let store = Store::open(&home.0).expect("upgrades");
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
fn recovery_marks_dead_owner_runs_interrupted() {
    let temp = TempStore::new();
    let store = temp.store();
    let schedule = add(store, "tick", Trigger::Every { seconds: 1 }, 0);
    // A run owned by a process that no longer exists, written the way a
    // crashed daemon would have left it.
    orphan_run(&temp.home, "run_orphan", &schedule, "scheduled");
    let (recovered, _) = store.recover().expect("recovers");
    assert_eq!(recovered.interrupted, 1);
    assert_eq!(
        store
            .get_run("run_orphan")
            .expect("reads")
            .expect("kept")
            .status,
        "interrupted"
    );
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
    let slow = begin(store, &add(store, "slow", Trigger::Manual, 0));
    let quick = begin(store, &add(store, "quick", Trigger::Manual, 0));
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
fn recovery_interrupts_a_one_shot_run_and_sweeps_only_spent_jobs() {
    let temp = TempStore::new();
    let store = temp.store();
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
    assert_eq!((recovered.interrupted, recovered.swept), (1, 1));
    assert_eq!(
        store
            .get_run("run_once")
            .expect("reads")
            .expect("kept")
            .status,
        "interrupted"
    );
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
    begin(store, &add(store, "tick", Trigger::Manual, 0));
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
    let old = begin(store, &schedule);
    let pointer = PathBuf::from(old.log_pointer.clone().expect("has a log"));
    std::fs::create_dir_all(pointer.parent().expect("day folder")).expect("log folder");
    std::fs::write(&pointer, "12345").expect("log");
    store
        .finish_run(&old.id, "succeeded", None, Some(0))
        .expect("finishes");
    let running = begin(store, &add(store, "other", Trigger::Manual, 0));
    let (removed, freed) = store.prune_history(now_ms() + 1, None).expect("prunes");
    assert_eq!((removed, freed), (1, 5));
    assert!(!pointer.exists());
    assert!(!temp.home.join("logs").join("tick").exists());
    assert!(store.get_run(&running.id).expect("reads").is_some());
}

#[test]
fn the_daemon_lock_has_one_live_holder() {
    let temp = TempStore::new();
    let store = temp.store();
    assert_eq!(
        store
            .claim_daemon(100, "0.3.0", 1, |_| true)
            .expect("claims"),
        None
    );
    let holder = store
        .claim_daemon(200, "0.3.0", 2, |_| true)
        .expect("claims")
        .expect("held");
    assert_eq!(holder.pid, 100);
    assert!(store.heartbeat(100).expect("beats"));
    assert_eq!(
        store
            .claim_daemon(200, "0.3.0", 2, |_| false)
            .expect("claims"),
        None
    );
    assert!(!store.heartbeat(100).expect("beats"));
    store.clear_daemon(200).expect("clears");
    assert_eq!(store.read_daemon().expect("reads"), None);
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
fn a_fire_late_by_exactly_its_window_still_runs() {
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
