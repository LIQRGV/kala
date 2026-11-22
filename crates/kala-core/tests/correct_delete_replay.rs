//! Slice 3, item 4: replay rules for `Deleted` (kind 7) entries
//! (docs/slice-3-decisions.md, "Item 4"). Every crafted log has VALID framing
//! and CRCs, so the only thing wrong is the rule under test; every corrupt
//! shape has a positive control (the same log with the offending field fixed
//! must open). Logs are built byte by byte from the documented layout.
//!
//! kind 7 `Deleted`: `of u64 | rev u32 | entity u64 | attr u32 | tick i64 | value (rest)`
mod common;

use common::{commit_frame, frame, txn};
use kala_core::log::{EntryBody, read_log};
use kala_core::*;
use std::path::{Path, PathBuf};

const K_FACT: u8 = 4;
const K_DEFINE_EDGE: u8 = 6;
const K_DELETED: u8 = 7;

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        )
}

fn entity(raw: u64) -> EntityId {
    EntityId::from_bytes(raw.to_be_bytes())
}

struct Base {
    _dir: tempfile::TempDir,
    path: PathBuf,
    bytes: Vec<u8>,
    greg: u64,
    varrow: u64,
    name: u32,
    ally_of: u32,
    /// entry id of `Greg name "Greg" @1`
    fact: u64,
    /// entry id of a DefineEntity
    define: u64,
    /// entry id of the first Commit
    commit: u64,
    next_entry: u64,
    next_tx: u64,
}

/// genesis + greg + varrow + `Greg name "Greg" @1` (tx 3).
fn base() -> Base {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let (greg, varrow);
    {
        let mut w = World::create(&path, schema()).unwrap();
        let g = w.create_entity("CHARACTER", Some("greg")).unwrap();
        let v = w.create_entity("PLACE", Some("varrow")).unwrap();
        w.assert(g, "name", Value::Str("Greg".into()), Some(Tick(1)))
            .unwrap();
        greg = u64::from_be_bytes(g.to_bytes());
        varrow = u64::from_be_bytes(v.to_bytes());
        assert!(g < v, "CHARACTER sorts before PLACE");
    }
    let log = read_log(&path).unwrap();
    let attr = |n: &str| {
        log.iter()
            .find_map(|e| match &e.body {
                EntryBody::DefineAttr { id, def } if def.name == n => Some(id.0),
                _ => None,
            })
            .unwrap()
    };
    let fact = log
        .iter()
        .find_map(|e| match &e.body {
            EntryBody::Fact { .. } => Some(e.id.0),
            _ => None,
        })
        .unwrap();
    let define = log
        .iter()
        .find_map(|e| match &e.body {
            EntryBody::DefineEntity { .. } => Some(e.id.0),
            _ => None,
        })
        .unwrap();
    let commit = log
        .iter()
        .find_map(|e| match &e.body {
            EntryBody::Commit { .. } => Some(e.id.0),
            _ => None,
        })
        .unwrap();
    Base {
        bytes: std::fs::read(&path).unwrap(),
        name: attr("name"),
        ally_of: attr("ally_of"),
        fact,
        define,
        commit,
        next_entry: log.last().unwrap().id.0 + 1,
        next_tx: 4,
        greg,
        varrow,
        path,
        _dir: dir,
    }
}

fn fact_payload(e: u64, attr: u32, tick: i64, retract: bool, v: &Value) -> Vec<u8> {
    let mut b = e.to_le_bytes().to_vec();
    b.extend_from_slice(&attr.to_le_bytes());
    b.extend_from_slice(&tick.to_le_bytes());
    b.push(u8::from(retract));
    b.extend_from_slice(&v.encode().unwrap());
    b
}

fn deleted_payload(of: u64, rev: u32, e: u64, attr: u32, tick: i64, v: &Value) -> Vec<u8> {
    let mut b = of.to_le_bytes().to_vec();
    b.extend_from_slice(&rev.to_le_bytes());
    b.extend_from_slice(&e.to_le_bytes());
    b.extend_from_slice(&attr.to_le_bytes());
    b.extend_from_slice(&tick.to_le_bytes());
    b.extend_from_slice(&v.encode().unwrap());
    b
}

fn define_edge(id: u64, source: u64, attr: u32, target: u64) -> Vec<u8> {
    let mut b = id.to_le_bytes().to_vec();
    b.extend_from_slice(&source.to_le_bytes());
    b.extend_from_slice(&attr.to_le_bytes());
    b.extend_from_slice(&target.to_le_bytes());
    b
}

type Ent = (u8, Vec<u8>);

/// A growing file: each `push` appends one valid txn.
struct Log<'a> {
    _b: &'a Base,
    bytes: Vec<u8>,
    next_entry: u64,
    next_tx: u64,
}

