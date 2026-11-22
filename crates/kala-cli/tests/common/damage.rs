#![allow(dead_code)]
//! Byte-level helpers to damage a world file (no test-hooks needed): where
//! each transaction sits, a copy of a world, a snapshot of a directory.
//! Layout (log format v4): `header(12) txn*`, `txn = begin(25) body`,
//! `begin = 0x07 | tx u64 | body_len u64 | body_crc u32 | begin_crc u32` (LE).

use std::path::{Path, PathBuf};

pub const HEADER_LEN: usize = 12;
pub const BEGIN_LEN: usize = 25;

#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub tx: u64,
    pub start: usize,
    pub body_start: usize,
    pub end: usize,
}

/// Parses a well-formed file by its begin records (CRCs are not checked).
pub fn spans(bytes: &[u8]) -> Vec<Span> {
    let mut out = Vec::new();
    let mut at = HEADER_LEN;
    while at < bytes.len() {
        assert!(at + BEGIN_LEN <= bytes.len(), "truncated begin at {at}");
        assert_eq!(bytes[at], 0x07, "not a begin record at {at}");
        let tx = u64::from_le_bytes(bytes[at + 1..at + 9].try_into().unwrap());
        let len = u64::from_le_bytes(bytes[at + 9..at + 17].try_into().unwrap()) as usize;
        let body_start = at + BEGIN_LEN;
        let end = body_start + len;
        assert!(end <= bytes.len());
        out.push(Span {
            tx,
            start: at,
            body_start,
            end,
        });
        at = end;
    }
    out
}

/// Copies the world file to a fresh temp dir.
pub fn copy_of(src: &Path) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    std::fs::copy(src, &p).unwrap();
    (dir, p)
}

/// A temp dir holding `bytes` as `w.kala`.
pub fn file_with(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    std::fs::write(&p, bytes).unwrap();
    (dir, p)
}

/// Every file in `dir` with its bytes, sorted by name.
pub fn dir_snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).unwrap_or_default(),
            )
        })
        .collect();
    v.sort();
    v
}
