//! `World::chain_tip` and `log::chain_tip_holds`: a handle remembers the
//! last transaction it applied (where it is, and its chained checksum) and
//! proves its whole prefix unchanged by re-reading that one transaction.

mod common;

use common::spans;
use kala_core::log::{ChainTip, LogFault, chain_tip_holds};
use kala_core::*;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

fn schema() -> Schema {
    Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "tag",
        ValueType::Str,
        Kind::State,
        Cardinality::Many,
    ))
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

/// genesis, greg, `tag = earlier @-10`, `tag = fixed @0`.
fn build(path: &Path, earlier: &str) -> EntityId {
    let mut w = World::create(path, schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    w.assert(greg, "tag", s(earlier), Some(Tick(-10))).unwrap();
    w.assert(greg, "tag", s("fixed"), Some(Tick(0))).unwrap();
    greg
}

fn read(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

/// The tip the bytes of a well-formed file imply.
fn tip_of(bytes: &[u8]) -> ChainTip {
    let sp = *spans(bytes).last().unwrap();
    ChainTip {
        tx: TxId(sp.tx),
        offset: sp.start as u64,
        end: sp.end as u64,
        checksum: u32::from_le_bytes(bytes[sp.end - 4..sp.end].try_into().unwrap()),
    }
}

#[test]
fn the_tip_follows_every_commit_and_every_open_agrees() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let mut w = World::create(&p, schema()).unwrap();
    let genesis = w.chain_tip().unwrap();
    assert_eq!(genesis, tip_of(&read(&p)));
    assert_eq!((genesis.tx, genesis.offset), (TxId(0), 12));
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    assert_eq!(w.chain_tip().unwrap(), tip_of(&read(&p)));
    assert_eq!(w.chain_tip().unwrap().offset, genesis.end);
    // A write that changes nothing appends nothing and moves nothing.
    w.assert(greg, "tag", s("a"), Some(Tick(1))).unwrap();
    let before = w.chain_tip();
    w.assert(greg, "tag", s("a"), Some(Tick(1))).unwrap();
    assert_eq!(w.chain_tip(), before);
    assert_eq!(before.unwrap().tx, w.latest_tx());
    assert_eq!(before.unwrap(), tip_of(&read(&p)));
    assert!(chain_tip_holds(&p, &before.unwrap()).unwrap());
    // A reader of the same bytes records the same tip.
    let reader = World::open_read_only(&p).unwrap();
    assert_eq!(reader.chain_tip(), before);
    drop(w);
    assert_eq!(World::open(&p).unwrap().chain_tip(), before);
    // No file, no tip.
    assert_eq!(World::in_memory(schema()).unwrap().chain_tip(), None);
}

#[test]
fn an_older_tip_still_holds_after_plain_appends() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let greg = build(&p, "alpha");
    let reader = World::open_read_only(&p).unwrap();
    let pinned = reader.chain_tip().unwrap();
    let mut w = World::open(&p).unwrap();
    for i in 0..5 {
        w.assert(greg, "tag", s(&format!("n{i}")), Some(Tick(10 + i)))
            .unwrap();
        assert!(chain_tip_holds(&p, &pinned).unwrap(), "after append {i}");
        assert!(chain_tip_holds(&p, &w.chain_tip().unwrap()).unwrap());
    }
    // The reader is a snapshot: its tip doesn't move.
    assert_eq!(reader.chain_tip(), Some(pinned));
}

/// The scenario of `divergence-proof.rs`: another process cuts the log
/// before a transaction this handle applied, rewrites a different
/// transaction of the same length, re-appends byte-identical later content
/// (as far as a valid log allows) and appends more. Same inode, longer
/// file, same tx at the same offset: only the chained checksum tells.
#[test]
fn a_cut_and_rewrite_before_the_tip_is_detected_by_rereading_one_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original.kala");
    let replacement = dir.path().join("replacement.kala");
    let greg = build(&original, "alpha");
    build(&replacement, "bravo");
    let (a, b) = (read(&original), read(&replacement));
    let sp = spans(&a);
    assert_eq!(sp, spans(&b));
    assert_eq!(a.len(), b.len());

    let pinned = World::open_read_only(&original).unwrap();
    let tip = pinned.chain_tip().unwrap();
    assert_eq!(
        (tip.tx, tip.offset, tip.end),
        (TxId(3), sp[3].start as u64, a.len() as u64)
    );
    assert!(chain_tip_holds(&original, &tip).unwrap());
    {
        let mut file = OpenOptions::new().write(true).open(&original).unwrap();
        file.set_len(sp[2].start as u64).unwrap();
        file.seek(SeekFrom::Start(sp[2].start as u64)).unwrap();
        file.write_all(&b[sp[2].start..]).unwrap();
        file.sync_all().unwrap();
    }
    let mut rewritten = World::open(&original).unwrap();
    rewritten
        .assert(greg, "tag", s("newer"), Some(Tick(1)))
        .unwrap();
    assert!(doctor::doctor(&rewritten, &Default::default()).is_clean());
    let after = read(&original);
    // What a tail-only check without the chain would compare is unchanged:
    // tx#3 sits at the same offset, with the same length, tx and entries.
    assert_eq!(spans(&after)[3], sp[3]);
    // (Its begin record's `body_crc` and its checksum field are the only
    // bytes that differ.)
    assert_eq!(
        after[sp[3].body_start..sp[3].end - 4],
        a[sp[3].body_start..sp[3].end - 4]
    );
    assert!(after.len() > a.len());
    // The chained checksum is not.
    assert!(!chain_tip_holds(&original, &tip).unwrap());
    assert!(chain_tip_holds(&original, &rewritten.chain_tip().unwrap()).unwrap());
    assert_ne!(
        pinned.as_of(greg, "tag", Tick(-10)).unwrap(),
        rewritten.as_of(greg, "tag", Tick(-10)).unwrap()
    );
}

