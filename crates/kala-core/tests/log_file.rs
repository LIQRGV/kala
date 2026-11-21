//! Item 13: the log file. `World::create` writes a header and the schema as
//! genesis tx#0; every committed transaction appends its entries plus one
//! `Commit`; `kala_core::log::read_log` reads the committed entries back.
//!
//! Expected API (contract, "Log file"):
//! - `World::create(path: &Path, schema: Schema) -> Result<World, Error>`
//!   (create_new: an existing path -> `Err(Error::Io { kind: AlreadyExists, .. })`).
//! - `Error::Io { kind: std::io::ErrorKind, message: String }`.
//! - `kala_core::log::{read_log, LogEntry, EntryBody, EntryId, LOG_MAGIC,
//!   LOG_FORMAT_VERSION}`; `read_log(path: &Path) -> Result<Vec<LogEntry>, Error>`;
//!   `LOG_MAGIC: [u8; 8] = *b"KALALOG\0"`, `LOG_FORMAT_VERSION: u32 = 2`;
//!   `EntryId(pub u64)`; `LogEntry { pub id: EntryId, pub body: EntryBody }`
//!   (Debug, Clone, PartialEq); `EntryBody::{DefineNodeType{id: NodeTypeId, name: String},
//!   DefineAttr{id: AttrId, def: AttrDef}, DefineEntity{id: EntityId, handle: Option<String>},
//!   Fact{entity, attr: AttrId, value: Value, tick: Tick, op: Op}, Commit{tx: TxId, checksum: u32}}`.
//!
//! Only the 12-byte header and what `read_log` returns are asserted; the
//! body byte layout is not. Reopen/replay and torn-tail recovery are not
//! tested here.

use kala_core::log::{EntryBody, EntryId, LOG_FORMAT_VERSION, LOG_MAGIC, LogEntry, read_log};
use kala_core::*;
use std::path::{Path, PathBuf};

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "age",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "height",
            ValueType::F64,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "alive",
            ValueType::Bool,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
        .attribute(AttrDef::new(
            "bought",
            ValueType::Ref,
            Kind::Event,
            Cardinality::Many,
        ))
}

struct Fx {
    _dir: tempfile::TempDir,
    path: PathBuf,
    w: World,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("world.kala");
    let w = World::create(&path, schema()).expect("create");
    Fx { _dir: dir, path, w }
}

fn bytes(p: &Path) -> Vec<u8> {
    std::fs::read(p).expect("read file")
}

fn log(p: &Path) -> Vec<LogEntry> {
    read_log(p).expect("read_log")
}

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn commits(entries: &[LogEntry]) -> Vec<TxId> {
    entries
        .iter()
        .filter_map(|e| match e.body {
            EntryBody::Commit { tx, .. } => Some(tx),
            _ => None,
        })
        .collect()
}

