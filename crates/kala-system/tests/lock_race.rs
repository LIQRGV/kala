//! The writer lock must not report `Busy` when nobody else writes.
//!
//! A process that spawns children from another thread briefly shares its
//! open files with each child (between fork and exec). A lock on a file
//! closed in that window is still held for a moment, so reopening the system
//! world right after bootstrapping it can find it locked although no other
//! writer exists. `kala`'s own test binaries do exactly this.
#![cfg(unix)]
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn bootstrap_then_write_is_not_busy_while_other_threads_spawn_processes() {
    let stop = Arc::new(AtomicBool::new(false));
    let spawners: Vec<_> = (0..4)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = Command::new("true").status();
                }
            })
        })
        .collect();
    let mut failures = Vec::new();
    for i in 0..300 {
        let dir = tempfile::tempdir().unwrap();
        if let Err(e) = kala_system::system::add(dir.path(), "author@localhost", None) {
            failures.push((i, e));
        }
    }
    stop.store(true, Ordering::Relaxed);
    for s in spawners {
        s.join().unwrap();
    }
    assert!(failures.is_empty(), "{failures:?}");
}
