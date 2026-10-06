//! The foreground daemon loop: heartbeat, claim due and queued fires, sweep
//! stalled runs, prune by retention. Named run_loop because `loop` is a
//! Rust keyword. (Phase 5.)