fn attr_id(entries: &[LogEntry], name: &str) -> AttrId {
    entries
        .iter()
        .find_map(|e| match &e.body {
            EntryBody::DefineAttr { id, def } if def.name == name => Some(*id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no DefineAttr for {name}"))
}

/// The entries after the genesis transaction (everything after Commit tx 0).
fn after_genesis(entries: &[LogEntry]) -> &[LogEntry] {
    let i = entries
        .iter()
        .position(|e| matches!(e.body, EntryBody::Commit { .. }))
        .expect("genesis commit");
    &entries[i + 1..]
}

#[test]
fn header_is_magic_then_version_little_endian() {
    let f = fx();
    let b = bytes(&f.path);
    assert!(b.len() >= 12);
    assert_eq!(&b[..8], b"KALALOG\0");
    assert_eq!(LOG_MAGIC, *b"KALALOG\0");
    assert_eq!(LOG_FORMAT_VERSION, 2);
    assert_eq!(&b[8..12], &2u32.to_le_bytes());
}

#[test]
fn create_on_existing_path_is_already_exists_and_leaves_file_untouched() {
    let f = fx();
    let mut w = f.w;
    let e = w.create_entity("CHARACTER", Some("greg")).unwrap();
    w.assert(e, "age", Value::I64(3), t(1)).unwrap();
    let before = bytes(&f.path);
    let err = World::create(&f.path, schema()).expect_err("must fail");
    assert!(
        matches!(
            err,
            Error::Io {
                kind: std::io::ErrorKind::AlreadyExists,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(bytes(&f.path), before);
}

#[test]
fn create_on_a_pre_existing_foreign_file_does_not_overwrite_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("other");
    std::fs::write(&path, b"not a kala log").unwrap();
    let err = World::create(&path, schema()).expect_err("must fail");
    assert!(matches!(
        err,
        Error::Io {
            kind: std::io::ErrorKind::AlreadyExists,
            ..
        }
    ));
    assert_eq!(bytes(&path), b"not a kala log");
}

#[test]
fn genesis_is_defines_in_declaration_order_then_commit_tx0() {
    let f = fx();
    assert_eq!(f.w.latest_tx(), TxId(0));
    let entries = log(&f.path);
    let n_types = 2;
    let n_attrs = 7;
    assert_eq!(entries.len(), n_types + n_attrs + 1);

    assert_eq!(
        entries[0].body,
        EntryBody::DefineNodeType {
            id: NodeTypeId(0),
            name: "CHARACTER".into()
        }
    );
    assert_eq!(
        entries[1].body,
        EntryBody::DefineNodeType {
            id: NodeTypeId(1),
            name: "PLACE".into()
        }
    );

    let expected_defs = [
        AttrDef::new("located_in", ValueType::Ref, Kind::State, Cardinality::One),
        AttrDef::new("name", ValueType::Str, Kind::State, Cardinality::One),
        AttrDef::new("age", ValueType::I64, Kind::State, Cardinality::Many),
        AttrDef::new("height", ValueType::F64, Kind::State, Cardinality::Many),
        AttrDef::new("alive", ValueType::Bool, Kind::State, Cardinality::Many),
        AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed(),
        AttrDef::new("bought", ValueType::Ref, Kind::Event, Cardinality::Many),
    ];
    let mut ids = Vec::new();
    for (i, def) in expected_defs.iter().enumerate() {
        match &entries[2 + i].body {
            EntryBody::DefineAttr { id, def: got } => {
                assert_eq!(got, def, "attribute #{i}");
                ids.push(id.0);
            }
            other => panic!("entry {} is {other:?}, expected DefineAttr", 2 + i),
        }
    }
    // distinct and increasing in declaration order
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");

    assert!(matches!(
        entries[n_types + n_attrs].body,
        EntryBody::Commit { tx: TxId(0), .. }
    ));
    assert_eq!(commits(&entries), vec![TxId(0)]);
}

#[test]
fn entry_ids_start_at_one_and_increase_by_one_across_the_whole_log() {
    let mut f = fx();
    let greg = f.w.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = f.w.create_entity("PLACE", None).unwrap();
    f.w.assert(greg, "located_in", Value::Ref(varrow), t(1))
        .unwrap();
    f.w.assert(greg, "age", Value::I64(30), t(2)).unwrap();
    f.w.retract(greg, "located_in", Value::Ref(varrow), t(5))
        .unwrap();
    let entries = log(&f.path);
    assert_eq!(entries[0].id, EntryId(1));
    for (i, e) in entries.iter().enumerate() {
        assert_eq!(e.id, EntryId(i as u64 + 1), "entry at position {i}");
    }
}

#[test]
fn create_entity_with_handle_writes_define_entity_then_commit() {
    let mut f = fx();
    let before = log(&f.path).len();
    let tx0 = f.w.latest_tx();
    let edwin = f.w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let entries = log(&f.path);
    let new = &entries[before..];
    assert_eq!(new.len(), 2, "{new:?}");
    assert_eq!(
        new[0].body,
        EntryBody::DefineEntity {
            id: edwin,
            handle: Some("edwin".into())
        }
    );
    match new[1].body {
        EntryBody::Commit { tx, .. } => {
            assert_eq!(tx, TxId(tx0.0 + 1));
            assert_eq!(tx, f.w.latest_tx());
        }
        ref other => panic!("{other:?}"),
    }
}

#[test]
fn create_entity_without_handle_has_handle_none() {
    let mut f = fx();
    let before = log(&f.path).len();
    let p = f.w.create_entity("PLACE", None).unwrap();
    let entries = log(&f.path);
    assert_eq!(
        entries[before].body,
        EntryBody::DefineEntity {
            id: p,
            handle: None
        }
    );
}

#[test]
fn txn_defines_come_before_facts_and_end_in_one_commit_with_the_txns_tx() {
    let mut f = fx();
    let before = log(&f.path).len();
    let prev = f.w.latest_tx();
    let mut txn = f.w.begin();
    let greg = txn.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = txn.create_entity("PLACE", Some("varrow")).unwrap();
    txn.assert(greg, "located_in", Value::Ref(varrow), t(-300))
        .unwrap();
    txn.assert(greg, "age", Value::I64(40), t(-300)).unwrap();
    let res = txn.commit().unwrap();
    assert_eq!(res.tx, TxId(prev.0 + 1));

    let entries = log(&f.path);
    let new = &entries[before..];
    // the ref assert also interns an edge (slice 2, item 3)
    assert_eq!(new.len(), 6, "{new:?}");
    assert!(matches!(new[0].body, EntryBody::DefineEntity { id, .. } if id == greg));
    assert!(matches!(new[1].body, EntryBody::DefineEntity { id, .. } if id == varrow));
    assert!(
        matches!(new[2].body, EntryBody::DefineEdge { source, target, .. }
        if source == greg && target == varrow)
    );
    assert!(matches!(new[3].body, EntryBody::Fact { .. }));
    assert!(matches!(new[4].body, EntryBody::Fact { .. }));
    assert!(matches!(new[5].body, EntryBody::Commit { tx, .. } if tx == res.tx));
    let n_commits = new
        .iter()
        .filter(|e| matches!(e.body, EntryBody::Commit { .. }))
        .count();
    assert_eq!(n_commits, 1);
}

#[test]
fn facts_carry_entity_attr_value_tick_and_op_for_all_five_types() {
    let mut f = fx();
    let greg = f.w.create_entity("CHARACTER", None).unwrap();
    let varrow = f.w.create_entity("PLACE", None).unwrap();
    let long_bio = "x".repeat(5000);
    f.w.assert(greg, "located_in", Value::Ref(varrow), t(i64::MIN))
        .unwrap();
    f.w.assert(greg, "name", Value::Str("Edwin\0é".into()), t(-1))
        .unwrap();
    f.w.assert(greg, "age", Value::I64(i64::MIN), t(0)).unwrap();
    f.w.assert(greg, "height", Value::F64(1.75), t(1)).unwrap();
    f.w.assert(greg, "alive", Value::Bool(true), t(i64::MAX))
        .unwrap();
    f.w.assert(greg, "bio", Value::Str(long_bio.clone()), t(2))
        .unwrap();
    f.w.retract(greg, "alive", Value::Bool(true), t(i64::MAX))
        .err(); // same tick conflict: must not be logged
    f.w.retract(greg, "located_in", Value::Ref(varrow), t(7))
        .unwrap();

    let entries = log(&f.path);
    let facts: Vec<&EntryBody> = entries
        .iter()
        .map(|e| &e.body)
        .filter(|b| matches!(b, EntryBody::Fact { .. }))
        .collect();
    let id = |n: &str| attr_id(&entries, n);
    let expect = |entity, attr: &str, value, tick, op| EntryBody::Fact {
        entity,
        attr: id(attr),
        value,
        tick: Tick(tick),
        op,
    };
    let want = vec![
        expect(greg, "located_in", Value::Ref(varrow), i64::MIN, Op::Assert),
        expect(greg, "name", Value::Str("Edwin\0é".into()), -1, Op::Assert),
        expect(greg, "age", Value::I64(i64::MIN), 0, Op::Assert),
        expect(greg, "height", Value::F64(1.75), 1, Op::Assert),
        expect(greg, "alive", Value::Bool(true), i64::MAX, Op::Assert),
        expect(greg, "bio", Value::Str(long_bio), 2, Op::Assert),
        expect(greg, "located_in", Value::Ref(varrow), 7, Op::Retract),
    ];
    assert_eq!(facts.len(), want.len(), "{facts:?}");
    for (g, w) in facts.iter().zip(&want) {
        assert_eq!(*g, w);
    }
}

#[test]
fn noop_writes_append_nothing() {
    let mut f = fx();
    let greg = f.w.create_entity("CHARACTER", None).unwrap();
    f.w.assert(greg, "age", Value::I64(1), t(10)).unwrap();
    let b = bytes(&f.path);
    let l = log(&f.path);
    let tx = f.w.latest_tx();
    // assert already holding, retract of a value not holding, repeated op
    assert_eq!(
        f.w.assert(greg, "age", Value::I64(1), t(10))
            .unwrap()
            .changed,
        0
    );
    assert_eq!(
        f.w.assert(greg, "age", Value::I64(1), t(20))
            .unwrap()
            .changed,
        0
    );
    assert_eq!(
        f.w.retract(greg, "age", Value::I64(2), t(10))
            .unwrap()
            .changed,
        0
    );
    assert_eq!(
        f.w.retract(greg, "age", Value::I64(1), t(5))
            .unwrap()
            .changed,
        0
    );
    // a txn with only no-ops
    let mut txn = f.w.begin();
    assert_eq!(
        txn.assert(greg, "age", Value::I64(1), t(10))
            .unwrap()
            .changed,
        0
    );
    assert_eq!(txn.commit().unwrap().changed, 0);
    assert_eq!(bytes(&f.path), b);
    assert_eq!(log(&f.path), l);
    assert_eq!(f.w.latest_tx(), tx);
}

#[test]
fn failed_writes_append_nothing() {
    let mut f = fx();
    let greg = f.w.create_entity("CHARACTER", Some("greg")).unwrap();
    f.w.assert(greg, "age", Value::I64(1), t(10)).unwrap();
    f.w.assert(greg, "bought", Value::Ref(greg), t(10)).unwrap();
    let b = bytes(&f.path);
    assert!(f.w.assert(greg, "nope", Value::I64(1), t(1)).is_err());
    assert!(f.w.assert(greg, "age", Value::Bool(true), t(1)).is_err());
    assert!(
        f.w.retract(greg, "bought", Value::Ref(greg), t(11))
            .is_err()
    );
    assert!(f.w.retract(greg, "age", Value::I64(1), t(20)).is_ok());
    let b2 = bytes(&f.path);
    assert!(b2.len() > b.len(), "the successful retract is logged");
    let b = b2;
    // same-tick conflict: assert vs the retract stored at tick 20
    assert!(f.w.assert(greg, "age", Value::I64(1), t(20)).is_err());
    assert!(f.w.create_entity("CHARACTER", Some("greg")).is_err());
    assert!(f.w.create_entity("NOPE", None).is_err());
    assert!(
        f.w.assert(greg, "name", Value::Str("x".repeat(257)), t(1))
            .is_err()
    );
    assert_eq!(bytes(&f.path), b);
}

#[test]
fn dropped_and_poisoned_txns_append_nothing() {
    let mut f = fx();
    let greg = f.w.create_entity("CHARACTER", Some("greg")).unwrap();
    let b = bytes(&f.path);
    let l = log(&f.path);
    {
        let mut txn = f.w.begin();
        txn.create_entity("PLACE", Some("varrow")).unwrap();
        txn.assert(greg, "age", Value::I64(9), t(3)).unwrap();
        // dropped without commit
    }
    assert_eq!(bytes(&f.path), b);
    {
        let mut txn = f.w.begin();
        txn.create_entity("PLACE", Some("ashford")).unwrap();
        txn.assert(greg, "age", Value::I64(9), t(3)).unwrap();
        assert!(txn.assert(greg, "nope", Value::I64(1), t(3)).is_err());
        assert!(matches!(txn.commit(), Err(Error::TransactionAborted)));
    }
    assert_eq!(bytes(&f.path), b);
    assert_eq!(log(&f.path), l);
}

#[test]
fn every_committed_tx_has_exactly_one_commit_and_tx_numbers_are_consecutive() {
    let mut f = fx();
    let greg = f.w.create_entity("CHARACTER", None).unwrap();
    f.w.assert(greg, "age", Value::I64(1), t(1)).unwrap();
    f.w.assert(greg, "age", Value::I64(1), t(1)).unwrap(); // no-op
    let mut txn = f.w.begin();
    let p = txn.create_entity("PLACE", None).unwrap();
    txn.assert(greg, "located_in", Value::Ref(p), t(2)).unwrap();
    txn.assert(greg, "age", Value::I64(2), t(2)).unwrap();
    txn.commit().unwrap();
    f.w.retract(greg, "age", Value::I64(2), t(3)).unwrap();

    let entries = log(&f.path);
    let txs = commits(&entries);
    let want: Vec<TxId> = (0..txs.len() as u64).map(TxId).collect();
    assert_eq!(txs, want);
    assert_eq!(txs.last().copied(), Some(f.w.latest_tx()));
    assert_eq!(txs.len(), 5); // genesis, create, assert, txn, retract

    // the log never ends with a non-Commit entry, and no two Commits are adjacent
    assert!(matches!(
        entries.last().unwrap().body,
        EntryBody::Commit { .. }
    ));
    assert!(
        entries
            .windows(2)
            .all(|w| !(matches!(w[0].body, EntryBody::Commit { .. })
                && matches!(w[1].body, EntryBody::Commit { .. })))
    );
    // non-genesis part is non-empty
    assert!(!after_genesis(&entries).is_empty());
}

#[test]
fn read_log_is_read_only() {
    let mut f = fx();
    let greg = f.w.create_entity("CHARACTER", Some("greg")).unwrap();
    f.w.assert(greg, "age", Value::I64(1), t(1)).unwrap();
    let before = bytes(&f.path);
    let a = log(&f.path);
    let b = log(&f.path);
    assert_eq!(a, b);
    assert_eq!(bytes(&f.path), before);
}

#[test]
fn file_backed_reads_match_in_memory_world() {
    let mut f = fx();
    let mut m = World::in_memory(schema()).unwrap();

    fn run(w: &mut World) -> (EntityId, EntityId, EntityId) {
        let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
        let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
        let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
        w.assert(greg, "located_in", Value::Ref(varrow), Some(Tick(-300)))
            .unwrap();
        w.retract(greg, "located_in", Value::Ref(varrow), Some(Tick(-250)))
            .unwrap();
        w.assert(greg, "located_in", Value::Ref(ashford), Some(Tick(-250)))
            .unwrap();
        w.assert(greg, "located_in", Value::Ref(varrow), Some(Tick(-400)))
            .unwrap(); // prequel
        (greg, varrow, ashford)
    }
    let (g1, v1, a1) = run(&mut f.w);
    let (g2, v2, a2) = run(&mut m);
    assert_eq!((g1, v1, a1), (g2, v2, a2));

    assert_eq!(f.w.latest_tx(), m.latest_tx());
    assert_eq!(f.w.latest_tick(), m.latest_tick());
    assert_eq!(
        f.w.history(g1, "located_in").unwrap(),
        m.history(g1, "located_in").unwrap()
    );
    for tick in [-1000, -400, -300, -251, -250, 0, i64::MAX] {
        assert_eq!(
            f.w.as_of(g1, "located_in", Tick(tick)).unwrap(),
            m.as_of(g1, "located_in", Tick(tick)).unwrap(),
            "tick {tick}"
        );
    }
    for tx in 0..=f.w.latest_tx().0 {
        assert_eq!(
            f.w.at_canon(TxId(tx)).history(g1, "located_in").ok(),
            m.at_canon(TxId(tx)).history(g1, "located_in").ok(),
            "canon {tx}"
        );
    }
}
