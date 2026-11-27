//! Format 6: every transaction's Commit checksum also covers the previous
//! transaction's, and genesis chains from the header (see `kala_core::log`,
//! "The checksum chain"). A log whose earlier transaction was replaced by
//! another *valid* transaction is Corrupt, old-format files are refused, and
//! torn / damaged tails are still judged from the local checksums alone.

mod common;

use common::{Span, chained, crc, first_broken_link, spans};
use kala_core::doctor::{Finding, doctor, doctor_log};
use kala_core::log::{EntryBody, LOG_FORMAT_VERSION, LogFault, read_log};
use kala_core::*;
use proptest::prelude::*;
use std::path::{Path, PathBuf};

fn schema(attr: &str) -> Schema {
    Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        attr,
        ValueType::Str,
        Kind::State,
        Cardinality::Many,
    ))
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

/// genesis, greg, `tag = earlier @-10`, `tag = fixed @0`, `tag = last @5`:
/// two builds differing only in `earlier` (same length) differ in tx#2 only,
/// apart from what the chain forces.
fn build(path: &Path, attr: &str, earlier: &str) -> EntityId {
    let mut w = World::create(path, schema(attr)).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    w.assert(greg, attr, s(earlier), Some(Tick(-10))).unwrap();
    w.assert(greg, attr, s("fixed"), Some(Tick(0))).unwrap();
    w.assert(greg, attr, s("last"), Some(Tick(5))).unwrap();
    assert!(doctor(&w, &Default::default()).is_clean());
    greg
}

fn read(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

/// The names in `dir`, sorted: nothing may appear (no quarantine file).
fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// Every begin record and body checksum of a well-formed file is right:
/// nothing transaction-local can tell the file was edited.
fn local_checksums_hold(bytes: &[u8]) -> bool {
    spans(bytes).iter().all(|sp| {
        let begin = &bytes[sp.start..sp.body_start];
        let body_crc = u32::from_le_bytes(begin[17..21].try_into().unwrap());
        let begin_crc = u32::from_le_bytes(begin[21..25].try_into().unwrap());
        crc(&begin[..21]) == begin_crc && crc(&bytes[sp.body_start..sp.end]) == body_crc
    })
}

/// `World::open`, `open_refusing_damaged`, `open_read_only`, `read_log` and
/// doctor all refuse `path` as Corrupt / a store-integrity finding, and none
/// of them changes the file or its directory.
fn assert_corrupt_everywhere(path: &Path) {
    let before = read(path);
    let dir = path.parent().unwrap();
    let names = listing(dir);
    type Open = fn(&Path) -> Result<World, Error>;
    let opens: [(&str, Open); 3] = [
        ("open", World::open),
        ("open_refusing_damaged", World::open_refusing_damaged),
        ("open_read_only", World::open_read_only),
    ];
    for (name, open) in opens {
        match open(path) {
            Err(Error::Corrupt(_)) => {}
            Err(e) => panic!("{name}: expected Corrupt, got {e:?}"),
            Ok(w) => panic!(
                "{name}: a log with a replaced transaction opened (latest {:?}, recovery {:?})",
                w.latest_tx(),
                w.recovery()
            ),
        }
    }
    assert!(matches!(read_log(path), Err(Error::Corrupt(_))));
    assert!(!doctor_log(path, &Default::default()).unwrap().is_clean());
    assert_eq!(read(path), before, "the file must be untouched");
    assert_eq!(listing(dir), names, "nothing may be created next to it");
}

/// Two worlds built alike except for `tx`, whose bytes in `a` are replaced
/// by `b`'s (the same length). Returns `a`'s path (in a kept temp dir).
fn spliced(tx: usize, a: (&str, &str), b: (&str, &str)) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("a.kala");
    let pb = dir.path().join("b.kala");
    build(&pa, a.0, a.1);
    build(&pb, b.0, b.1);
    let mut aa = read(&pa);
    let bb = read(&pb);
    let sp: Vec<Span> = spans(&aa);
    assert_eq!(sp, spans(&bb), "both builds must frame alike");
    assert_eq!(sp.len(), 5);
    let r = sp[tx].start..sp[tx].end;
    assert_ne!(aa[r.clone()], bb[r.clone()], "tx#{tx} must differ");
    let untouched_after = aa[sp[tx].end..].to_vec();
    aa[r.clone()].copy_from_slice(&bb[r]);
    assert_eq!(aa[sp[tx].end..], untouched_after[..]);
    assert!(
        local_checksums_hold(&aa),
        "the splice must be invisible to the local checksums"
    );
    std::fs::remove_file(&pb).unwrap();
    std::fs::write(&pa, &aa).unwrap();
    (dir, pa)
}

