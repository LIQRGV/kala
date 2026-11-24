//! Torn tail recovery under log format v2 (docs/slice-2-decisions.md, Item 1).
//! On `World::open`, a last transaction that was cut short (fewer than 25
//! bytes, or `body_len` running past EOF, or a zero-filled tail) is ignored AND
//! cut off: the file is truncated to the end of the last committed transaction.
//! All committed transactions survive; writing afterwards continues tx and
//! entry IDs from the last committed ones. Damage to bytes that are all present
//! is `Corrupt` (file untouched) unless it is in the LAST transaction with no
//! valid begin record after it: then (slice 3 item 0a) it is cut like a torn
//! tail and reported as `CutCause::Damaged`.
mod common;

use kala_core::log::{EntryBody, read_log};
use kala_core::*;
use std::path::{Path, PathBuf};

fn schema() -> Schema {
    let a = |n, t, k, c| AttrDef::new(n, t, k, c);
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(a(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(a("name", ValueType::Str, Kind::State, Cardinality::One))
        .attribute(a("age", ValueType::I64, Kind::State, Cardinality::Many))
}

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn bytes(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

fn len(p: &Path) -> u64 {
    std::fs::metadata(p).unwrap().len()
}

struct Ents {
    greg: EntityId,
    edwin: EntityId,
    varrow: EntityId,
}

const TICKS: [i64; 9] = [i64::MIN, -400, -300, -250, -1, 0, 7, 500, i64::MAX];
const ATTRS: [&str; 3] = ["located_in", "name", "age"];

/// All reads at every canon, as one string. Handles probed include the one
/// the last txn creates.
fn snapshot(w: &World, e: &Ents) -> String {
    let mut out = format!("tx={:?} tick={:?}\n", w.latest_tx(), w.latest_tick());
    for tx in 0..=w.latest_tx().0 + 1 {
        let v = w.at_canon(TxId(tx));
        for en in [e.greg, e.edwin, e.varrow] {
            for a in ATTRS {
                out += &format!("c{tx} {en:?} {a} hist={:?}\n", v.history(en, a));
                for k in TICKS {
                    out += &format!("c{tx} {en:?} {a} @{k} {:?}\n", v.as_of(en, a, Tick(k)));
                }
            }
        }
    }
    for h in ["greg", "edwin", "varrow", "lisa"] {
        out += &format!("{h}={:?}\n", w.entity_by_handle(h));
    }
    out
}

struct Fx {
    _dir: tempfile::TempDir,
    path: PathBuf,
    ents: Ents,
    /// file length after each committed txn, in order (index = tx number)
    lens: Vec<u64>,
}

/// Several committed txns; the last one creates `lisa` and writes facts.
/// `before_last` is the snapshot before the last txn, `after` after it.
fn fixture() -> (Fx, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let mut lens = vec![len(&path)];
    let step = |lens: &mut Vec<u64>| lens.push(len(&path));
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    step(&mut lens);
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    step(&mut lens);
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    step(&mut lens);
    w.assert(greg, "located_in", Value::Ref(varrow), t(-300))
        .unwrap();
    step(&mut lens);
    w.retract(greg, "located_in", Value::Ref(varrow), t(-250))
        .unwrap();
    step(&mut lens);
    w.assert(greg, "name", Value::Str("Greg".into()), t(0))
        .unwrap();
    step(&mut lens);
    w.assert(edwin, "age", Value::I64(30), t(7)).unwrap();
    step(&mut lens);
    let ents = Ents {
        greg,
        edwin,
        varrow,
    };
    let before = snapshot(&w, &ents);
    let l0 = len(&path);
    let mut txn = w.begin();
    let lisa = txn.create_entity("CHARACTER", Some("lisa")).unwrap();
    txn.assert(lisa, "age", Value::I64(9), t(500)).unwrap();
    txn.assert(greg, "located_in", Value::Ref(varrow), t(600))
        .unwrap();
    txn.assert(edwin, "name", Value::Str("Edwin".into()), t(-1))
        .unwrap();
    txn.commit().unwrap();
    let after = snapshot(&w, &ents);
    assert_ne!(before, after);
    assert!(len(&path) > l0);
    lens.push(len(&path));
    drop(w);
    (
        Fx {
            _dir: dir,
            path,
            ents,
            lens,
        },
        before,
        after,
    )
}

fn l0(f: &Fx) -> u64 {
    f.lens[f.lens.len() - 2]
}

fn l1(f: &Fx) -> u64 {
    f.lens[f.lens.len() - 1]
}

fn truncated_copy(src: &Path, k: u64, dir: &Path) -> PathBuf {
    let p = dir.join(format!("cut{k}.kala"));
    std::fs::write(&p, &bytes(src)[..k as usize]).unwrap();
    p
}

fn entry_ids_contiguous(p: &Path) {
    for (i, e) in read_log(p).unwrap().iter().enumerate() {
        assert_eq!(e.id.0, i as u64 + 1, "entry at position {i}");
    }
}

fn open_ok(p: &Path) -> World {
    match World::open(p) {
        Ok(w) => w,
        Err(e) => panic!("open failed: {e:?}"),
    }
}

fn assert_corrupt_untouched(p: &Path) {
    let before = bytes(p);
    match World::open(p) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!("expected Corrupt, got {e:?}"),
        Ok(_) => panic!("expected Corrupt, open succeeded"),
    }
    assert_eq!(bytes(p), before, "file must stay untouched");
}

/// `p` holds the fixture log with the LAST transaction damaged (all its bytes
/// present) and no valid begin record after it. open() must cut it exactly
/// like a torn tail but report `CutCause::Damaged`: file cut back to `keep`,
/// quarantine `<file>.torn-<keep>` holds exactly the cut bytes, the world is
/// the one before the last txn (`before`), and a second open is clean.
/// read_log must ignore the damaged tail without touching the file.
fn assert_cut_as_damaged(p: &Path, keep: u64, before: &str, e: &Ents) {
    let damaged = bytes(p);
    let committed = read_log(p).unwrap();
    assert_eq!(bytes(p), damaged, "read_log must not modify the file");
    {
        let scratch = tempfile::tempdir().unwrap();
        let clean = scratch.path().join("clean.kala");
        std::fs::write(&clean, &damaged[..keep as usize]).unwrap();
        assert_eq!(
            committed,
            read_log(&clean).unwrap(),
            "read_log ignores the damaged tail"
        );
    }
    let w = open_ok(p);
    let r = w.recovery().clone();
    assert_eq!(r.cause, Some(CutCause::Damaged));
    assert_eq!(r.bytes_cut, damaged.len() as u64 - keep);
    let mut q = p.as_os_str().to_owned();
    q.push(format!(".torn-{keep}"));
    let q = PathBuf::from(q);
    assert_eq!(r.quarantine, Some(q.clone()));
    assert_eq!(snapshot(&w, e), before);
    assert_eq!(w.entity_by_handle("lisa"), None);
    drop(w);
    assert_eq!(bytes(p), damaged[..keep as usize]);
    assert_eq!(bytes(&q), damaged[keep as usize..]);
    assert_eq!(
        read_log(p).unwrap(),
        committed,
        "read_log == committed prefix"
    );
    let w = open_ok(p);
    assert_eq!(*w.recovery(), RecoveryReport::default());
}

/// After recovery: writing works, tx = last committed + 1, entry IDs continue
/// from the last committed entry, and a further reopen sees the write.
fn check_writes_continue(p: &Path, e: &Ents) {
    let mut w = open_ok(p);
    let tx = w.latest_tx();
    let n_entries = read_log(p).unwrap().len() as u64;
    let r = w.assert(e.greg, "age", Value::I64(4242), t(12345)).unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(w.latest_tx(), TxId(tx.0 + 1));
    let log = read_log(p).unwrap();
    assert_eq!(log.len() as u64, n_entries + 2);
    assert_eq!(log[n_entries as usize].id.0, n_entries + 1);
    entry_ids_contiguous(p);
    drop(w);
    let w = open_ok(p);
    assert_eq!(w.latest_tx(), TxId(tx.0 + 1));
    assert!(
        w.as_of_value(e.greg, "age", &Value::I64(4242), Tick(12345))
            .unwrap()
    );
}

#[test]
fn truncating_at_every_byte_of_the_last_txn_recovers_to_before_it() {
    let (f, before, after) = fixture();
    let (a, b) = (l0(&f), l1(&f));
    assert!(b - a > 50, "last txn should span many bytes");
    let scratch = tempfile::tempdir().unwrap();
    for k in a..b {
        let p = truncated_copy(&f.path, k, scratch.path());
        let w = match World::open(&p) {
            Ok(w) => w,
            Err(e) => panic!("k={k} (L0={a}, L1={b}): open failed: {e:?}"),
        };
        assert_eq!(snapshot(&w, &f.ents), before, "k={k}");
        assert_eq!(w.entity_by_handle("lisa"), None, "k={k}");
        drop(w);
        assert_eq!(len(&p), a, "k={k}: file must be cut to L0");
        assert_eq!(bytes(&p), bytes(&f.path)[..a as usize], "k={k}");
        check_writes_continue(&p, &f.ents);
    }
    // k == L1: nothing is lost, nothing cut.
    let p = truncated_copy(&f.path, b, scratch.path());
    let w = open_ok(&p);
    assert_eq!(snapshot(&w, &f.ents), after);
    drop(w);
    assert_eq!(len(&p), b);
}

#[test]
fn cutting_a_txn_with_define_entity_removes_the_entity_and_reuses_its_seq() {
    let (f, _, _) = fixture();
    // cut just before the Commit frame (4 + 21 bytes): everything but COMMIT is there
    let scratch = tempfile::tempdir().unwrap();
    let p = truncated_copy(&f.path, l1(&f) - 25, scratch.path());
    let mut w = open_ok(&p);
    assert_eq!(w.entity_by_handle("lisa"), None);
    assert_eq!(len(&p), l0(&f));
    let c = w.create_entity("CHARACTER", Some("lisa")).unwrap();
    assert_eq!(w.format_id(c), "CHARACTER#3");
    assert_eq!(w.entity_by_handle("lisa"), Some(c));
    drop(w);
    assert_eq!(open_ok(&p).entity_by_handle("lisa"), Some(c));
}

#[test]
fn cut_txns_tx_and_entry_numbers_are_reused() {
    let (f, _, _) = fixture();
    let committed = read_log(&f.path).unwrap();
    let last_tx = committed
        .iter()
        .filter_map(|e| match e.body {
            EntryBody::Commit { tx, .. } => Some(tx.0),
            _ => None,
        })
        .next_back()
        .unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let p = truncated_copy(&f.path, l1(&f) - 1, scratch.path());
    let mut w = open_ok(&p);
    assert_eq!(w.latest_tx(), TxId(last_tx - 1));
    let r = w.assert(f.ents.greg, "age", Value::I64(1), t(1)).unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(w.latest_tx(), TxId(last_tx));
    drop(w);
    let log = read_log(&p).unwrap();
    // the new single-fact txn took the cut txn's first entry ID
    let first_cut_id = read_log(&f.path)
        .unwrap()
        .iter()
        .position(|e| matches!(e.body, EntryBody::DefineEntity { handle: Some(ref h), .. } if h == "lisa"))
        .unwrap() as u64
        + 1;
    assert_eq!(log[(first_cut_id - 1) as usize].id.0, first_cut_id);
    entry_ids_contiguous(&p);
}

// Slice 3 item 0a (CHANGES slice 2 rule 6): a damaged LAST txn with no valid
// begin record after it is cut like a torn tail, but reported as Damaged.
// (Slice-2 name: `..._is_corrupt_and_untouched`.) The Corrupt variant, with a
// later valid begin record, is the next test.
#[test]
fn bad_checksum_field_on_the_last_commit_is_cut_as_damaged() {
    let (f, before, _) = fixture();
    let scratch = tempfile::tempdir().unwrap();
    for off in 1..=4u64 {
        let p = scratch.path().join(format!("c{off}.kala"));
        let mut d = bytes(&f.path);
        let n = d.len();
        d[n - off as usize] ^= 0x01;
        std::fs::write(&p, &d).unwrap();
        assert_cut_as_damaged(&p, l0(&f), &before, &f.ents);
    }
}

#[test]
fn bad_checksum_field_on_the_last_commit_is_corrupt_when_a_later_valid_txn_follows() {
    // keeps the slice-2 intent: damage that is NOT at the tail is never cut.
    // A real, fully valid txn is appended through the API after the fixture,
    // then the (former) last txn's Commit checksum is damaged.
    let (f, _, _) = fixture();
    let scratch = tempfile::tempdir().unwrap();
    let base = scratch.path().join("base.kala");
    std::fs::copy(&f.path, &base).unwrap();
    {
        let mut w = open_ok(&base);
        w.assert(f.ents.greg, "age", Value::I64(77), t(8)).unwrap();
    }
    let orig = bytes(&base);
    let spans = common::spans(&orig);
    let damaged = spans[spans.len() - 2];
    for off in 1..=4usize {
        let p = scratch.path().join(format!("cl{off}.kala"));
        let mut d = orig.clone();
        d[damaged.end - off] ^= 0x01;
        std::fs::write(&p, &d).unwrap();
        assert_corrupt_untouched(&p);
        assert_read_log_corrupt_untouched(&p);
    }
}

// Slice 3 item 0a (was `..._is_corrupt_and_untouched`): damaged last txn with
// nothing valid after it is cut as Damaged.
#[test]
fn bad_byte_in_a_fact_payload_of_the_last_txn_is_cut_as_damaged() {
    let (f, before, _) = fixture();
    let scratch = tempfile::tempdir().unwrap();
    // bytes inside the last fact frame, just before the 25-byte Commit frame
    for back in [26u64, 27, 30, 33] {
        let p = scratch.path().join(format!("f{back}.kala"));
        let mut d = bytes(&f.path);
        let at = d.len() - back as usize;
        d[at] ^= 0x40;
        std::fs::write(&p, &d).unwrap();
        assert_cut_as_damaged(&p, l0(&f), &before, &f.ents);
    }
}

#[test]
fn short_or_zero_or_cut_short_tails_after_a_clean_log_are_cut_back_and_random_tail_is_corrupt() {
    let (f, _, after) = fixture();
    let clean = bytes(&f.path);
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut random = Vec::new();
    for _ in 0..300 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        random.push((seed >> 33) as u8);
    }
    // Tails that v2 classifies as torn (cut + quarantined): shorter than a
    // begin record, all-zero, or a VALID begin whose body runs past EOF.
    let next_tx = common::spans(&clean).len() as u64;
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("1 byte", vec![0x07]),
        ("2 bytes", vec![0x07, 0x00]),
        ("3 bytes", vec![0x07, 0x00, 0x00]),
        ("huge length only", 0xFFFF_FFFFu32.to_le_bytes().to_vec()),
        ("huge length + a few bytes", {
            let mut v = 0xFFFF_FFFFu32.to_le_bytes().to_vec();
            v.extend_from_slice(&[1, 2, 3, 4, 5]);
            v
        }),
        ("zero length frame", vec![0, 0, 0, 0]),
        ("24 bytes of junk", vec![0x5A; 24]),
        ("zeros", vec![0; 64]),
        (
            "valid begin, body missing",
            common::begin_raw(common::BEGIN_TAG, next_tx, 1_000, 0),
        ),
        ("valid begin, huge body_len", {
            let mut v = common::begin_raw(common::BEGIN_TAG, next_tx, u64::MAX, 0);
            v.extend_from_slice(&[1, 2, 3]);
            v
        }),
    ];
    let scratch = tempfile::tempdir().unwrap();
    for (name, junk) in cases {
        let p = scratch.path().join("g.kala");
        let mut d = clean.clone();
        d.extend_from_slice(&junk);
        std::fs::write(&p, &d).unwrap();
        let w = match World::open(&p) {
            Ok(w) => w,
            Err(e) => panic!("{name}: open failed: {e:?}"),
        };
        assert_eq!(snapshot(&w, &f.ents), after, "{name}");
        drop(w);
        assert_eq!(
            bytes(&p),
            clean,
            "{name}: file must be cut back to the clean log"
        );
        check_writes_continue(&p, &f.ents);
    }
    // 300 random bytes: not a valid begin and not zeros -> damage, never cut.
    let p = scratch.path().join("random.kala");
    let mut d = clean.clone();
    d.extend_from_slice(&random);
    std::fs::write(&p, &d).unwrap();
    assert_corrupt_untouched(&p);
    assert_read_log_corrupt_untouched(&p);
}

