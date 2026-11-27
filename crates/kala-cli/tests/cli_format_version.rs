//! Log format 6 on the real binary: a file written by an older format is
//! refused by read and write commands and by `kala serve` with the
//! dump-and-reload message and is never modified; `kala doctor` reports a
//! broken checksum chain as a store-integrity error.

mod common;

use common::*;
use std::path::{Path, PathBuf};

/// A real format-5 file, written by the last build before the checksum
/// chain (`kala create`, then `[[:create :CHARACTER :greg] [:assert :greg
/// :name "Greg" :at -300]]`): 249 bytes, clean under that build.
const FORMAT_5_WORLD: &str = "4b414c414c4f47000500000007000000000000000019000000000000001cdf4421cb9434fd1500000005010000000000000000000000000000005f2d05dd070100000000000000a2000000000000001cdf442167522f9d1a00000009020000000000000001000009000000434841524143544552011a00000003030000000000000001000000000000000104000000677265671f0000000904000000000000000000000000040000006e616d6504000001000000010026000000040500000000000000010000000000000000000000d4feffffffffffff000004477265670000150000000506000000000000000100000000000000ae7b0353";

fn old_world() -> Vec<u8> {
    let h = FORMAT_5_WORLD;
    (0..h.len() / 2)
        .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn assert_dump_and_reload(what: &str, o: &std::process::Output) {
    let err = stderr(o);
    assert_eq!(code(o), 3, "{what}: a foreign log is a store error: {err}");
    for needle in [
        "older Kala log format",
        "older kala binary",
        "kala dump",
        "kala transact --create",
        "left untouched",
    ] {
        assert!(
            err.contains(needle),
            "{what}: {needle:?} missing from: {err}"
        );
    }
    assert_eq!(stdout(o), "", "{what}: nothing on stdout");
}

#[test]
fn an_old_format_world_is_refused_by_read_and_write_commands_and_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.kala");
    let old = old_world();
    assert_eq!(old[8..12], 5u32.to_le_bytes());
    std::fs::write(&path, &old).unwrap();
    let p = path.to_str().unwrap();
    let form = dir.path().join("t.edn");
    std::fs::write(&form, "[[:assert :greg :name \"G\" :at 1]]\n").unwrap();
    let f = form.to_str().unwrap();
    let out = dir.path().join("out.edn");
    let o = out.to_str().unwrap();
    let commands: Vec<Vec<&str>> = vec![
        // reads
        vec!["info", p],
        vec!["asof", p, "greg"],
        vec!["history", p, "greg", "name"],
        vec!["query", p, "[:find ?e :where [?e :name \"Greg\"]]"],
        vec!["log", p],
        vec!["schema", p],
        vec!["doctor", p],
        vec!["dump", p],
        vec!["dump", p, "-o", o],
        vec!["export", p],
        // writes
        vec!["transact", p, f],
        vec!["transact", "--acknowledge-recovery", p, f],
        vec!["transact", "--dry-run", p, f],
    ];
    for args in commands {
        let what = args.join(" ");
        let run = kala(&args);
        assert_dump_and_reload(&what, &run);
        assert!(
            stderr(&run).contains("version 5"),
            "{what}: {}",
            stderr(&run)
        );
        assert_eq!(std::fs::read(&path).unwrap(), old, "{what}: file modified");
        assert_eq!(listing(dir.path()), ["old.kala", "t.edn"], "{what}");
    }
}

#[test]
fn kala_serve_and_kala_user_refuse_an_old_format_accounts_database() {
    let dir = tempfile::tempdir().unwrap();
    let system = dir.path().join("system.kala");
    let old = old_world();
    std::fs::write(&system, &old).unwrap();
    let d = dir.path().to_str().unwrap();
    // Refused before anything is bound, so the command returns at once.
    let serve = kala(["serve", "--listen", "127.0.0.1:0", d]);
    assert_dump_and_reload("serve", &serve);
    assert!(
        stderr(&serve).contains("kala dump --all"),
        "{}",
        stderr(&serve)
    );
    assert!(!stderr(&serve).contains("listening"), "{}", stderr(&serve));
    for args in [
        vec!["user", "list", d],
        vec!["user", "add", d, "mara@localhost", "--trust"],
        vec!["user", "drop", d, "root@localhost"],
    ] {
        let what = args.join(" ");
        let run = kala(&args);
        assert_dump_and_reload(&what, &run);
    }
    assert_eq!(std::fs::read(&system).unwrap(), old);
    assert_eq!(listing(dir.path()), ["system.kala"]);
}