#[test]
fn every_commit_checksum_chains_from_the_previous_one_and_genesis_from_the_header() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    build(&p, "tag", "alpha");
    let b = read(&p);
    // Spelled out once, independently of the helper: the documented layout.
    let mut link = crc(&b[..12]);
    for sp in spans(&b) {
        let stored = u32::from_le_bytes(b[sp.end - 4..sp.end].try_into().unwrap());
        let mut covered = link.to_le_bytes().to_vec();
        covered.extend_from_slice(&b[sp.body_start..sp.end - 4]);
        assert_eq!(stored, crc(&covered), "tx#{}: chained checksum", sp.tx);
        link = stored;
    }
    assert_eq!(first_broken_link(&b), None);
    // The entries carry the same checksums.
    let from_entries: Vec<u32> = read_log(&p)
        .unwrap()
        .iter()
        .filter_map(|e| match e.body {
            EntryBody::Commit { checksum, .. } => Some(checksum),
            _ => None,
        })
        .collect();
    let from_bytes: Vec<u32> = spans(&b)
        .iter()
        .map(|sp| u32::from_le_bytes(b[sp.end - 4..sp.end].try_into().unwrap()))
        .collect();
    assert_eq!(from_entries, from_bytes);
}

#[test]
fn final_checksum_changes_when_an_earlier_same_length_transaction_changes() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.kala");
    let b = dir.path().join("b.kala");
    build(&a, "tag", "alpha");
    build(&b, "tag", "bravo");
    let (aa, bb) = (read(&a), read(&b));
    assert_eq!(spans(&aa), spans(&bb));
    assert_ne!(
        aa[aa.len() - 4..],
        bb[bb.len() - 4..],
        "the final commit must commit to the earlier differing fact"
    );
}

#[test]
fn a_middle_transaction_replaced_by_a_valid_one_of_the_same_length_is_corrupt() {
    let (_d, p) = spliced(2, ("tag", "alpha"), ("tag", "bravo"));
    assert_corrupt_everywhere(&p);
    // The break shows where the chain stops holding: the transaction after
    // the replaced one.
    assert_eq!(first_broken_link(&read(&p)), Some(3));
}

#[test]
fn the_first_transaction_replaced_by_a_valid_one_of_the_same_length_is_corrupt() {
    // Another genesis (the attribute is named differently); later
    // transactions name the attribute by ID, so they replay on either.
    let (_d, p) = spliced(0, ("tag", "alpha"), ("tug", "alpha"));
    assert_corrupt_everywhere(&p);
    assert_eq!(first_broken_link(&read(&p)), Some(1));
}

#[test]
fn a_changed_header_is_never_read_as_this_log() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    build(&p, "tag", "alpha");
    let good = read(&p);
    for at in 0..12 {
        let mut b = good.clone();
        b[at] ^= 0x01;
        std::fs::write(&p, &b).unwrap();
        for open in [
            World::open,
            World::open_read_only,
            World::open_refusing_damaged,
        ] {
            let Err(e) = open(&p) else {
                panic!("byte {at}: a changed header must not open");
            };
            if at < 8 {
                assert_eq!(e, Error::BadMagic, "byte {at}");
            } else {
                assert!(
                    matches!(e, Error::UnsupportedVersion(_)),
                    "byte {at}: {e:?}"
                );
            }
        }
        assert_eq!(read(&p), b);
    }
    // Genesis is bound to the header it was written under: with the header
    // link computed for any other header, its checksum doesn't verify.
    let sp = spans(&good)[0];
    let stored = u32::from_le_bytes(good[sp.end - 4..sp.end].try_into().unwrap());
    let covered = &good[sp.body_start..sp.end - 4];
    assert_eq!(stored, chained(crc(&good[..12]), covered));
    let mut other = good[..12].to_vec();
    other[8] ^= 0x01;
    assert_ne!(stored, chained(crc(&other), covered));
    assert_ne!(stored, crc(covered), "not the unchained checksum either");
}

