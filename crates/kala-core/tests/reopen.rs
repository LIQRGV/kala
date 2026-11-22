//! Item 14: reopen = replay. `World::open(path: &Path) -> Result<World, Error>`
//! replays the log and rebuilds IDs, schema, indexes, latest tx and latest
//! tick, so every answer matches the pre-reopen world, at every canon.
//!
//! Torn-tail recovery (item 15) is not tested here.

mod common;

use kala_core::log::{EntryBody, LOG_FORMAT_VERSION, LOG_MAGIC, read_log};
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
        .attribute(a("height", ValueType::F64, Kind::State, Cardinality::Many))
        .attribute(a("alive", ValueType::Bool, Kind::State, Cardinality::Many))
        .attribute(a("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
        .attribute(a("bought", ValueType::Ref, Kind::Event, Cardinality::Many))
}

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn bytes(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

fn open_err(p: &Path) -> Error {
    match World::open(p) {
        Err(e) => e,
        Ok(_) => panic!("open should have failed"),
    }
}

struct Ents {
    greg: EntityId,
    edwin: EntityId,
    varrow: EntityId,
    ashford: EntityId,
    mara: EntityId,
}

/// A non-trivial world: all five value types, retracts, out-of-order writes,
/// several transactions including one that creates an entity and writes facts.
fn build(path: &Path) -> (World, Ents) {
    let mut w = World::create(path, schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = w.create_entity("PLACE", None).unwrap();
    w.assert(greg, "located_in", Value::Ref(varrow), t(-300))
        .unwrap();
    w.retract(greg, "located_in", Value::Ref(varrow), t(-250))
        .unwrap();
    w.assert(greg, "located_in", Value::Ref(ashford), t(-250))
        .unwrap();
    w.assert(greg, "located_in", Value::Ref(varrow), t(-400))
        .unwrap(); // prequel
    w.retract(greg, "located_in", Value::Ref(varrow), t(-350))
        .unwrap(); // out of order, inside a later interval
    w.assert(greg, "name", Value::Str("Greg\0the\u{e9}".into()), t(0))
        .unwrap();
    w.assert(greg, "age", Value::I64(i64::MIN), t(i64::MIN))
        .unwrap();
    w.assert(greg, "age", Value::I64(30), t(-100)).unwrap();
    w.retract(greg, "age", Value::I64(30), t(-50)).unwrap();
    w.assert(greg, "height", Value::F64(1.75), t(5)).unwrap();
    w.assert(greg, "height", Value::F64(-0.0), t(6)).unwrap();
    w.assert(greg, "alive", Value::Bool(true), t(i64::MAX))
        .unwrap();
    w.assert(greg, "bio", Value::Str("b".repeat(1000)), t(3))
        .unwrap();
    w.assert(edwin, "bought", Value::Ref(greg), t(-208))
        .unwrap();
    w.assert(edwin, "age", Value::I64(1), t(10)).unwrap();
    w.assert(edwin, "age", Value::I64(1), t(10)).unwrap(); // no-op
    let mut txn = w.begin();
    let mara = txn.create_entity("CHARACTER", Some("mara")).unwrap();
    txn.assert(mara, "located_in", Value::Ref(ashford), t(-260))
        .unwrap();
    txn.assert(mara, "alive", Value::Bool(true), t(-260))
        .unwrap();
    txn.assert(edwin, "located_in", Value::Ref(ashford), None)
        .unwrap();
    txn.commit().unwrap();
    (
        w,
        Ents {
            greg,
            edwin,
            varrow,
            ashford,
            mara,
        },
    )
}

const TICKS: [i64; 17] = [
    i64::MIN,
    -1000,
    -400,
    -351,
    -350,
    -300,
    -260,
    -251,
    -250,
    -100,
    -50,
    0,
    5,
    6,
    10,
    1000,
    i64::MAX,
];
const ATTRS: [&str; 7] = [
    "located_in",
    "name",
    "age",
    "height",
    "alive",
    "bio",
    "bought",
];

/// Every read the API offers, at every canon, rendered as one string.
fn snapshot(w: &World, e: &Ents) -> String {
    let mut out = String::new();
    let ents = [e.greg, e.edwin, e.mara, e.varrow, e.ashford];
    let probes = [
        ("located_in", Value::Ref(e.varrow)),
        ("located_in", Value::Ref(e.ashford)),
        ("age", Value::I64(30)),
        ("age", Value::I64(1)),
        ("alive", Value::Bool(true)),
        ("bought", Value::Ref(e.greg)),
    ];
    out += &format!("tx={:?} tick={:?}\n", w.latest_tx(), w.latest_tick());
    for tx in 0..=w.latest_tx().0 + 1 {
        let v = w.at_canon(TxId(tx));
        for &en in &ents {
            for a in ATTRS {
                out += &format!("c{tx} {en:?} {a} hist={:?}\n", v.history(en, a));
                for k in TICKS {
                    out += &format!("c{tx} {en:?} {a} @{k} {:?}\n", v.as_of(en, a, Tick(k)));
                }
            }
            for (a, val) in &probes {
                for k in TICKS {
                    out += &format!(
                        "c{tx} {en:?} {a} {val:?} @{k} {:?}\n",
                        v.as_of_value(en, a, val, Tick(k))
                    );
                }
            }
        }
    }
    for en in ents {
        out += &format!("{en:?}={}\n", w.format_id(en));
        for a in ATTRS {
            out += &format!("latest {en:?} {a} {:?}\n", w.history(en, a));
        }
    }
    for h in ["greg", "edwin", "mara", "varrow", "nobody", "Greg"] {
        out += &format!("{h}={:?}\n", w.entity_by_handle(h));
    }
    out
}

struct Built {
    _dir: tempfile::TempDir,
    path: PathBuf,
    ents: Ents,
    before: String,
}

fn built() -> Built {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let (w, ents) = build(&path);
    let before = snapshot(&w, &ents);
    drop(w);
    Built {
        _dir: dir,
        path,
        ents,
        before,
    }
}

#[test]
fn reopen_answers_match_at_every_canon_and_tick() {
    let b = built();
    let w = World::open(&b.path).unwrap();
    assert_eq!(snapshot(&w, &b.ents), b.before);
}

#[test]
fn reopen_of_a_clean_log_leaves_the_file_byte_identical() {
    let b = built();
    let before = bytes(&b.path);
    drop(World::open(&b.path).unwrap());
    assert_eq!(bytes(&b.path), before);
}

#[test]
fn reopen_twice_gives_the_same_answers() {
    let b = built();
    drop(World::open(&b.path).unwrap());
    let w = World::open(&b.path).unwrap();
    assert_eq!(snapshot(&w, &b.ents), b.before);
}

#[test]
fn reopen_restores_latest_tx_tick_handles_and_format_ids() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let (w, e) = build(&path);
    let (tx, tick) = (w.latest_tx(), w.latest_tick());
    assert_eq!(tick, Tick(i64::MAX));
    drop(w);
    let w = World::open(&path).unwrap();
    assert_eq!(w.latest_tx(), tx);
    assert_eq!(w.latest_tick(), tick);
    assert_eq!(w.entity_by_handle("greg"), Some(e.greg));
    assert_eq!(w.entity_by_handle("mara"), Some(e.mara));
    assert_eq!(w.entity_by_handle("varrow"), Some(e.varrow));
    assert_eq!(w.entity_by_handle("nobody"), None);
    assert_eq!(w.format_id(e.greg), "CHARACTER#1");
    assert_eq!(w.format_id(e.edwin), "CHARACTER#2");
    assert_eq!(w.format_id(e.mara), "CHARACTER#3");
    assert_eq!(w.format_id(e.varrow), "PLACE#1");
    assert_eq!(w.format_id(e.ashford), "PLACE#2");
}

#[test]
fn reopen_keeps_the_schema_enforced() {
    let b = built();
    let mut w = World::open(&b.path).unwrap();
    let g = b.ents.greg;
    assert!(matches!(
        w.assert(g, "nope", Value::I64(1), t(1)),
        Err(Error::UnknownAttribute(_))
    ));
    assert!(matches!(
        w.assert(g, "age", Value::Bool(true), t(1)),
        Err(Error::WrongValueType { .. })
    ));
    assert!(matches!(
        w.retract(b.ents.edwin, "bought", Value::Ref(g), t(1)),
        Err(Error::RetractEvent(_))
    ));
    assert!(matches!(
        w.create_entity("NOPE", None),
        Err(Error::UnknownNodeType(_))
    ));
    assert!(matches!(
        w.assert(g, "name", Value::Str("x".repeat(257)), t(1)),
        Err(Error::Value(_))
    ));
    // unindexed attribute still takes long strings
    assert!(
        w.assert(g, "bio", Value::Str("y".repeat(5000)), t(4))
            .is_ok()
    );
}

#[test]
fn writing_continues_after_reopen_with_next_seq_and_tx() {
    let b = built();
    let mut w = World::open(&b.path).unwrap();
    let tx = w.latest_tx();
    let c = w.create_entity("CHARACTER", Some("lisa")).unwrap();
    assert_eq!(w.format_id(c), "CHARACTER#4");
    assert_eq!(w.latest_tx(), TxId(tx.0 + 1));
    let p = w.create_entity("PLACE", None).unwrap();
    assert_eq!(w.format_id(p), "PLACE#3");
    assert_eq!(w.latest_tx(), TxId(tx.0 + 2));
    w.assert(c, "age", Value::I64(7), t(1)).unwrap();
    assert_eq!(w.latest_tx(), TxId(tx.0 + 3));
    assert_eq!(w.entity_by_handle("lisa"), Some(c));
    // and it survives another reopen
    drop(w);
    let mut w = World::open(&b.path).unwrap();
    assert_eq!(w.entity_by_handle("lisa"), Some(c));
    let d = w.create_entity("CHARACTER", None).unwrap();
    assert_eq!(w.format_id(d), "CHARACTER#5");
}

#[test]
fn duplicate_handle_of_a_pre_reopen_entity_is_rejected() {
    let b = built();
    let mut w = World::open(&b.path).unwrap();
    let tx = w.latest_tx();
    let before = bytes(&b.path);
    assert_eq!(
        w.create_entity("CHARACTER", Some("greg")),
        Err(Error::DuplicateHandle("greg".into()))
    );
    assert_eq!(
        w.create_entity("PLACE", Some("varrow")),
        Err(Error::DuplicateHandle("varrow".into()))
    );
    assert_eq!(w.latest_tx(), tx);
    assert_eq!(bytes(&b.path), before);
}

#[test]
fn entry_ids_and_tx_continue_across_the_reopen_boundary() {
    let b = built();
    let n_before = read_log(&b.path).unwrap().len();
    let mut w = World::open(&b.path).unwrap();
    let c = w.create_entity("CHARACTER", None).unwrap();
    w.assert(c, "age", Value::I64(1), t(1)).unwrap();
    let entries = read_log(&b.path).unwrap();
    assert_eq!(entries.len(), n_before + 4); // define+commit, fact+commit
    for (i, e) in entries.iter().enumerate() {
        assert_eq!(e.id.0, i as u64 + 1, "entry at position {i}");
    }
    let txs: Vec<u64> = entries
        .iter()
        .filter_map(|e| match e.body {
            EntryBody::Commit { tx, .. } => Some(tx.0),
            _ => None,
        })
        .collect();
    assert_eq!(txs, (0..txs.len() as u64).collect::<Vec<_>>());
    assert_eq!(*txs.last().unwrap(), w.latest_tx().0);
}

#[test]
fn noop_and_conflict_judgement_sees_pre_reopen_facts() {
    let b = built();
    let mut w = World::open(&b.path).unwrap();
    let g = b.ents.greg;
    let before = bytes(&b.path);
    // age 30 asserted @-100, retracted @-50
    assert_eq!(
        w.assert(g, "age", Value::I64(30), t(-100)).unwrap().changed,
        0
    );
    assert_eq!(
        w.assert(g, "age", Value::I64(30), t(-75)).unwrap().changed,
        0
    );
    assert_eq!(
        w.retract(g, "age", Value::I64(30), t(-50)).unwrap().changed,
        0
    );
    assert_eq!(
        w.retract(g, "age", Value::I64(30), t(-200))
            .unwrap()
            .changed,
        0
    );
    assert_eq!(
        w.retract(g, "age", Value::I64(30), t(0)).unwrap().changed,
        0
    );
    assert_eq!(bytes(&b.path), before);
    // same-tick conflicts against replayed facts
    assert!(matches!(
        w.retract(g, "age", Value::I64(30), t(-100)),
        Err(Error::SameTickConflict { .. })
    ));
    assert!(matches!(
        w.assert(g, "age", Value::I64(30), t(-50)),
        Err(Error::SameTickConflict { .. })
    ));
    assert_eq!(bytes(&b.path), before);
    // out-of-order retract judged at its own tick is stored
    let r = w.retract(g, "age", Value::I64(30), t(-75)).unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(r.tick, Tick(-75));
    assert!(bytes(&b.path).len() > before.len());
}

#[test]
fn tick_defaulting_after_reopen_uses_the_replayed_latest_tick() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let c = w.create_entity("CHARACTER", None).unwrap();
    w.assert(c, "age", Value::I64(1), t(500)).unwrap();
    w.assert(c, "age", Value::I64(2), t(-10)).unwrap(); // prequel: latest stays 500
    drop(w);
    let mut w = World::open(&path).unwrap();
    assert_eq!(w.latest_tick(), Tick(500));
    let r = w.assert(c, "age", Value::I64(3), None).unwrap();
    assert_eq!(r.tick, Tick(500));
    assert_eq!(r.changed, 1);
    let mut txn = w.begin();
    txn.set_default_tick(Tick(-3));
    let r = txn.assert(c, "age", Value::I64(4), None).unwrap();
    assert_eq!(r.tick, Tick(-3));
    txn.commit().unwrap();
}

#[test]
fn genesis_only_world_reopens_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    drop(World::create(&path, schema()).unwrap());
    let before = bytes(&path);
    let mut w = World::open(&path).unwrap();
    assert_eq!(w.latest_tx(), TxId(0));
    assert_eq!(w.latest_tick(), Tick(0));
    assert_eq!(bytes(&path), before);
    let c = w.create_entity("CHARACTER", Some("first")).unwrap();
    assert_eq!(w.format_id(c), "CHARACTER#1");
    let r = w.assert(c, "age", Value::I64(1), None).unwrap();
    assert_eq!(r.tick, Tick(0));
    assert_eq!(w.latest_tx(), TxId(2));
}