#[test]
fn mid_log_corruption_followed_by_valid_txns_is_corrupt_and_untouched() {
    let (f, _, _) = fixture();
    let scratch = tempfile::tempdir().unwrap();
    // damage inside txn #i (not the last): a payload byte of its first frame,
    // and separately the last byte of its Commit checksum.
    for i in 1..f.lens.len() - 1 {
        let start = f.lens[i - 1] as usize;
        let end = f.lens[i] as usize;
        for (what, at) in [
            ("begin", start + 3),
            ("body", start + common::BEGIN_LEN + 8),
            ("checksum", end - 1),
        ] {
            let p = scratch.path().join(format!("m{i}{what}.kala"));
            let mut d = bytes(&f.path);
            d[at] ^= 0x01;
            std::fs::write(&p, &d).unwrap();
            let before = bytes(&p);
            match World::open(&p) {
                Err(Error::Corrupt(_)) => {}
                Err(e) => panic!("txn {i} {what}: expected Corrupt, got {e:?}"),
                Ok(_) => panic!("txn {i} {what}: expected Corrupt, open succeeded"),
            }
            assert_eq!(bytes(&p), before, "txn {i} {what}: file must be untouched");
        }
    }
}

#[test]
fn damage_to_the_second_to_last_txn_is_corrupt_not_a_silent_drop() {
    let (f, _, _) = fixture();
    let scratch = tempfile::tempdir().unwrap();
    let p = scratch.path().join("s.kala");
    let mut d = bytes(&f.path);
    let at = l0(&f) as usize - 1; // last byte of the previous Commit's checksum
    d[at] ^= 0xFF;
    std::fs::write(&p, &d).unwrap();
    assert_corrupt_untouched(&p);
}

