//! Slice 3, item 0(c): replay validates like live writes
//! (docs/slice-3-decisions.md). Each crafted log below has VALID CRCs and
//! framing, so the only thing wrong with it is the rule under test. Every
//! shape has a positive control: the same log with the offending entry fixed
//! must open, so a red here is about the rule and not about the byte layout.
//!
//! Logs are built byte by byte from the documented layout (tests/common):
//! a real world supplies the genesis and a few committed txns, then crafted
//! transactions are appended.
//!
//! Log version note: these tests append to whatever version the library
//! writes (the header is copied from the file), so they keep working when
//! slice 3 bumps the format to 3.
mod common;

use common::{commit_frame, frame, txn};
use kala_core::log::{EntryBody, read_log};
use kala_core::*;
use std::path::{Path, PathBuf};

const K_FACT: u8 = 4;
const K_DEFINE_EDGE: u8 = 6;

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
        .attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        )
}

struct Base {
    _dir: tempfile::TempDir,
    path: PathBuf,
    bytes: Vec<u8>,
    greg: u64,
    varrow: u64,
    located_in: u32,
    name: u32,
    ally_of: u32,
    next_entry: u64,
    next_tx: u64,
}

/// genesis + greg + varrow + `Greg name "Greg" @1` (tx 3). Nothing else is
/// held: no edge exists yet.
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
        assert_eq!(w.latest_tx(), TxId(3));
        greg = u64::from_be_bytes(g.to_bytes());
        varrow = u64::from_be_bytes(v.to_bytes());
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
    Base {
        bytes: std::fs::read(&path).unwrap(),
        located_in: attr("located_in"),
        name: attr("name"),
        ally_of: attr("ally_of"),
        next_entry: log.last().unwrap().id.0 + 1,
        next_tx: 4,
        greg,
        varrow,
        path,
        _dir: dir,
    }
}

