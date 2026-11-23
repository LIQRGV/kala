//! `World::open_refusing_damaged`: the CLI's write gate, under the lock.
//! A damaged last transaction is refused before anything is cut or
//! quarantined (PR #3 review round 2); clean and torn logs open as with
//! `World::open`.
mod common;
use kala_core::{CutCause, Error, Schema, World};
use std::path::PathBuf;

fn world(dir: &tempfile::TempDir) -> PathBuf {
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, Schema::new().node_type("CHARACTER")).unwrap();
    w.create_entity("CHARACTER", Some("a")).unwrap();
    w.create_entity("CHARACTER", Some("b")).unwrap();
    path
}

fn files(dir: &tempfile::TempDir) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name().into_string().unwrap(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

#[test]
fn a_damaged_tail_is_refused_without_cutting_or_quarantining() {
    let dir = tempfile::tempdir().unwrap();
    let path = world(&dir);
    let mut bytes = std::fs::read(&path).unwrap();
    let last = *common::spans(&bytes).last().unwrap();
    bytes[last.body_start + 6] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();
    let before = files(&dir);
    let tail = (last.end - last.start) as u64;
    for _ in 0..2 {
        match World::open_refusing_damaged(&path) {
            Err(Error::RecoveryRequired { bytes }) => assert_eq!(bytes, tail),
            other => panic!("expected RecoveryRequired, got {other:?}"),
        }
        assert_eq!(files(&dir), before);
    }
    // The lock was released: the explicit path still recovers.
    let w = World::open(&path).unwrap();
    assert_eq!(w.recovery().cause, Some(CutCause::Damaged));
}

#[test]
fn clean_and_torn_logs_open_as_with_world_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = world(&dir);
    let w = World::open_refusing_damaged(&path).unwrap();
    assert_eq!(w.recovery().cause, None);
    drop(w);
    let bytes = std::fs::read(&path).unwrap();
    let last = *common::spans(&bytes).last().unwrap();
    std::fs::write(&path, &bytes[..last.end - 3]).unwrap();
    let w = World::open_refusing_damaged(&path).unwrap();
    assert_eq!(w.recovery().cause, Some(CutCause::Torn));
}