/// The world `kala create` + three `kala transact` calls build, with
/// `earlier` as the middle fact's value.
fn built(dir: &Path, name: &str, earlier: &str) -> PathBuf {
    let path = dir.join(name);
    let p = path.to_str().unwrap();
    assert_eq!(code(&kala(["create", p])), 0);
    for (i, form) in [
        "[[:create :CHARACTER :greg]]".to_owned(),
        format!("[[:assert :greg :tag \"{earlier}\" :at -10]]"),
        "[[:assert :greg :tag \"fixed\" :at 0]]".to_owned(),
    ]
    .iter()
    .enumerate()
    {
        let f = dir.join(format!("{name}-{i}.edn"));
        std::fs::write(&f, form).unwrap();
        let o = kala(["transact", p, f.to_str().unwrap()]);
        assert_eq!(code(&o), 0, "{}", stderr(&o));
    }
    path
}

/// `(start, end)` of every transaction of a well-formed log.
fn txn_ranges(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut at = 12;
    let mut out = Vec::new();
    while at < bytes.len() {
        assert_eq!(bytes[at], 0x07);
        let len = u64::from_le_bytes(bytes[at + 9..at + 17].try_into().unwrap()) as usize;
        out.push((at, at + 25 + len));
        at += 25 + len;
    }
    assert_eq!(at, bytes.len());
    out
}