#[test]
fn wrong_magic_is_bad_magic() {
    let b = built();
    let mut d = bytes(&b.path);
    d[0] ^= 0xFF;
    std::fs::write(&b.path, &d).unwrap();
    assert_eq!(open_err(&b.path), Error::BadMagic);
    assert_eq!(bytes(&b.path), d);
}

#[test]
fn unknown_version_is_unsupported_version() {
    let b = built();
    let mut d = bytes(&b.path);
    d[8..12].copy_from_slice(&4u32.to_le_bytes());
    std::fs::write(&b.path, &d).unwrap();
    assert_eq!(open_err(&b.path), Error::UnsupportedVersion(4));
    assert_eq!(bytes(&b.path), d);
}

#[test]
fn missing_file_is_io_not_found_and_is_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("absent.kala");
    assert!(matches!(
        open_err(&p),
        Error::Io {
            kind: std::io::ErrorKind::NotFound,
            ..
        }
    ));
    assert!(!p.exists());
}

#[test]
fn header_only_file_without_genesis_is_corrupt_and_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let mut h = LOG_MAGIC.to_vec();
    h.extend_from_slice(&LOG_FORMAT_VERSION.to_le_bytes());
    std::fs::write(&p, &h).unwrap();
    assert!(matches!(open_err(&p), Error::Corrupt(_)));
    assert_eq!(bytes(&p), h);
}