#[test]
fn genesis_only_world_with_a_torn_second_txn_opens_at_tx_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let genesis_len = len(&path);
    w.create_entity("CHARACTER", Some("greg")).unwrap();
    let full = bytes(&path);
    drop(w);
    for k in genesis_len..full.len() as u64 {
        let p = dir.path().join(format!("g{k}.kala"));
        std::fs::write(&p, &full[..k as usize]).unwrap();
        let mut w = open_ok(&p);
        assert_eq!(w.latest_tx(), TxId(0), "k={k}");
        assert_eq!(w.latest_tick(), Tick(0), "k={k}");
        assert_eq!(w.entity_by_handle("greg"), None, "k={k}");
        assert_eq!(len(&p), genesis_len, "k={k}");
        let c = w.create_entity("CHARACTER", Some("greg")).unwrap();
        assert_eq!(w.format_id(c), "CHARACTER#1");
        assert_eq!(w.latest_tx(), TxId(1));
    }
}

#[test]
fn truncation_inside_the_genesis_txn_is_corrupt_and_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    drop(World::create(&path, schema()).unwrap());
    let full = bytes(&path);
    for k in 12..full.len() {
        let p = dir.path().join(format!("t{k}.kala"));
        std::fs::write(&p, &full[..k]).unwrap();
        assert_corrupt_untouched(&p);
    }
}