#[test]
fn doctor_reports_a_replaced_transaction_as_a_store_integrity_error() {
    let dir = tempfile::tempdir().unwrap();
    let a = built(dir.path(), "a.kala", "alpha");
    let b = built(dir.path(), "b.kala", "bravo");
    let clean = kala(["doctor", a.to_str().unwrap()]);
    assert_eq!(code(&clean), 0, "{}", stdout(&clean));
    // tx#2 of `a` replaced by `b`'s: a valid transaction of the same length,
    // same tx and entry IDs; every later byte as it was.
    let mut aa = std::fs::read(&a).unwrap();
    let bb = std::fs::read(&b).unwrap();
    let ranges = txn_ranges(&aa);
    assert_eq!(ranges, txn_ranges(&bb));
    assert_eq!(ranges.len(), 4);
    let (s, e) = ranges[2];
    assert_ne!(aa[s..e], bb[s..e]);
    aa[s..e].copy_from_slice(&bb[s..e]);
    std::fs::write(&a, &aa).unwrap();
    let p = a.to_str().unwrap();

    let o = kala(["doctor", p]);
    assert_eq!(code(&o), 1, "findings exit 1: {}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("errors (1):"), "{out}");
    assert!(
        out.contains(&format!(
            "checksum_mismatch: tx#3 at byte offset {}",
            ranges[3].0
        )),
        "{out}"
    );
    assert!(out.contains("doesn't continue the log before it"), "{out}");

    let o = kala(["doctor", "--json", p]);
    assert_eq!(code(&o), 1);
    let j: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(j["clean"], false);
    assert_eq!(j["errors"].as_array().unwrap().len(), 1);
    assert_eq!(j["errors"][0]["kind"], "checksum_mismatch");

    // Every command that opens the world refuses it; nothing is cut.
    let form = dir.path().join("a.kala-0.edn");
    for args in [
        vec!["info", p],
        vec!["asof", p, "greg"],
        vec!["transact", p, form.to_str().unwrap()],
        vec![
            "transact",
            "--acknowledge-recovery",
            p,
            form.to_str().unwrap(),
        ],
    ] {
        let o = kala(&args);
        assert_eq!(code(&o), 3, "{args:?}: {}", stderr(&o));
        assert!(
            stderr(&o).contains("corrupt log"),
            "{args:?}: {}",
            stderr(&o)
        );
        assert!(
            stderr(&o).contains("tx#3: Commit checksum mismatch"),
            "{args:?}: {}",
            stderr(&o)
        );
    }
    assert_eq!(std::fs::read(&a).unwrap(), aa);
    assert!(
        !listing(dir.path()).iter().any(|n| n.contains(".torn-")),
        "no quarantine: {:?}",
        listing(dir.path())
    );
}

/// What the older binary's `kala dump` printed for [`FORMAT_5_WORLD`]:
/// the way across the format change. Restoring it here gives the same
/// entry IDs and tx numbers the old file had.
const FORMAT_5_DUMP: &str = r#";; Kala replayable world dump
;; Kala version: 0.1.0
;; Source format version: 5
;; Latest tx: 1; entries: 6
;; Replay: kala transact --create NEW.kala DUMP
[:genesis
]
;; tx#1
[
  [:node-type :CHARACTER :inferred]
  [:create :CHARACTER :greg]
  [:attribute :name :string :state :one :indexed :inferred]
  [:assert :greg :name "Greg" :at -300 :precision :exact]
]
"#;

#[test]
fn the_older_binarys_dump_restores_with_the_same_entry_ids_and_tx_numbers() {
    use kala_core::log::{EntryBody, LOG_FORMAT_VERSION, read_log};
    let dir = tempfile::tempdir().unwrap();
    let dump = dir.path().join("old.dump.edn");
    std::fs::write(&dump, FORMAT_5_DUMP).unwrap();
    let new = dir.path().join("new.kala");
    let o = kala([
        "transact",
        "--create",
        new.to_str().unwrap(),
        dump.to_str().unwrap(),
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let bytes = std::fs::read(&new).unwrap();
    assert_eq!(bytes[8..12], LOG_FORMAT_VERSION.to_le_bytes());
    // The old file: entries 1 (genesis Commit), then tx#1 = entries 2..=6,
    // the fact at entry#5. Frame kinds in file order, with their entry IDs.
    let old = old_world();
    let kinds = |b: &[u8]| -> Vec<(u64, u8, u64)> {
        let mut out = Vec::new();
        for (s, e) in txn_ranges(b) {
            let tx = u64::from_le_bytes(b[s + 1..s + 9].try_into().unwrap());
            let mut at = s + 25;
            while at < e {
                let len = u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize;
                let id = u64::from_le_bytes(b[at + 5..at + 13].try_into().unwrap());
                out.push((tx, b[at + 4], id));
                at += 4 + len;
            }
        }
        out
    };
    assert_eq!(kinds(&bytes), kinds(&old));
    assert_eq!(kinds(&bytes).len(), 6);
    // Only the Commit checksums differ in meaning: every other payload byte
    // is the old file's.
    let payloads = |b: &[u8]| -> Vec<Vec<u8>> {
        txn_ranges(b)
            .into_iter()
            .map(|(s, e)| b[s + 25..e - 4].to_vec())
            .collect()
    };
    assert_eq!(payloads(&bytes), payloads(&old));
    let log = read_log(&new).unwrap();
    assert!(matches!(
        log.last().unwrap().body,
        EntryBody::Commit { tx, .. } if tx.0 == 1
    ));
    assert!(matches!(log[4].body, EntryBody::Fact { .. }));
    assert_eq!(log[4].id.0, 5);
    let d = kala(["doctor", new.to_str().unwrap()]);
    assert_eq!(code(&d), 0, "{}", stdout(&d));
    // And its own dump is the old dump but for the format line.
    let o = kala(["dump", new.to_str().unwrap()]);
    assert_eq!(
        stdout(&o),
        FORMAT_5_DUMP.replace("format version: 5", "format version: 6")
    );
}