/// Appends a checksum-valid transaction `[frame(kind=4 Fact), Commit]` whose
/// Fact uses `entity` / `attr`, following the documented byte layout.
fn append_fact_txn(path: &Path, entity: u64, attr: u32, value: &Value) {
    let entries = read_log(path).unwrap();
    let last_id = entries.last().unwrap().id.0;
    let last_tx = entries
        .iter()
        .rev()
        .find_map(|e| match e.body {
            EntryBody::Commit { tx, .. } => Some(tx.0),
            _ => None,
        })
        .unwrap();

    let mut payload = vec![4u8];
    payload.extend_from_slice(&(last_id + 1).to_le_bytes());
    payload.extend_from_slice(&entity.to_le_bytes());
    payload.extend_from_slice(&attr.to_le_bytes());
    payload.extend_from_slice(&5i64.to_le_bytes());
    payload.push(0); // assert
    payload.extend_from_slice(&value.encode().unwrap());
    let mut fact = (payload.len() as u32).to_le_bytes().to_vec();
    fact.extend_from_slice(&payload);

    let mut commit = 21u32.to_le_bytes().to_vec(); // kind + entry id + tx + checksum
    commit.push(5);
    commit.extend_from_slice(&(last_id + 2).to_le_bytes());
    commit.extend_from_slice(&(last_tx + 1).to_le_bytes());

    let mut h = crc32fast::Hasher::new();
    h.update(&fact);
    h.update(&commit);
    commit.extend_from_slice(&h.finalize().to_le_bytes());

    // v2: the txn is framed by a begin record (not part of the Commit checksum).
    let mut body = fact;
    body.extend_from_slice(&commit);
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    use std::io::Write;
    f.write_all(&common::txn(last_tx + 1, &body)).unwrap();
}