#[test]
fn read_log_on_a_torn_file_returns_only_committed_entries_and_does_not_modify_it() {
    let (f, _, _) = fixture();
    let committed_prefix = read_log(&f.path).unwrap();
    let n_last = committed_prefix.len();
    let scratch = tempfile::tempdir().unwrap();
    let full = bytes(&f.path);
    let (a, b) = (l0(&f), l1(&f));
    let clean_at_l0 = {
        let p = truncated_copy(&f.path, a, scratch.path());
        read_log(&p).unwrap()
    };
    assert!(clean_at_l0.len() < n_last);
    for k in a..b {
        let p = truncated_copy(&f.path, k, scratch.path());
        let before = bytes(&p);
        assert_eq!(before, full[..k as usize]);
        assert_eq!(read_log(&p).unwrap(), clean_at_l0, "k={k}");
        assert_eq!(
            bytes(&p),
            before,
            "k={k}: read_log must not modify the file"
        );
    }
    // bad checksum on the last commit: slice 3 item 0a: a damaged tail is
    // ignored by read_log like a torn one (committed prefix, file untouched).
    let p = scratch.path().join("bad.kala");
    let mut d = full.clone();
    let n = d.len();
    d[n - 1] ^= 1;
    std::fs::write(&p, &d).unwrap();
    assert_eq!(read_log(&p).unwrap(), clean_at_l0, "damaged tail ignored");
    assert_eq!(bytes(&p), d, "read_log must not modify the file");
}