#[test]
fn a_tip_does_not_hold_on_a_shorter_cut_damaged_or_foreign_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    build(&p, "alpha");
    let good = read(&p);
    let tip = tip_of(&good);
    let check = |bytes: &[u8]| {
        std::fs::write(&p, bytes).unwrap();
        let r = chain_tip_holds(&p, &tip);
        assert_eq!(read(&p), bytes, "never modified");
        r
    };
    assert_eq!(check(&good), Ok(true));
    // Shorter: the transaction was cut (wholly or partly).
    for len in [12, tip.offset as usize, tip.end as usize - 1] {
        assert_eq!(check(&good[..len]), Ok(false), "len {len}");
    }
    // Any byte of the transaction changed.
    for at in tip.offset as usize..tip.end as usize {
        let mut b = good.clone();
        b[at] ^= 0x10;
        assert_eq!(check(&b), Ok(false), "byte {at}");
    }
    // Bytes before it changed without the chain following: the tip's own
    // bytes are intact, and that is all one transaction can say. (A writer
    // can't produce this file: open refuses it as Corrupt.)
    let mut b = good.clone();
    b[tip.offset as usize - 1] ^= 0x10;
    assert_eq!(check(&b), Ok(true));
    assert!(matches!(World::open_read_only(&p), Err(Error::Corrupt(_))));
    // Another transaction's range, another tx, another checksum.
    let first = spans(&good)[0];
    let wrong = [
        ChainTip {
            offset: first.start as u64,
            end: first.end as u64,
            ..tip
        },
        ChainTip {
            tx: TxId(tip.tx.0 + 1),
            ..tip
        },
        ChainTip {
            checksum: tip.checksum ^ 1,
            ..tip
        },
        ChainTip {
            end: tip.end - 1,
            ..tip
        },
        ChainTip {
            end: tip.offset,
            ..tip
        },
        ChainTip { offset: 0, ..tip },
        ChainTip {
            end: u64::MAX,
            ..tip
        },
    ];
    std::fs::write(&p, &good).unwrap();
    for t in wrong {
        assert_eq!(chain_tip_holds(&p, &t), Ok(false), "{t:?}");
    }
    // Not this format's log at all.
    let mut b = good.clone();
    b[8] = 5;
    assert_eq!(check(&b), Err(Error::UnsupportedVersion(5)));
    b[0] = b'X';
    assert_eq!(check(&b), Err(Error::BadMagic));
    assert!(matches!(
        chain_tip_holds(&dir.path().join("absent.kala"), &tip),
        Err(Error::Io { .. })
    ));
    assert!(matches!(
        chain_tip_holds(dir.path(), &tip),
        Err(Error::Io { .. })
    ));
}

#[test]
fn a_failed_append_leaves_the_tip_on_the_last_durable_transaction() {
    for fault in [LogFault::FailNextSync, LogFault::FailNextWrite] {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("w.kala");
        let greg = build(&p, "alpha");
        let mut w = World::open(&p).unwrap();
        let tip = w.chain_tip().unwrap();
        w.inject_log_fault(fault);
        assert!(w.assert(greg, "tag", s("lost"), Some(Tick(9))).is_err());
        assert_eq!(w.chain_tip(), Some(tip), "{fault:?}");
        assert!(chain_tip_holds(&p, &tip).unwrap());
        drop(w);
        let mut w = World::open(&p).unwrap();
        assert_eq!(w.chain_tip(), Some(tip));
        w.assert(greg, "tag", s("kept"), Some(Tick(9))).unwrap();
        let next = w.chain_tip().unwrap();
        assert_eq!((next.tx, next.offset), (TxId(4), tip.end));
        assert_eq!(next, tip_of(&read(&p)));
        assert!(chain_tip_holds(&p, &tip).unwrap());
    }
}

#[test]
fn a_recovery_cut_moves_the_tip_back_to_the_last_committed_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    build(&p, "alpha");
    let good = read(&p);
    let sp = spans(&good);
    let applied = tip_of(&good);
    std::fs::write(&p, &good[..good.len() - 3]).unwrap();
    // A reader leaves the torn tail in the file and stands before it.
    let before = tip_of(&good[..sp[3].start]);
    assert_eq!(World::open_read_only(&p).unwrap().chain_tip(), Some(before));
    let w = World::open(&p).unwrap();
    assert_eq!(w.recovery().cause, Some(CutCause::Torn));
    assert_eq!(w.chain_tip(), Some(before));
    // A handle that had applied the cut transaction sees the divergence.
    assert!(!chain_tip_holds(&p, &applied).unwrap());
    assert!(chain_tip_holds(&p, &before).unwrap());
}