fn attr_id(path: &Path, name: &str) -> u32 {
    read_log(path)
        .unwrap()
        .iter()
        .find_map(|e| match &e.body {
            EntryBody::DefineAttr { id, def } if def.name == name => Some(id.0),
            _ => None,
        })
        .unwrap()
}

#[test]
fn crafted_valid_txn_is_accepted_by_open_control() {
    // Control for the corrupt cases below: the same hand-built frame with a
    // DEFINEd entity opens fine, so the failures there are about the ID.
    let b = built();
    let age = attr_id(&b.path, "age");
    append_fact_txn(&b.path, b.ents.greg.raw(), age, &Value::I64(77));
    let w = World::open(&b.path).unwrap();
    assert!(
        w.as_of_value(b.ents.greg, "age", &Value::I64(77), Tick(5))
            .unwrap()
    );
}

#[test]
fn checksum_valid_fact_on_undefined_entity_is_corrupt() {
    let b = built();
    let age = attr_id(&b.path, "age");
    // CHARACTER#999 was never defined (type 0, seq 999)
    let ghost = EntityId::new(NodeTypeId(0), 999).unwrap().raw();
    append_fact_txn(&b.path, ghost, age, &Value::I64(1));
    let before = bytes(&b.path);
    assert!(matches!(open_err(&b.path), Error::Corrupt(_)));
    assert_eq!(
        bytes(&b.path),
        before,
        "corruption must not be truncated away"
    );
}