#[test]
fn doctor_reports_the_broken_link_once_as_a_store_integrity_error() {
    let (_d, p) = spliced(2, ("tag", "alpha"), ("tag", "bravo"));
    let before = read(&p);
    let report = doctor_log(&p, &Default::default()).unwrap();
    let mismatches: Vec<&Finding> = report
        .findings
        .iter()
        .filter(|f| matches!(f, Finding::ChecksumMismatch { .. }))
        .collect();
    let at = spans(&before)[3].start as u64;
    assert_eq!(
        mismatches,
        [&Finding::ChecksumMismatch {
            tx: TxId(3),
            offset: at
        }],
        "one finding, at the transaction that no longer chains: {report:?}"
    );
    assert_eq!(read(&p), before);
}

#[test]
fn doctor_on_an_open_world_reports_a_log_rewritten_underneath_it() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("a.kala");
    let pb = dir.path().join("b.kala");
    build(&pa, "tag", "alpha");
    build(&pb, "tag", "bravo");
    // A writable handle: doctor re-reads its log file from disk.
    let reader = World::open(&pa).unwrap();
    assert!(doctor(&reader, &Default::default()).is_clean());
    let mut aa = read(&pa);
    let bb = read(&pb);
    let sp = spans(&aa)[2];
    aa[sp.start..sp.end].copy_from_slice(&bb[sp.start..sp.end]);
    std::fs::write(&pa, &aa).unwrap();
    let report = doctor(&reader, &Default::default());
    assert!(
        report
            .findings
            .iter()
            .any(|f| matches!(f, Finding::ChecksumMismatch { tx: TxId(3), .. })),
        "{report:?}"
    );
}

/// A last transaction whose local checksums are right but which chains from
/// something else: every byte is what a writer sent, so it is no torn or
/// damaged tail and must never be cut.
#[test]
fn a_last_transaction_that_does_not_chain_is_corrupt_not_a_tail_to_cut() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    build(&p, "tag", "alpha");
    let mut b = read(&p);
    let sp = *spans(&b).last().unwrap();
    // The format-5 checksum: the body alone, no previous link.
    let unchained = crc(&b[sp.body_start..sp.end - 4]);
    b[sp.end - 4..sp.end].copy_from_slice(&unchained.to_le_bytes());
    common::reseal(&mut b, sp);
    assert!(local_checksums_hold(&b));
    std::fs::write(&p, &b).unwrap();
    assert_corrupt_everywhere(&p);
}

#[test]
fn a_torn_last_transaction_is_still_cut_without_acknowledgement() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let greg = build(&p, "tag", "alpha");
    let b = read(&p);
    let sp = *spans(&b).last().unwrap();
    for cut in [
        sp.start + 1,
        sp.start + 24,
        sp.start + 25,
        sp.end - 5,
        sp.end - 1,
    ] {
        let q = dir.path().join(format!("torn-{cut}.kala"));
        std::fs::write(&q, &b[..cut]).unwrap();
        assert!(matches!(
            World::open_read_only(&q).unwrap().recovery().cause,
            Some(CutCause::Torn)
        ));
        // Torn tails are cut by both writable opens.
        let mut w = World::open_refusing_damaged(&q).unwrap();
        assert_eq!(w.recovery().cause, Some(CutCause::Torn), "cut at {cut}");
        assert_eq!(w.recovery().bytes_cut, (cut - sp.start) as u64);
        assert_eq!(w.latest_tx(), TxId(3));
        assert_eq!(read(&q), b[..sp.start]);
        // Never acknowledged, so writes go on, chained after tx#3.
        w.assert(greg, "tag", s("again"), Some(Tick(7))).unwrap();
        drop(w);
        assert_eq!(first_broken_link(&read(&q)), None);
        let w = World::open(&q).unwrap();
        assert_eq!(w.recovery().bytes_cut, 0);
        assert_eq!(w.latest_tx(), TxId(4));
        assert!(doctor(&w, &Default::default()).is_clean());
    }
}