#[test]
fn recovery_is_idempotent_a_second_open_changes_nothing() {
    let (f, before, _) = fixture();
    let scratch = tempfile::tempdir().unwrap();
    let p = truncated_copy(&f.path, l1(&f) - 3, scratch.path());
    drop(open_ok(&p));
    let after_first = bytes(&p);
    let w = open_ok(&p);
    assert_eq!(snapshot(&w, &f.ents), before);
    drop(w);
    assert_eq!(bytes(&p), after_first);
}

// ---- item 15 hardening: damage that must never be a silent drop ----

use common::{COMMIT_FRAME, commit_frame};

fn split_frames(d: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < d.len() {
        let n = u32::from_le_bytes(d[i..i + 4].try_into().unwrap()) as usize;
        out.push(d[i..i + 4 + n].to_vec());
        i += 4 + n;
    }
    out
}

fn assert_read_log_corrupt_untouched(p: &Path) {
    let before = bytes(p);
    match read_log(p) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!("read_log: expected Corrupt, got {e:?}"),
        Ok(v) => panic!("read_log: expected Corrupt, got {} entries", v.len()),
    }
    assert_eq!(bytes(p), before, "read_log must not modify the file");
}

#[test]
fn damaged_commit_kind_byte_of_the_second_to_last_txn_is_corrupt() {
    let (f, _, _) = fixture();
    let scratch = tempfile::tempdir().unwrap();
    // kind byte of the previous txn's Commit frame
    let at = l0(&f) as usize - COMMIT_FRAME + 4;
    assert_eq!(bytes(&f.path)[at], 5, "test assumes this is the kind byte");
    // The txn's begin record is re-sealed after each edit so that framing
    // (body_crc, begin_crc) stays valid and only the body CONTENT is wrong:
    // the damage must be caught by body validation, not by body_crc.
    let span = *common::spans(&bytes(&f.path))
        .iter()
        .find(|s| s.end == l0(&f) as usize)
        .unwrap();
    // other defined kinds, an unknown kind, and a single bit flip
    for (i, newk) in [1u8, 2, 3, 4, 0, 6, 0x85, 0xFF].into_iter().enumerate() {
        let p = scratch.path().join(format!("k{i}.kala"));
        let mut d = bytes(&f.path);
        d[at] = newk;
        common::reseal(&mut d, span);
        std::fs::write(&p, &d).unwrap();
        let before = bytes(&p);
        match World::open(&p) {
            Err(Error::Corrupt(_)) => {}
            Err(e) => panic!("kind {newk}: expected Corrupt, got {e:?}"),
            Ok(_) => panic!("kind {newk}: expected Corrupt, open succeeded"),
        }
        assert_eq!(
            bytes(&p),
            before,
            "kind {newk}: open must not touch the file"
        );
        assert_read_log_corrupt_untouched(&p);
    }
}