#[test]
fn checksum_valid_fact_on_undefined_attribute_is_corrupt() {
    let b = built();
    append_fact_txn(&b.path, b.ents.greg.raw(), 9999, &Value::I64(1));
    let before = bytes(&b.path);
    assert!(matches!(open_err(&b.path), Error::Corrupt(_)));
    assert_eq!(bytes(&b.path), before);
}

#[test]
fn checksum_valid_fact_with_ref_to_undefined_entity_is_corrupt() {
    let b = built();
    let loc = attr_id(&b.path, "located_in");
    let ghost = EntityId::new(NodeTypeId(1), 999).unwrap();
    append_fact_txn(&b.path, b.ents.greg.raw(), loc, &Value::Ref(ghost));
    let before = bytes(&b.path);
    assert!(matches!(open_err(&b.path), Error::Corrupt(_)));
    assert_eq!(bytes(&b.path), before);
}

// ---------------------------------------------------------------------------
// Hand-built, checksum-valid transactions that the live API could never
// write. Each must make open() fail with Corrupt and leave the file untouched.
// ---------------------------------------------------------------------------

fn frame(kind: u8, id: u64, body: &[u8]) -> Vec<u8> {
    let mut payload = vec![kind];
    payload.extend_from_slice(&id.to_le_bytes());
    payload.extend_from_slice(body);
    let mut f = (payload.len() as u32).to_le_bytes().to_vec();
    f.extend_from_slice(&payload);
    f
}