impl<'a> Log<'a> {
    fn new(b: &'a Base) -> Log<'a> {
        Log {
            _b: b,
            bytes: b.bytes.clone(),
            next_entry: b.next_entry,
            next_tx: b.next_tx,
        }
    }

    /// Appends a txn; returns the entry ids given to `entries`.
    fn push(&mut self, entries: &[Ent]) -> Vec<u64> {
        let mut body = Vec::new();
        let mut ids = Vec::new();
        let mut id = self.next_entry;
        for (k, p) in entries {
            body.extend(frame(*k, id, p));
            ids.push(id);
            id += 1;
        }
        body.extend(commit_frame(&body.clone(), id, self.next_tx));
        self.bytes.extend(txn(self.next_tx, &body));
        self.next_entry = id + 1;
        self.next_tx += 1;
        ids
    }
}

impl Base {
    fn name_val(&self) -> Value {
        Value::Str("Greg".into())
    }
    /// A Deleted of the base fact with the given fields.
    fn del(&self, of: u64, rev: u32, tick: i64, v: &Value) -> Ent {
        (
            K_DELETED,
            deleted_payload(of, rev, self.greg, self.name, tick, v),
        )
    }
    /// The correct Deleted of the base fact.
    fn good_del(&self) -> Ent {
        self.del(self.fact, 1, 1, &self.name_val())
    }
    fn greg_name_fact(&self, tick: i64, retract: bool, v: &str) -> Ent {
        (
            K_FACT,
            fact_payload(self.greg, self.name, tick, retract, &Value::Str(v.into())),
        )
    }
}

fn open_bytes(path: &Path, file: &[u8]) -> Result<World, Error> {
    std::fs::write(path, file).unwrap();
    World::open(path)
}

fn expect_corrupt(b: &Base, file: &[u8], label: &str) {
    match open_bytes(&b.path, file) {
        Err(Error::Corrupt(m)) => eprintln!("{label}: {m}"),
        Err(e) => panic!("{label}: expected Corrupt, got {e:?}"),
        Ok(_) => panic!("{label}: expected Corrupt, got Ok"),
    }
    assert_eq!(
        std::fs::read(&b.path).unwrap(),
        file,
        "{label}: file touched"
    );
}

fn expect_ok(b: &Base, file: &[u8], label: &str) -> World {
    match open_bytes(&b.path, file) {
        Ok(w) => w,
        Err(e) => panic!("{label}: control must open, got {e:?}"),
    }
}

// ------------------------------------------------------------- positive

#[test]
fn control_a_well_formed_deleted_opens_and_hides_the_fact() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[b.good_del()]);
    let w = expect_ok(&b, &l.bytes, "good delete");
    assert!(
        !w.as_of_value(entity(b.greg), "name", &b.name_val(), Tick(5))
            .unwrap()
    );
    assert!(
        w.at_canon(TxId(3))
            .as_of_value(entity(b.greg), "name", &b.name_val(), Tick(5))
            .unwrap()
    );
}

#[test]
fn a_base_fact_replayed_at_a_deleted_key_in_a_later_txn_opens() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[b.good_del()]);
    l.push(&[b.greg_name_fact(1, false, "Greg")]);
    let w = expect_ok(&b, &l.bytes, "delete then re-assert in later txn");
    assert!(
        w.as_of_value(entity(b.greg), "name", &b.name_val(), Tick(5))
            .unwrap()
    );
    assert_eq!(w.latest_tx(), TxId(5));
}

// ------------------------------------------------------- `of` rules

#[test]
fn of_pointing_forward_is_corrupt() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[b.del(b.next_entry + 50, 1, 1, &b.name_val())]);
    expect_corrupt(&b, &l.bytes, "of forward");
}

#[test]
fn of_pointing_at_itself_is_corrupt() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[b.del(b.next_entry, 1, 1, &b.name_val())]);
    expect_corrupt(&b, &l.bytes, "of itself");
}

#[test]
fn of_pointing_at_a_fact_of_the_same_txn_is_corrupt() {
    let b = base();
    let zed = Value::Str("Zed".into());
    let mut l = Log::new(&b);
    // entries: [Fact Zed @10 (id n), Deleted of n]
    l.push(&[
        b.greg_name_fact(10, false, "Zed"),
        b.del(b.next_entry, 1, 10, &zed),
    ]);
    expect_corrupt(&b, &l.bytes, "of same-txn fact");
}

#[test]
fn of_pointing_at_a_fact_of_the_same_txn_control_two_txns_open() {
    let b = base();
    let zed = Value::Str("Zed".into());
    let mut l = Log::new(&b);
    l.push(&[b.greg_name_fact(10, false, "Zed")]);
    l.push(&[b.del(b.next_entry, 1, 10, &zed)]);
    expect_ok(&b, &l.bytes, "fact then delete in later txn");
}

#[test]
fn of_pointing_at_a_define_is_corrupt() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[b.del(b.define, 1, 1, &b.name_val())]);
    expect_corrupt(&b, &l.bytes, "of define");
}