#[test]
fn a_damaged_last_transaction_still_requires_acknowledgement() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let greg = build(&p, "tag", "alpha");
    let good = read(&p);
    let sp = *spans(&good).last().unwrap();
    // Every body byte, the chained checksum field included.
    for at in [sp.body_start, sp.body_start + 9, sp.end - 4, sp.end - 1] {
        let mut b = good.clone();
        b[at] ^= 0x40;
        std::fs::write(&p, &b).unwrap();
        assert!(matches!(
            World::open_refusing_damaged(&p),
            Err(Error::RecoveryRequired { .. })
        ));
        assert_eq!(read(&p), b, "refused without a cut");
        let mut w = World::open(&p).unwrap();
        assert_eq!(w.recovery().cause, Some(CutCause::Damaged), "byte {at}");
        // Reported, not cut: the bytes stay until the acknowledged commit.
        assert!(w.recovery().quarantine.is_none());
        assert_eq!(read(&p), b, "opened without a cut");
        assert_eq!(w.latest_tx(), TxId(3));
        assert!(matches!(
            w.assert(greg, "tag", s("again"), Some(Tick(7))),
            Err(Error::RecoveryUnacknowledged)
        ));
        assert_eq!(read(&p), b, "refused without a cut");
        w.acknowledge_recovery();
        w.assert(greg, "tag", s("again"), Some(Tick(7))).unwrap();
        let q = w.recovery().quarantine.clone().expect("quarantined");
        assert_eq!(read(&q), b[sp.start..], "the damaged bytes were saved");
        drop(w);
        let after = read(&p);
        assert_eq!(after[..sp.start], good[..sp.start]);
        assert_eq!(first_broken_link(&after), None);
        assert!(doctor(&World::open(&p).unwrap(), &Default::default()).is_clean());
        for q in listing(dir.path()) {
            if q.contains(".torn-") {
                std::fs::remove_file(dir.path().join(q)).unwrap();
            }
        }
    }
}

#[test]
fn damage_followed_by_a_valid_transaction_is_still_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    build(&p, "tag", "alpha");
    let good = read(&p);
    let sp = spans(&good);
    for earlier in &sp[..sp.len() - 1] {
        for at in [earlier.body_start + 5, earlier.end - 1] {
            let mut b = good.clone();
            b[at] ^= 0x40;
            std::fs::write(&p, &b).unwrap();
            assert_corrupt_everywhere(&p);
        }
    }
}

/// A real format-5 file, written by the last build before the chain
/// (`kala create`, then `[[:create :CHARACTER :greg] [:assert :greg :name
/// "Greg" :at -300]]`): 249 bytes, clean under that build.
const FORMAT_5_WORLD: &str = "4b414c414c4f47000500000007000000000000000019000000000000001cdf4421cb9434fd1500000005010000000000000000000000000000005f2d05dd070100000000000000a2000000000000001cdf442167522f9d1a00000009020000000000000001000009000000434841524143544552011a00000003030000000000000001000000000000000104000000677265671f0000000904000000000000000000000000040000006e616d6504000001000000010026000000040500000000000000010000000000000000000000d4feffffffffffff000004477265670000150000000506000000000000000100000000000000ae7b0353";