fn body_define_entity(id: u64, handle: Option<&str>) -> (u8, Vec<u8>) {
    let mut b = id.to_le_bytes().to_vec();
    match handle {
        None => b.push(0),
        Some(h) => {
            b.push(1);
            b.extend_from_slice(&(h.len() as u32).to_le_bytes());
            b.extend_from_slice(h.as_bytes());
        }
    }
    (3, b)
}

fn body_fact(entity: u64, attr: u32, tick: i64, retract: bool, value: &[u8]) -> (u8, Vec<u8>) {
    let mut b = entity.to_le_bytes().to_vec();
    b.extend_from_slice(&attr.to_le_bytes());
    b.extend_from_slice(&tick.to_le_bytes());
    b.push(retract as u8);
    b.extend_from_slice(value);
    (4, b)
}

fn body_define_node_type(id: u16, name: &str) -> (u8, Vec<u8>) {
    let mut b = id.to_le_bytes().to_vec();
    b.extend_from_slice(&(name.len() as u32).to_le_bytes());
    b.extend_from_slice(name.as_bytes());
    (1, b)
}

fn body_define_attr(id: u32, name: &str) -> (u8, Vec<u8>) {
    let mut b = id.to_le_bytes().to_vec();
    b.extend_from_slice(&(name.len() as u32).to_le_bytes());
    b.extend_from_slice(name.as_bytes());
    // i64, state, many, indexed, reverse_cardinality none, direction none, no inverse
    b.extend_from_slice(&[2, 0, 1, 1, 0, 0, 0]);
    (2, b)
}

/// Appends one checksum-valid transaction made of the given (kind, body)
/// entries plus a Commit, with entry IDs and tx continuing the log.
fn append_txn(path: &Path, entries: &[(u8, Vec<u8>)]) {
    let log = read_log(path).unwrap();
    let mut next_id = log.last().unwrap().id.0 + 1;
    let tx = log
        .iter()
        .filter(|e| matches!(e.body, EntryBody::Commit { .. }))
        .count() as u64;
    let mut out = Vec::new();
    for (kind, body) in entries {
        out.extend(frame(*kind, next_id, body));
        next_id += 1;
    }
    let mut commit = 21u32.to_le_bytes().to_vec();
    commit.push(5);
    commit.extend_from_slice(&next_id.to_le_bytes());
    commit.extend_from_slice(&tx.to_le_bytes());
    let mut h = crc32fast::Hasher::new();
    h.update(&out);
    h.update(&commit);
    commit.extend_from_slice(&h.finalize().to_le_bytes());
    out.extend(commit);
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    use std::io::Write;
    f.write_all(&common::txn(tx, &out)).unwrap();
}

fn assert_corrupt_untouched(b: &Built, entries: &[(u8, Vec<u8>)]) {
    append_txn(&b.path, entries);
    let before = bytes(&b.path);
    match World::open(&b.path) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!("expected Corrupt, got {e:?}"),
        Ok(_) => panic!("expected Corrupt, open succeeded"),
    }
    assert_eq!(bytes(&b.path), before, "file must stay untouched");
}

/// The next unused CHARACTER entity ID in `build()`'s world (CHARACTER#4).
fn next_char(b: &Built, seq: u64) -> u64 {
    EntityId::new(b.ents.greg.node_type(), seq).unwrap().raw()
}

#[test]
fn control_well_formed_define_entity_txn_opens() {
    let b = built();
    append_txn(
        &b.path,
        &[body_define_entity(next_char(&b, 4), Some("lisa"))],
    );
    let w = World::open(&b.path).unwrap();
    assert!(w.entity_by_handle("lisa").is_some());
}

#[test]
fn define_entity_seq_skipping_is_corrupt() {
    let b = built();
    assert_corrupt_untouched(&b, &[body_define_entity(next_char(&b, 6), None)]);
}