#[test]
fn read_log_on_mid_log_corruption_is_corrupt_and_untouched() {
    let (f, _, _) = fixture();
    let scratch = tempfile::tempdir().unwrap();
    for i in 1..f.lens.len() - 1 {
        let start = f.lens[i - 1] as usize;
        let end = f.lens[i] as usize;
        for (what, at) in [
            ("begin", start + 3),
            ("body", start + common::BEGIN_LEN + 8),
            ("checksum", end - 1),
        ] {
            let p = scratch.path().join(format!("r{i}{what}.kala"));
            let mut d = bytes(&f.path);
            d[at] ^= 0x01;
            std::fs::write(&p, &d).unwrap();
            assert_read_log_corrupt_untouched(&p);
        }
    }
}

#[test]
fn torn_tail_after_a_checksum_valid_but_replay_invalid_txn_is_corrupt() {
    let (f, _, _) = fixture();
    let scratch = tempfile::tempdir().unwrap();
    let d = bytes(&f.path);
    let (a, b) = (l0(&f) as usize, l1(&f) as usize);
    // Rebuild the last txn without its DefineEntity frame: its facts now use
    // an entity that was never defined. Renumber entry IDs, fix the checksum.
    let sp = *common::spans(&d).last().unwrap();
    assert_eq!((sp.start, sp.end), (a, b));
    let frames = split_frames(&d[sp.body_start..b]);
    let first_id = u64::from_le_bytes(frames[0][5..13].try_into().unwrap());
    let tx = u64::from_le_bytes(frames.last().unwrap()[13..21].try_into().unwrap());
    let kept: Vec<Vec<u8>> = frames[..frames.len() - 1]
        .iter()
        .filter(|fr| fr[4] != 3)
        .cloned()
        .collect();
    assert!(kept.len() < frames.len() - 1, "a DefineEntity was removed");
    let mut body = Vec::new();
    for (i, fr) in kept.iter().enumerate() {
        let mut fr = fr.clone();
        fr[5..13].copy_from_slice(&(first_id + i as u64).to_le_bytes());
        body.extend_from_slice(&fr);
    }
    let commit = commit_frame(
        common::link(&d[..a]),
        &body,
        first_id + kept.len() as u64,
        tx,
    );
    let mut full_body = body.clone();
    full_body.extend_from_slice(&commit);
    let mut bad = d[..a].to_vec();
    bad.extend_from_slice(&common::txn(tx, &full_body));

    // sanity-check the construction: with no tail it is Corrupt too
    let p0 = scratch.path().join("notail.kala");
    std::fs::write(&p0, &bad).unwrap();
    assert_corrupt_untouched(&p0);

    // then various torn tails after it
    let tails: Vec<Vec<u8>> = vec![
        vec![0x01],
        vec![0x40, 0x00, 0x00],
        0xFFFF_FFFFu32.to_le_bytes().to_vec(),
        d[a..a + 20].to_vec(),
        vec![0; 40],
    ];
    for (i, tail) in tails.iter().enumerate() {
        let p = scratch.path().join(format!("tail{i}.kala"));
        let mut file = bad.clone();
        file.extend_from_slice(tail);
        std::fs::write(&p, &file).unwrap();
        assert_corrupt_untouched(&p);
        assert_eq!(bytes(&p), file, "tail {i}: tail must not be cut");
    }
}

#[test]
fn commit_only_txn_after_a_bad_checksum_txn_is_corrupt() {
    let (f, _, _) = fixture();
    let scratch = tempfile::tempdir().unwrap();
    let mut d = bytes(&f.path);
    let log = read_log(&f.path).unwrap();
    let last_entry = log.last().unwrap().id.0;
    let last_tx = match log.last().unwrap().body {
        EntryBody::Commit { tx, .. } => tx.0,
        _ => panic!("log must end in a commit"),
    };
    let n = d.len();
    d[n - 1] ^= 0x01; // last txn now has a bad checksum
    // a valid commit-only txn follows it
    let extra = commit_frame(common::link(&d), &[], last_entry + 1, last_tx + 1);
    d.extend_from_slice(&common::txn(last_tx + 1, &extra));
    let p = scratch.path().join("c.kala");
    std::fs::write(&p, &d).unwrap();
    assert_corrupt_untouched(&p);
    assert_read_log_corrupt_untouched(&p);
}