fn unhex(h: &str) -> Vec<u8> {
    (0..h.len() / 2)
        .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

#[test]
fn an_old_format_file_is_refused_by_every_open_with_the_dump_and_reload_message() {
    assert_eq!(LOG_FORMAT_VERSION, 6);
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("old.kala");
    let old = unhex(FORMAT_5_WORLD);
    assert_eq!(old.len(), 249);
    assert_eq!(old[8..12], 5u32.to_le_bytes());
    std::fs::write(&p, &old).unwrap();
    type Open = fn(&Path) -> Result<World, Error>;
    let opens: [(&str, Open); 3] = [
        ("open", World::open),
        ("open_read_only", World::open_read_only),
        ("open_refusing_damaged", World::open_refusing_damaged),
    ];
    for (name, open) in opens {
        let e = match open(&p) {
            Err(e) => e,
            Ok(w) => panic!("{name}: a format-5 file opened at {:?}", w.latest_tx()),
        };
        assert_eq!(e, Error::UnsupportedVersion(5), "{name}");
        let m = e.to_string();
        for needle in [
            "format version 5",
            "older Kala",
            "kala dump",
            "kala transact --create",
            "older kala binary",
        ] {
            assert!(m.contains(needle), "{name}: {needle:?} missing from: {m}");
        }
        assert_eq!(read(&p), old, "{name}: the old file must be untouched");
    }
    assert_eq!(read_log(&p), Err(Error::UnsupportedVersion(5)));
    assert_eq!(
        doctor_log(&p, &Default::default()),
        Err(Error::UnsupportedVersion(5))
    );
    assert_eq!(listing(dir.path()), ["old.kala"]);
    // The same bytes relabelled as format 6 are not a format-6 log either:
    // nothing is half-read.
    let mut relabelled = old.clone();
    relabelled[8..12].copy_from_slice(&6u32.to_le_bytes());
    std::fs::write(&p, &relabelled).unwrap();
    assert!(matches!(World::open(&p), Err(Error::Corrupt(_))));
    assert_eq!(read(&p), relabelled);
    // A newer version gets no dump advice (this build can't know).
    let e = Error::UnsupportedVersion(LOG_FORMAT_VERSION + 1).to_string();
    assert!(!e.contains("kala dump"), "{e}");
}

#[test]
fn after_a_failed_fsync_the_next_commit_chains_from_the_last_durable_transaction() {
    for fault in [LogFault::FailNextSync, LogFault::FailNextWrite] {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("w.kala");
        let greg = build(&p, "tag", "alpha");
        let durable = read(&p);
        let mut w = World::open(&p).unwrap();
        w.inject_log_fault(fault);
        assert!(matches!(
            w.assert(greg, "tag", s("lost"), Some(Tick(9))),
            Err(Error::Io { .. })
        ));
        assert_eq!(
            read(&p),
            durable,
            "{fault:?}: the failed transaction is cut back"
        );
        assert_eq!(w.latest_tx(), TxId(4));
        // The handle refuses until reopened.
        assert!(w.assert(greg, "tag", s("x"), Some(Tick(9))).is_err());
        drop(w);
        let mut w = World::open(&p).unwrap();
        assert_eq!(w.recovery().bytes_cut, 0);
        w.assert(greg, "tag", s("kept"), Some(Tick(9))).unwrap();
        w.assert(greg, "tag", s("more"), Some(Tick(10))).unwrap();
        drop(w);
        let b = read(&p);
        assert_eq!(b[..durable.len()], durable[..]);
        assert_eq!(first_broken_link(&b), None, "{fault:?}");
        let w = World::open(&p).unwrap();
        assert_eq!(w.recovery().bytes_cut, 0);
        assert_eq!(w.latest_tx(), TxId(6));
        assert!(doctor(&w, &Default::default()).is_clean());
        assert_eq!(
            w.as_of(greg, "tag", Tick(10)).unwrap().len(),
            5,
            "alpha, fixed, last, kept, more; never `lost`"
        );
    }
}

/// What a world answers, for comparing two opens of (supposedly) one log.
fn fingerprint(w: &World) -> String {
    format!("{:?}|{:?}", w.latest_tx(), w.entries().collect::<Vec<_>>())
}

/// Commits `values` (one transaction each, lengths and ticks as given) and
/// returns the file's bytes.
fn build_random(path: &Path, values: &[(String, i64)]) -> Vec<u8> {
    let mut w = World::create(path, schema("tag")).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    for (i, (v, tick)) in values.iter().enumerate() {
        // Unique per transaction, so every assert changes something.
        w.assert(greg, "tag", s(&format!("{i}:{v}")), Some(Tick(*tick)))
            .unwrap();
    }
    drop(w);
    read(path)
}

fn values() -> impl Strategy<Value = Vec<(String, i64)>> {
    prop::collection::vec(("[a-z]{0,12}", -50i64..50), 1..8)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, .. ProptestConfig::default() })]

    /// Whatever was committed reopens, and no single changed byte anywhere
    /// in the file yields a silently different world: it is refused, or (in
    /// the last transaction's body only) reported as a damaged tail that
    /// leaves exactly the log before it.
    #[test]
    fn a_flipped_byte_never_yields_a_silently_different_world(
        vals in values(),
        picks in prop::collection::vec((any::<prop::sample::Index>(), 1u8..=255), 1..24),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("w.kala");
        let good = build_random(&p, &vals);
        prop_assert_eq!(first_broken_link(&good), None);
        let clean = World::open_read_only(&p).unwrap();
        prop_assert_eq!(clean.recovery().bytes_cut, 0);
        prop_assert_eq!(clean.latest_tx(), TxId(vals.len() as u64 + 1));
        prop_assert!(doctor(&clean, &Default::default()).is_clean());
        let sp = spans(&good);
        let last = *sp.last().unwrap();
        // The world without its last transaction, for the damaged-tail case.
        let q = dir.path().join("prefix.kala");
        std::fs::write(&q, &good[..last.start]).unwrap();
        let prefix = fingerprint(&World::open_read_only(&q).unwrap());

        for (index, mask) in picks {
            let at = index.index(good.len());
            let mut b = good.clone();
            b[at] ^= mask;
            let f = dir.path().join("flipped.kala");
            std::fs::write(&f, &b).unwrap();
            match World::open_read_only(&f) {
                Err(Error::Corrupt(_) | Error::BadMagic | Error::UnsupportedVersion(_)) => {}
                Err(e) => prop_assert!(false, "byte {}: unexpected error {:?}", at, e),
                Ok(w) => {
                    prop_assert!(at >= last.body_start, "byte {} opened", at);
                    prop_assert_eq!(w.recovery().cause, Some(CutCause::Damaged));
                    prop_assert_eq!(w.recovery().bytes_cut, (good.len() - last.start) as u64);
                    prop_assert_eq!(fingerprint(&w), prefix.clone());
                    // The writable open agrees, and demands acknowledgement.
let refused = World::open_refusing_damaged(&f);
                    let refused = matches!(refused, Err(Error::RecoveryRequired { .. }));
                    prop_assert!(refused, "byte {}: a damaged tail needs review", at);
                }
            }
            prop_assert_eq!(read(&f), b);
        }
    }

    /// Any transaction but the last, replaced by a valid transaction of the
    /// same length (all later bytes as they were), is Corrupt.
    #[test]
    fn a_replaced_earlier_transaction_is_always_corrupt(
        vals in values().prop_filter("needs a value to change", |v| v.len() >= 2),
        which in any::<prop::sample::Index>(),
        letter in "[A-Z]",
    ) {
        let dir = tempfile::tempdir().unwrap();
        let pa = dir.path().join("a.kala");
        let pb = dir.path().join("b.kala");
        // Change one committed value (not the last one), keeping its length.
        let k = which.index(vals.len() - 1);
        let mut other = vals.clone();
        other[k].0.push_str(&letter);
        let mut same_len = vals.clone();
        same_len[k].0.push('z');
        let aa = build_random(&pa, &same_len);
        let bb = build_random(&pb, &other);
        let sp = spans(&aa);
        prop_assert_eq!(&sp, &spans(&bb));
        let t = sp[k + 2]; // genesis, greg, then the values
        let mut spliced = aa.clone();
        spliced[t.start..t.end].copy_from_slice(&bb[t.start..t.end]);
        prop_assert!(local_checksums_hold(&spliced));
        std::fs::write(&pa, &spliced).unwrap();
        for open in [World::open, World::open_read_only] {
            let corrupt = matches!(open(&pa), Err(Error::Corrupt(_)));
            prop_assert!(corrupt, "a replaced tx#{} must be Corrupt", t.tx);
        }
        prop_assert_eq!(read(&pa), spliced);
        prop_assert_eq!(first_broken_link(&read(&pa)), Some(t.tx + 1));
    }
}