#[test]
fn define_entity_seq_repeating_is_corrupt() {
    let b = built();
    assert_corrupt_untouched(&b, &[body_define_entity(next_char(&b, 3), None)]);
}

#[test]
fn define_entity_seq_repeating_within_one_txn_is_corrupt() {
    let b = built();
    assert_corrupt_untouched(
        &b,
        &[
            body_define_entity(next_char(&b, 4), None),
            body_define_entity(next_char(&b, 4), None),
        ],
    );
}

#[test]
fn define_entity_with_empty_handle_is_corrupt() {
    let b = built();
    assert_corrupt_untouched(&b, &[body_define_entity(next_char(&b, 4), Some(""))]);
}

#[test]
fn define_entity_with_duplicate_handle_is_corrupt() {
    let b = built();
    assert_corrupt_untouched(&b, &[body_define_entity(next_char(&b, 4), Some("greg"))]);
}

#[test]
fn define_entity_with_duplicate_handle_within_one_txn_is_corrupt() {
    let b = built();
    assert_corrupt_untouched(
        &b,
        &[
            body_define_entity(next_char(&b, 4), Some("twin")),
            body_define_entity(next_char(&b, 5), Some("twin")),
        ],
    );
}

#[test]
fn define_entity_of_undeclared_node_type_is_corrupt() {
    let b = built();
    let id = EntityId::new(NodeTypeId(77), 1).unwrap().raw();
    assert_corrupt_untouched(&b, &[body_define_entity(id, None)]);
}

#[test]
fn define_entity_with_edge_tag_bit_is_corrupt() {
    let b = built();
    let id = next_char(&b, 4) | (1 << 63);
    assert_corrupt_untouched(&b, &[body_define_entity(id, None)]);
}

#[test]
fn fact_on_edge_tagged_subject_is_corrupt() {
    let b = built();
    let age = attr_id(&b.path, "age");
    // edge#1 is defined by the fixture; edge#999 is not
    let edge = 999u64 | (1 << 63);
    let v = Value::I64(1).encode().unwrap();
    assert_corrupt_untouched(&b, &[body_fact(edge, age, 999, false, &v)]);
}

#[test]
fn fact_with_edge_tagged_ref_target_is_corrupt() {
    let b = built();
    let loc = attr_id(&b.path, "located_in");
    let edge = EntityId::from_bytes((b.ents.varrow.raw() | (1 << 63)).to_be_bytes());
    let v = Value::Ref(edge).encode().unwrap();
    assert_corrupt_untouched(&b, &[body_fact(b.ents.greg.raw(), loc, 999, false, &v)]);
}

#[test]
fn fact_before_the_define_entity_it_uses_in_the_same_txn_is_corrupt() {
    let b = built();
    let age = attr_id(&b.path, "age");
    let new = next_char(&b, 4);
    let v = Value::I64(1).encode().unwrap();
    assert_corrupt_untouched(
        &b,
        &[
            body_fact(new, age, 999, false, &v),
            body_define_entity(new, None),
        ],
    );
}

#[test]
fn control_define_entity_then_fact_in_one_txn_opens() {
    let b = built();
    let age = attr_id(&b.path, "age");
    let new = next_char(&b, 4);
    let v = Value::I64(1).encode().unwrap();
    append_txn(
        &b.path,
        &[
            body_define_entity(new, None),
            body_fact(new, age, 999, false, &v),
        ],
    );
    assert!(World::open(&b.path).is_ok());
}

#[test]
fn define_node_type_after_genesis_is_corrupt() {
    let b = built();
    assert_corrupt_untouched(&b, &[body_define_node_type(50, "ITEM")]);
}

#[test]
fn define_node_type_reusing_an_existing_id_after_genesis_is_corrupt() {
    let b = built();
    assert_corrupt_untouched(&b, &[body_define_node_type(0, "OTHER")]);
}

#[test]
fn define_attr_after_genesis_is_corrupt() {
    let b = built();
    assert_corrupt_untouched(&b, &[body_define_attr(500, "rank")]);
}