#[test]
fn of_pointing_at_a_commit_is_corrupt() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[b.del(b.commit, 1, 1, &b.name_val())]);
    expect_corrupt(&b, &l.bytes, "of commit");
}

#[test]
fn of_pointing_at_a_deleted_entry_is_corrupt() {
    let b = base();
    let mut l = Log::new(&b);
    let ids = l.push(&[b.good_del()]);
    // the chain is deleted; point the second marker at the first Deleted and
    // re-assert first so only the `of` is wrong
    l.push(&[b.greg_name_fact(1, false, "Greg")]);
    l.push(&[b.del(ids[0], 2, 1, &b.name_val())]);
    expect_corrupt(&b, &l.bytes, "of deleted");
}

#[test]
fn of_pointing_at_a_deleted_entry_control_pointing_at_the_base_opens() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[b.good_del()]);
    l.push(&[b.greg_name_fact(1, false, "Greg")]);
    // the re-asserted fact is a NEW base fact (its own chain): delete it
    let refact = l.next_entry - 2;
    l.push(&[b.del(refact, 1, 1, &b.name_val())]);
    expect_ok(&b, &l.bytes, "delete of the re-asserted base fact");
}

// ------------------------------------------------------- rev rules

#[test]
fn first_correction_rev_0_is_corrupt() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[b.del(b.fact, 0, 1, &b.name_val())]);
    expect_corrupt(&b, &l.bytes, "rev 0");
}

#[test]
fn first_correction_rev_2_is_corrupt() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[b.del(b.fact, 2, 1, &b.name_val())]);
    expect_corrupt(&b, &l.bytes, "rev 2");
}

#[test]
fn first_correction_rev_u32_max_is_corrupt() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[b.del(b.fact, u32::MAX, 1, &b.name_val())]);
    expect_corrupt(&b, &l.bytes, "rev max");
}

// ------------------------------------------------------- key rules

#[test]
fn marker_at_the_wrong_value_is_corrupt() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[b.del(b.fact, 1, 1, &Value::Str("Other".into()))]);
    expect_corrupt(&b, &l.bytes, "wrong value");
}

#[test]
fn marker_at_the_wrong_tick_is_corrupt() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[b.del(b.fact, 1, 2, &b.name_val())]);
    expect_corrupt(&b, &l.bytes, "wrong tick");
}

#[test]
fn marker_at_the_wrong_entity_is_corrupt() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[(
        K_DELETED,
        deleted_payload(b.fact, 1, b.varrow, b.name, 1, &b.name_val()),
    )]);
    expect_corrupt(&b, &l.bytes, "wrong entity");
}

// ------------------------------------------------------- chain rules

#[test]
fn a_second_deleted_for_a_deleted_chain_is_corrupt() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[b.good_del()]);
    l.push(&[b.del(b.fact, 2, 1, &b.name_val())]);
    expect_corrupt(&b, &l.bytes, "double delete");
}

#[test]
fn a_second_deleted_for_the_same_chain_in_one_txn_is_corrupt() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[b.good_del(), b.del(b.fact, 2, 1, &b.name_val())]);
    expect_corrupt(&b, &l.bytes, "double delete same txn");
}

#[test]
fn deleted_and_fact_at_the_same_key_in_one_txn_is_corrupt() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[b.good_del(), b.greg_name_fact(1, false, "Greg")]);
    expect_corrupt(&b, &l.bytes, "Deleted + Fact one key one txn");
}

// ------------------------------------------------------- bidirectional

/// tx4: DefineEdge + `greg ally_of varrow @1` (greg < varrow, normalised).
fn with_pair(b: &Base) -> (Log<'_>, u64) {
    let mut l = Log::new(b);
    let v = Value::Ref(entity(b.varrow));
    let ids = l.push(&[
        (
            K_DEFINE_EDGE,
            define_edge(1 | (1 << 63), b.greg, b.ally_of, b.varrow),
        ),
        (K_FACT, fact_payload(b.greg, b.ally_of, 1, false, &v)),
    ]);
    (l, ids[1])
}

#[test]
fn bidirectional_marker_with_entity_above_value_is_corrupt() {
    let b = base();
    let (mut l, f) = with_pair(&b);
    // reversed: entity = varrow, value = Ref(greg)
    l.push(&[(
        K_DELETED,
        deleted_payload(f, 1, b.varrow, b.ally_of, 1, &Value::Ref(entity(b.greg))),
    )]);
    expect_corrupt(&b, &l.bytes, "reversed bidirectional marker");
}

#[test]
fn bidirectional_marker_control_normalised_order_opens() {
    let b = base();
    let (mut l, f) = with_pair(&b);
    l.push(&[(
        K_DELETED,
        deleted_payload(f, 1, b.greg, b.ally_of, 1, &Value::Ref(entity(b.varrow))),
    )]);
    let w = expect_ok(&b, &l.bytes, "normalised marker");
    assert!(
        w.as_of(entity(b.varrow), "ally_of", Tick(5))
            .unwrap()
            .is_empty()
    );
}