fn fact(entity: u64, attr: u32, tick: i64, retract: bool, v: &Value) -> Vec<u8> {
    let mut b = entity.to_le_bytes().to_vec();
    b.extend_from_slice(&attr.to_le_bytes());
    b.extend_from_slice(&tick.to_le_bytes());
    b.push(u8::from(retract));
    b.push(0); // precision byte (v4): Exact
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

impl Base {
    /// Appends one txn with the given (kind, payload) entries + a valid Commit
    /// and returns the full file bytes.
    fn with_txn(&self, entries: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        let mut id = self.next_entry;
        for (k, p) in entries {
            body.extend(frame(*k, id, p));
            id += 1;
        }
        let c = commit_frame(common::link(&self.bytes), &body, id, self.next_tx);
        body.extend(c);
        let mut f = self.bytes.clone();
        f.extend(txn(self.next_tx, &body));
        f
    }

    /// Appends two txns (the second sees the first).
    fn with_two(&self, a: &[(u8, Vec<u8>)], b: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut mid = Base {
            _dir: tempfile::tempdir().unwrap(),
            path: PathBuf::new(),
            bytes: self.with_txn(a),
            greg: self.greg,
            varrow: self.varrow,
            located_in: self.located_in,
            name: self.name,
            ally_of: self.ally_of,
            next_entry: self.next_entry + a.len() as u64 + 1,
            next_tx: self.next_tx + 1,
        };
        mid.path = self.path.clone();
        mid.with_txn(b)
    }

    fn name_fact(&self, tick: i64, retract: bool, s: &str) -> (u8, Vec<u8>) {
        (
            K_FACT,
            fact(self.greg, self.name, tick, retract, &Value::Str(s.into())),
        )
    }
}

fn open_bytes(path: &Path, file: &[u8]) -> Result<World, Error> {
    std::fs::write(path, file).unwrap();
    World::open(path)
}

fn expect_corrupt(b: &Base, file: &[u8], label: &str) {
    match open_bytes(&b.path, file) {
        Err(Error::Corrupt(_)) => {}
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

// ---- retract of a value not held at its tick ----

#[test]
fn retract_of_a_value_that_is_not_held_is_corrupt() {
    let b = base();
    let f = b.with_txn(&[b.name_fact(5, true, "Nobody")]);
    expect_corrupt(&b, &f, "retract Nobody @5");
}

#[test]
fn retract_of_a_value_that_is_not_held_is_corrupt_control_held_value_opens() {
    let b = base();
    let f = b.with_txn(&[b.name_fact(5, true, "Greg")]);
    let w = expect_ok(&b, &f, "retract Greg @5");
    assert_eq!(w.latest_tx(), TxId(4));
}

#[test]
fn retract_before_the_value_starts_to_hold_is_corrupt() {
    // "Greg" holds from tick 1; a retract at tick 0 is judged at tick 0.
    let b = base();
    let f = b.with_txn(&[b.name_fact(0, true, "Greg")]);
    expect_corrupt(&b, &f, "retract Greg @0");
}

// ---- repeated assert of a value already held ----

#[test]
fn repeated_assert_in_a_later_txn_is_corrupt() {
    let b = base();
    let f = b.with_txn(&[b.name_fact(3, false, "Greg")]);
    expect_corrupt(&b, &f, "assert Greg @3 while held since 1");
}

#[test]
fn repeated_assert_in_a_later_txn_control_new_value_opens() {
    let b = base();
    let f = b.with_txn(&[b.name_fact(3, false, "Gregor")]);
    expect_ok(&b, &f, "assert Gregor @3");
}

#[test]
fn repeated_assert_within_one_txn_at_a_later_tick_is_corrupt() {
    let b = base();
    let f = b.with_txn(&[b.name_fact(10, false, "Zed"), b.name_fact(20, false, "Zed")]);
    expect_corrupt(&b, &f, "assert Zed @10 then @20 in one txn");
}

#[test]
fn repeated_assert_within_one_txn_control_other_value_opens() {
    let b = base();
    let f = b.with_txn(&[b.name_fact(10, false, "Zed"), b.name_fact(20, false, "Yan")]);
    expect_ok(&b, &f, "assert Zed @10 then Yan @20");
}

#[test]
fn retract_after_an_earlier_entry_of_the_same_txn_holds_it_opens() {
    // Judged against earlier entries of the same txn: Zed holds from 10, so
    // a retract at 20 is a real change. (Positive control for the same-txn
    // rule: the order of ticks matters, not the txn boundary.)
    let b = base();
    let f = b.with_txn(&[b.name_fact(10, false, "Zed"), b.name_fact(20, true, "Zed")]);
    expect_ok(&b, &f, "assert Zed @10, retract Zed @20");
}

#[test]
fn retract_of_a_same_txn_value_before_it_holds_is_corrupt() {
    let b = base();
    let f = b.with_txn(&[b.name_fact(10, false, "Zed"), b.name_fact(5, true, "Zed")]);
    expect_corrupt(&b, &f, "assert Zed @10, retract Zed @5");
}

// ---- DefineEdge without an assert of that triple in its txn ----

fn edge_triple(b: &Base) -> (u8, Vec<u8>) {
    (
        K_DEFINE_EDGE,
        define_edge(1 | (1 << 63), b.greg, b.located_in, b.varrow),
    )
}

#[test]
fn define_edge_without_an_assert_of_its_triple_is_corrupt() {
    let b = base();
    // The txn has a fact, but not an assert of (greg, located_in, varrow).
    let f = b.with_txn(&[edge_triple(&b), b.name_fact(10, false, "Zed")]);
    expect_corrupt(&b, &f, "DefineEdge + unrelated fact");
}

#[test]
fn define_edge_with_only_a_retract_of_its_triple_is_corrupt() {
    // A retract of the triple is not an assert (and would itself be a no-op).
    let b = base();
    let retract = (
        K_FACT,
        fact(
            b.greg,
            b.located_in,
            10,
            true,
            &Value::Ref(entity(b.varrow)),
        ),
    );
    let f = b.with_txn(&[edge_triple(&b), retract]);
    expect_corrupt(&b, &f, "DefineEdge + retract of triple");
}

#[test]
fn define_edge_with_its_assert_opens_control() {
    let b = base();
    let assert = (
        K_FACT,
        fact(
            b.greg,
            b.located_in,
            10,
            false,
            &Value::Ref(entity(b.varrow)),
        ),
    );
    let f = b.with_txn(&[edge_triple(&b), assert]);
    let w = expect_ok(&b, &f, "DefineEdge + assert");
    assert_eq!(w.latest_tx(), TxId(4));
}

fn entity(raw: u64) -> EntityId {
    EntityId::from_bytes(raw.to_be_bytes())
}

// ---- Commit-only transactions ----

#[test]
fn a_commit_only_txn_after_genesis_is_corrupt() {
    let b = base();
    let f = b.with_txn(&[]);
    expect_corrupt(&b, &f, "commit-only txn");
}

#[test]
fn a_commit_only_txn_after_genesis_is_corrupt_control_with_a_fact_opens() {
    let b = base();
    let f = b.with_txn(&[b.name_fact(10, false, "Zed")]);
    expect_ok(&b, &f, "one-fact txn");
}

#[test]
fn a_commit_only_txn_between_valid_txns_is_corrupt() {
    let b = base();
    let f = b.with_two(&[], &[b.name_fact(10, false, "Zed")]);
    expect_corrupt(&b, &f, "commit-only txn followed by a valid one");
}

#[test]
fn a_commit_only_txn_after_a_valid_one_is_corrupt() {
    let b = base();
    let f = b.with_two(&[b.name_fact(10, false, "Zed")], &[]);
    expect_corrupt(&b, &f, "valid txn then commit-only");
}

// ---- item 0 CONFIRM edge cases (sdet): bidirectional and multi-edge txns ----

fn edge_id(n: u64) -> u64 {
    n | (1 << 63)
}

fn ref_fact(e: u64, attr: u32, tick: i64, retract: bool, target: u64) -> (u8, Vec<u8>) {
    (
        K_FACT,
        fact(e, attr, tick, retract, &Value::Ref(entity(target))),
    )
}

/// (smaller, larger) of the two entity ids: bidirectional facts are stored
/// normalised with entity <= value.
fn ordered(b: &Base) -> (u64, u64) {
    (b.greg.min(b.varrow), b.greg.max(b.varrow))
}

#[test]
fn bidirectional_define_edge_with_the_normalised_assert_opens_control() {
    let b = base();
    let (lo, hi) = ordered(&b);
    let f = b.with_txn(&[
        (K_DEFINE_EDGE, define_edge(edge_id(1), lo, b.ally_of, hi)),
        ref_fact(lo, b.ally_of, 10, false, hi),
    ]);
    let w = expect_ok(&b, &f, "bidirectional DefineEdge + normalised assert");
    assert_eq!(w.latest_tx(), TxId(4));
}

#[test]
fn bidirectional_define_edge_satisfied_only_by_the_reversed_spelling_is_corrupt() {
    // The DefineEdge is normalised, the only fact is spelled (larger, ally_of,
    // smaller). Replay rejects a bidirectional fact with entity > value, so
    // the reversed spelling can neither be stored nor satisfy the DefineEdge.
    let b = base();
    let (lo, hi) = ordered(&b);
    assert!(lo < hi);
    let f = b.with_txn(&[
        (K_DEFINE_EDGE, define_edge(edge_id(1), lo, b.ally_of, hi)),
        ref_fact(hi, b.ally_of, 10, false, lo),
    ]);
    expect_corrupt(&b, &f, "DefineEdge(lo,hi) + assert(hi,lo)");
}

#[test]
fn bidirectional_fact_with_entity_greater_than_value_is_corrupt_even_without_define_edge() {
    let b = base();
    let (lo, hi) = ordered(&b);
    let f = b.with_txn(&[ref_fact(hi, b.ally_of, 10, false, lo)]);
    expect_corrupt(&b, &f, "assert(hi, ally_of, lo) alone");
}

#[test]
fn two_define_edges_with_only_one_triple_asserted_is_corrupt() {
    let b = base();
    let e1 = (
        K_DEFINE_EDGE,
        define_edge(edge_id(1), b.greg, b.located_in, b.varrow),
    );
    let e2 = (
        K_DEFINE_EDGE,
        define_edge(edge_id(2), b.varrow, b.located_in, b.greg),
    );
    let only_first = ref_fact(b.greg, b.located_in, 10, false, b.varrow);
    let only_second = ref_fact(b.varrow, b.located_in, 10, false, b.greg);
    expect_corrupt(
        &b,
        &b.with_txn(&[e1.clone(), e2.clone(), only_first.clone()]),
        "edge 2 never asserted",
    );
    expect_corrupt(
        &b,
        &b.with_txn(&[e1.clone(), e2.clone(), only_second.clone()]),
        "edge 1 never asserted",
    );
    // control: both asserted opens
    expect_ok(
        &b,
        &b.with_txn(&[e1, e2, only_first, only_second]),
        "both edges asserted",
    );
}

#[test]
fn edge_property_fact_on_a_new_edge_whose_triple_is_not_asserted_in_the_txn_is_corrupt() {
    let b = base();
    let define = (
        K_DEFINE_EDGE,
        define_edge(edge_id(1), b.greg, b.located_in, b.varrow),
    );
    let prop = (
        K_FACT,
        fact(edge_id(1), b.name, 10, false, &Value::Str("note".into())),
    );
    expect_corrupt(
        &b,
        &b.with_txn(&[define.clone(), prop.clone()]),
        "property, no triple",
    );
    // control: the triple is asserted before the property
    let triple = ref_fact(b.greg, b.located_in, 10, false, b.varrow);
    expect_ok(
        &b,
        &b.with_txn(&[define, triple, prop]),
        "triple then property",
    );
}