#[test]
fn retract_of_an_event_attribute_is_corrupt() {
    let b = built();
    let bought = attr_id(&b.path, "bought");
    let v = Value::Ref(b.ents.greg).encode().unwrap();
    assert_corrupt_untouched(&b, &[body_fact(b.ents.edwin.raw(), bought, 999, true, &v)]);
}

#[test]
fn fact_with_wrong_value_type_is_corrupt() {
    let b = built();
    let age = attr_id(&b.path, "age");
    let v = Value::Bool(true).encode().unwrap();
    assert_corrupt_untouched(&b, &[body_fact(b.ents.greg.raw(), age, 999, false, &v)]);
}

#[test]
fn fact_with_indexed_string_over_256_bytes_is_corrupt() {
    let b = built();
    let name = attr_id(&b.path, "name");
    let v = Value::Str("x".repeat(257)).encode().unwrap();
    assert_corrupt_untouched(&b, &[body_fact(b.ents.greg.raw(), name, 999, false, &v)]);
}

#[test]
fn control_indexed_string_of_exactly_256_bytes_opens() {
    let b = built();
    let name = attr_id(&b.path, "name");
    let v = Value::Str("x".repeat(256)).encode().unwrap();
    append_txn(
        &b.path,
        &[body_fact(b.ents.greg.raw(), name, 999, false, &v)],
    );
    assert!(World::open(&b.path).is_ok());
}

#[test]
fn fact_repeating_an_earlier_txns_fact_with_the_same_op_is_corrupt() {
    // build(): greg age 30 asserted @-100 (an earlier committed txn).
    let b = built();
    let age = attr_id(&b.path, "age");
    let v = Value::I64(30).encode().unwrap();
    assert_corrupt_untouched(&b, &[body_fact(b.ents.greg.raw(), age, -100, false, &v)]);
}

#[test]
fn fact_with_the_opposite_op_at_an_earlier_txns_tick_is_corrupt() {
    let b = built();
    let age = attr_id(&b.path, "age");
    let v = Value::I64(30).encode().unwrap();
    assert_corrupt_untouched(&b, &[body_fact(b.ents.greg.raw(), age, -100, true, &v)]);
}

#[test]
fn two_facts_with_the_same_key_inside_one_txn_are_corrupt() {
    let b = built();
    let age = attr_id(&b.path, "age");
    let v = Value::I64(31).encode().unwrap();
    assert_corrupt_untouched(
        &b,
        &[
            body_fact(b.ents.greg.raw(), age, 999, false, &v),
            body_fact(b.ents.greg.raw(), age, 999, false, &v),
        ],
    );
}

#[test]
fn control_same_value_at_a_different_tick_opens() {
    let b = built();
    let age = attr_id(&b.path, "age");
    let v = Value::I64(30).encode().unwrap();
    // Slice 3 item 0c: the value must NOT already hold at the new tick, or a
    // live write would have been a no-op. 30 holds over [-100, -50), so the
    // control moves to -49 (just after the retract); it was -99 in slice 2.
    append_txn(
        &b.path,
        &[body_fact(b.ents.greg.raw(), age, -49, false, &v)],
    );
    assert!(World::open(&b.path).is_ok());
}

#[test]
fn same_value_asserted_at_a_different_tick_inside_its_held_interval_is_corrupt() {
    // the slice-2 control (assert 30 @-99), now a replay no-op: Corrupt
    let b = built();
    let age = attr_id(&b.path, "age");
    let v = Value::I64(30).encode().unwrap();
    assert_corrupt_untouched(&b, &[body_fact(b.ents.greg.raw(), age, -99, false, &v)]);
}

#[test]
fn f64_negative_zero_bytes_in_a_fact_frame_pin() {
    // Non-canonical -0.0 (tag 3, then the sign-bit trick applied to -0.0).
    let b = built();
    let h = attr_id(&b.path, "height");
    let bits = (-0.0f64).to_bits();
    let enc = if bits >> 63 == 1 {
        !bits
    } else {
        bits ^ (1 << 63)
    };
    let mut v = vec![3u8];
    v.extend_from_slice(&enc.to_be_bytes());
    assert_corrupt_untouched(&b, &[body_fact(b.ents.greg.raw(), h, 999, false, &v)]);
}
