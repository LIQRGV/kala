//! Slice 3, item 4: replay rules for `Revised` (kind 8) entries and the
//! Deleted+Revised pairs a Replace writes (docs/slice-3-decisions.md,
//! "Item 4"). Every crafted log has VALID framing and CRCs, so the only thing
//! wrong is the rule under test; every corrupt shape has a positive control
//! (the same log with the offending field fixed must open).
//!
//! kind 7 `Deleted`: `of u64 | rev u32 | entity u64 | attr u32 | tick i64 | value (rest)`
//! kind 8 `Revised`: `of u64 | rev u32 | entity u64 | attr u32 | tick i64 | op u8 | value (rest)`
mod common;

use common::{commit_frame, frame, txn};
use kala_core::log::{EntryBody, read_log};
use kala_core::*;
use std::path::{Path, PathBuf};

const K_FACT: u8 = 4;
const K_DEFINE_EDGE: u8 = 6;
const K_DELETED: u8 = 7;
const K_REVISED: u8 = 8;

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
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        )
        .attribute(AttrDef::new(
            "bought",
            ValueType::Ref,
            Kind::Event,
            Cardinality::Many,
        ))
}

fn entity(raw: u64) -> EntityId {
    EntityId::from_bytes(raw.to_be_bytes())
}

fn raw(e: EntityId) -> u64 {
    u64::from_be_bytes(e.to_bytes())
}

fn r(x: u64) -> Value {
    Value::Ref(entity(x))
}

struct Base {
    _dir: tempfile::TempDir,
    path: PathBuf,
    bytes: Vec<u8>,
    greg: u64,
    edwin: u64,
    varrow: u64,
    ashford: u64,
    name: u32,
    located: u32,
    ally: u32,
    bought: u32,
    /// Greg located_in Varrow: assert @-300
    fa: u64,
    /// retract @-250
    fr: u64,
    /// assert @-200
    fa2: u64,
    /// retract @-150
    fr2: u64,
    /// Greg name "Greg" @1
    fname: u64,
    /// Greg ally_of Edwin @1 (stored greg, edwin)
    fpair: u64,
    /// Greg bought Varrow @5 (event)
    fev: u64,
    next_edge: u64,
    next_entry: u64,
    next_tx: u64,
}

fn base() -> Base {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let ids;
    {
        let mut w = World::create(&path, schema()).unwrap();
        let g = w.create_entity("CHARACTER", Some("greg")).unwrap();
        let e = w.create_entity("CHARACTER", Some("edwin")).unwrap();
        let v = w.create_entity("PLACE", Some("varrow")).unwrap();
        let a = w.create_entity("PLACE", Some("ashford")).unwrap();
        assert!(g < e && e < v && v < a);
        let one = |wr: WriteResult| {
            assert_eq!(wr.entries.len(), 1);
            wr.entries[0].0
        };
        let loc = |w: &mut World, assert: bool, tick: i64| {
            let t = Some(Tick(tick));
            if assert {
                one(w.assert(g, "located_in", Value::Ref(v), t).unwrap())
            } else {
                one(w.retract(g, "located_in", Value::Ref(v), t).unwrap())
            }
        };
        let fa = loc(&mut w, true, -300);
        let fr = loc(&mut w, false, -250);
        let fa2 = loc(&mut w, true, -200);
        let fr2 = loc(&mut w, false, -150);
        let fname = one(w
            .assert(g, "name", Value::Str("Greg".into()), Some(Tick(1)))
            .unwrap());
        let fpair = one(w
            .assert(g, "ally_of", Value::Ref(e), Some(Tick(1)))
            .unwrap());
        let fev = one(w.assert(g, "bought", Value::Ref(v), Some(Tick(5))).unwrap());
        ids = (g, e, v, a, fa, fr, fa2, fr2, fname, fpair, fev);
    }
    let (g, e, v, a, fa, fr, fa2, fr2, fname, fpair, fev) = ids;
    let log = read_log(&path).unwrap();
    let attr = |n: &str| {
        log.iter()
            .find_map(|en| match &en.body {
                EntryBody::DefineAttr { id, def } if def.name == n => Some(id.0),
                _ => None,
            })
            .unwrap()
    };
    let max_edge = log
        .iter()
        .filter_map(|en| match &en.body {
            EntryBody::DefineEdge { id, .. } => Some(id.seq()),
            _ => None,
        })
        .max()
        .unwrap();
    let last_tx = log
        .iter()
        .filter_map(|en| match &en.body {
            EntryBody::Commit { tx, .. } => Some(tx.0),
            _ => None,
        })
        .max()
        .unwrap();
    Base {
        bytes: std::fs::read(&path).unwrap(),
        name: attr("name"),
        located: attr("located_in"),
        ally: attr("ally_of"),
        bought: attr("bought"),
        greg: raw(g),
        edwin: raw(e),
        varrow: raw(v),
        ashford: raw(a),
        fa,
        fr,
        fa2,
        fr2,
        fname,
        fpair,
        fev,
        next_edge: max_edge + 1,
        next_entry: log.last().unwrap().id.0 + 1,
        next_tx: last_tx + 1,
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

#[allow(clippy::too_many_arguments)]
fn revised_payload(
    of: u64,
    rev: u32,
    e: u64,
    attr: u32,
    tick: i64,
    retract: bool,
    v: &Value,
) -> Vec<u8> {
    let mut b = of.to_le_bytes().to_vec();
    b.extend_from_slice(&rev.to_le_bytes());
    b.extend_from_slice(&e.to_le_bytes());
    b.extend_from_slice(&attr.to_le_bytes());
    b.extend_from_slice(&tick.to_le_bytes());
    b.push(u8::from(retract));
    b.extend_from_slice(&v.encode().unwrap());
    b
}

fn define_edge(seq: u64, source: u64, attr: u32, target: u64) -> Vec<u8> {
    let mut b = (seq | (1 << 63)).to_le_bytes().to_vec();
    b.extend_from_slice(&source.to_le_bytes());
    b.extend_from_slice(&attr.to_le_bytes());
    b.extend_from_slice(&target.to_le_bytes());
    b
}

type Ent = (u8, Vec<u8>);

struct Log {
    bytes: Vec<u8>,
    next_entry: u64,
    next_tx: u64,
}

impl Log {
    fn new(b: &Base) -> Log {
        Log {
            bytes: b.bytes.clone(),
            next_entry: b.next_entry,
            next_tx: b.next_tx,
        }
    }

    /// Appends one valid txn; returns the entry ids given to `entries`.
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
    fn vv(&self) -> Value {
        r(self.varrow)
    }
    /// Deleted on `Greg located_in Varrow`.
    fn dl(&self, of: u64, rev: u32, tick: i64) -> Ent {
        (
            K_DELETED,
            deleted_payload(of, rev, self.greg, self.located, tick, &self.vv()),
        )
    }
    /// Revised on `Greg located_in <v>`.
    fn rl(&self, of: u64, rev: u32, tick: i64, retract: bool, v: &Value) -> Ent {
        (
            K_REVISED,
            revised_payload(of, rev, self.greg, self.located, tick, retract, v),
        )
    }
    /// The Revised of Varrow (same value).
    fn rlv(&self, of: u64, rev: u32, tick: i64, retract: bool) -> Ent {
        self.rl(of, rev, tick, retract, &self.vv())
    }
    /// A complete valid move of the retract chain `fr` from -250 to `to`.
    fn good_move(&self, to: i64) -> Vec<Ent> {
        vec![self.dl(self.fr, 1, -250), self.rlv(self.fr, 1, to, true)]
    }
    fn greg_name_fact(&self, tick: i64, v: &str) -> Ent {
        (
            K_FACT,
            fact_payload(self.greg, self.name, tick, false, &Value::Str(v.into())),
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

/// `entries` as one txn after `prefix` txns: Corrupt, and the same with
/// `fixed` opens.
fn pair(b: &Base, prefix: &[&[Ent]], bad: &[Ent], fixed: &[Ent], label: &str) -> World {
    let mut l = Log::new(b);
    for p in prefix {
        l.push(p);
    }
    let mut lb = Log {
        bytes: l.bytes.clone(),
        next_entry: l.next_entry,
        next_tx: l.next_tx,
    };
    lb.push(bad);
    expect_corrupt(b, &lb.bytes, label);
    l.push(fixed);
    expect_ok(b, &l.bytes, &format!("{label} (control)"))
}

fn holds(w: &World, e: u64, a: &str, v: &Value, tick: i64) -> bool {
    w.as_of_value(entity(e), a, v, Tick(tick)).unwrap()
}

// ------------------------------------------------------------- positive

#[test]
fn control_a_well_formed_move_opens_and_reads_back() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&b.good_move(-280));
    let w = expect_ok(&b, &l.bytes, "good move");
    let v = b.vv();
    assert!(holds(&w, b.greg, "located_in", &v, -290));
    assert!(!holds(&w, b.greg, "located_in", &v, -280));
    assert!(!holds(&w, b.greg, "located_in", &v, -260));
    assert!(!holds(&w, b.greg, "located_in", &v, -250));
    // canon before the correction: the original walk
    let old = w.at_canon(TxId(b.next_tx - 1));
    assert!(
        old.as_of_value(entity(b.greg), "located_in", &v, Tick(-260))
            .unwrap()
    );
}

#[test]
fn control_a_same_key_op_change_opens() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&[b.rlv(b.fr, 1, -250, false)]);
    let w = expect_ok(&b, &l.bytes, "same key op change");
    assert!(holds(&w, b.greg, "located_in", &b.vv(), -240));
}

// ------------------------------------------- Revised follows its Deleted

#[test]
fn revised_directly_after_the_deleted_of_another_chain_is_corrupt() {
    let b = base();
    pair(
        &b,
        &[],
        // Deleted of chain fa, then a Revised of chain fr
        &[b.dl(b.fa, 1, -300), b.rlv(b.fr, 1, -280, true)],
        &b.good_move(-280),
        "revised after another chain's deleted",
    );
}

#[test]
fn revised_directly_after_a_deleted_with_another_rev_is_corrupt() {
    let b = base();
    pair(
        &b,
        &[],
        &[b.dl(b.fr, 1, -250), b.rlv(b.fr, 2, -280, true)],
        &b.good_move(-280),
        "revised rev differs from the deleted's",
    );
}

#[test]
fn revised_at_the_deleteds_own_key_is_corrupt() {
    let b = base();
    for retract in [false, true] {
        let mut l = Log::new(&b);
        l.push(&[b.dl(b.fr, 1, -250), b.rlv(b.fr, 1, -250, retract)]);
        expect_corrupt(
            &b,
            &l.bytes,
            &format!("revised at deleted key retract={retract}"),
        );
    }
    let mut l = Log::new(&b);
    l.push(&b.good_move(-251));
    expect_ok(&b, &l.bytes, "one tick away");
}

#[test]
fn same_key_revised_with_an_unchanged_op_is_corrupt() {
    let b = base();
    // fr is a retract: a Revised retract at its key changes nothing
    pair(
        &b,
        &[],
        &[b.rlv(b.fr, 1, -250, true)],
        &[b.rlv(b.fr, 1, -250, false)],
        "same key same op",
    );
    // and a revised assert on the assert chain
    pair(
        &b,
        &[],
        &[b.rlv(b.fa, 1, -300, false)],
        &[b.rlv(b.fa, 1, -300, true)],
        "same key same op (assert chain)",
    );
}

#[test]
fn a_second_same_key_revised_must_flip_the_op_again() {
    let b = base();
    pair(
        &b,
        &[&[b.rlv(b.fr, 1, -250, false)]],
        &[b.rlv(b.fr, 2, -250, false)],
        &[b.rlv(b.fr, 2, -250, true)],
        "rev 2 repeats the current (assert) op",
    );
}

#[test]
fn a_revised_whose_rev_skips_one_is_corrupt() {
    let b = base();
    pair(
        &b,
        &[],
        &[b.rlv(b.fr, 2, -250, false)],
        &[b.rlv(b.fr, 1, -250, false)],
        "rev = chain + 2 (first correction)",
    );
    pair(
        &b,
        &[&[b.rlv(b.fr, 1, -250, false)]],
        &[b.rlv(b.fr, 3, -250, true)],
        &[b.rlv(b.fr, 2, -250, true)],
        "rev = chain + 2 (second correction)",
    );
    pair(
        &b,
        &[],
        &[b.dl(b.fr, 2, -250), b.rlv(b.fr, 2, -280, true)],
        &b.good_move(-280),
        "moved pair skipping a rev",
    );
}

#[test]
fn a_revised_with_rev_zero_or_an_old_rev_is_corrupt() {
    let b = base();
    pair(
        &b,
        &[],
        &[b.rlv(b.fr, 0, -250, false)],
        &[b.rlv(b.fr, 1, -250, false)],
        "rev 0",
    );
    pair(
        &b,
        &[&[b.rlv(b.fr, 1, -250, false)]],
        &[b.rlv(b.fr, 1, -250, true)],
        &[b.rlv(b.fr, 2, -250, true)],
        "rev repeated",
    );
}

#[test]
fn deleted_at_a_stale_key_after_a_move_is_corrupt() {
    let b = base();
    pair(
        &b,
        &[&b.good_move(-280)],
        // the chain's current key is now (-280), not (-250)
        &[b.dl(b.fr, 2, -250)],
        &[b.dl(b.fr, 2, -280)],
        "deleted at the pre-revision key",
    );
}

#[test]
fn a_revised_after_a_move_must_start_from_the_current_key() {
    let b = base();
    // second move: Deleted must name the moved key -280
    pair(
        &b,
        &[&b.good_move(-280)],
        &[b.dl(b.fr, 2, -250), b.rlv(b.fr, 2, -270, true)],
        &[b.dl(b.fr, 2, -280), b.rlv(b.fr, 2, -270, true)],
        "second move starts at the stale key",
    );
}

#[test]
fn a_revised_of_a_deleted_chain_not_directly_after_its_deleted_is_corrupt() {
    let b = base();
    // a plain fact between the Deleted and the Revised
    pair(
        &b,
        &[],
        &[
            b.dl(b.fr, 1, -250),
            b.greg_name_fact(10, "Zed"),
            b.rlv(b.fr, 1, -280, true),
        ],
        &b.good_move(-280),
        "fact between deleted and revised",
    );
    // the Revised in a later txn than its Deleted (control: both in one txn)
    let mut l = Log::new(&b);
    l.push(&[b.dl(b.fr, 1, -250)]);
    l.push(&[b.rlv(b.fr, 1, -280, true)]);
    expect_corrupt(&b, &l.bytes, "revised in a later txn than its deleted");
    let mut c = Log::new(&b);
    c.push(&b.good_move(-280));
    expect_ok(&b, &c.bytes, "same txn");
}

#[test]
fn a_revised_with_nothing_before_it_at_another_key_is_corrupt() {
    let b = base();
    pair(
        &b,
        &[],
        &[b.rlv(b.fr, 1, -280, true)],
        &b.good_move(-280),
        "moved revised without a deleted",
    );
}

// ------------------------------------------- landing on an occupied key

#[test]
fn a_moved_revised_onto_a_key_with_a_current_fact_of_either_op_is_corrupt() {
    let b = base();
    // fa2 = assert @-200, fr2 = retract @-150, both current
    for (to, label) in [(-200, "assert-held key"), (-150, "retract-held key")] {
        for retract in [false, true] {
            let mut l = Log::new(&b);
            l.push(&[b.dl(b.fr, 1, -250), b.rlv(b.fr, 1, to, retract)]);
            expect_corrupt(&b, &l.bytes, &format!("{label} retract={retract}"));
        }
    }
    // control: a free key takes either op
    for retract in [false, true] {
        let mut l = Log::new(&b);
        l.push(&[b.dl(b.fr, 1, -250), b.rlv(b.fr, 1, -280, retract)]);
        expect_ok(&b, &l.bytes, &format!("free key retract={retract}"));
    }
}

#[test]
fn a_moved_revised_onto_a_key_that_a_revised_chain_now_occupies_is_corrupt() {
    let b = base();
    // chain fr moves to -280 (Revised retract there); chain fr2 may not land on it
    let first = b.good_move(-280);
    pair(
        &b,
        &[&first],
        &[b.dl(b.fr2, 1, -150), b.rlv(b.fr2, 1, -280, true)],
        &[b.dl(b.fr2, 1, -150), b.rlv(b.fr2, 1, -285, true)],
        "onto a key held by an earlier Revised",
    );
}

#[test]
fn a_moved_revised_onto_a_key_whose_current_entry_is_deleted_opens() {
    let b = base();
    // delete the assert @-200 (its own chain), then move the retract there
    let del_a2 = [b.dl(b.fa2, 1, -200)];
    for retract in [true, false] {
        let mut l = Log::new(&b);
        l.push(&del_a2);
        l.push(&[b.dl(b.fr, 1, -250), b.rlv(b.fr, 1, -200, retract)]);
        let w = expect_ok(
            &b,
            &l.bytes,
            &format!("onto a deleted key retract={retract}"),
        );
        assert_eq!(
            holds(&w, b.greg, "located_in", &b.vv(), -200),
            !retract,
            "retract={retract}"
        );
    }
}

#[test]
fn a_moved_revised_onto_a_key_deleted_in_the_same_txn_is_corrupt() {
    let b = base();
    // one entry per key per txn: the Deleted of fa2's chain and a Revised at that key
    let mut l = Log::new(&b);
    l.push(&[
        b.dl(b.fa2, 1, -200),
        b.dl(b.fr, 1, -250),
        b.rlv(b.fr, 1, -200, true),
    ]);
    expect_corrupt(&b, &l.bytes, "key written twice in a txn");
}

// ------------------------------------------- DefineEdge and Revised

#[test]
fn a_revised_ref_assert_with_no_define_edge_is_corrupt() {
    let b = base();
    let ash = r(b.ashford);
    let bad = [b.dl(b.fa, 1, -300), b.rl(b.fa, 1, -300, false, &ash)];
    let good = [
        (
            K_DEFINE_EDGE,
            define_edge(b.next_edge, b.greg, b.located, b.ashford),
        ),
        b.dl(b.fa, 1, -300),
        b.rl(b.fa, 1, -300, false, &ash),
    ];
    let w = pair(
        &b,
        &[],
        &bad,
        &good,
        "revised assert of an un-interned triple",
    );
    assert!(
        w.edge_id(entity(b.greg), "located_in", entity(b.ashford))
            .is_some()
    );
}

#[test]
fn a_define_edge_satisfied_only_by_a_revised_assert_opens() {
    let b = base();
    let ash = r(b.ashford);
    let mut l = Log::new(&b);
    l.push(&[
        (
            K_DEFINE_EDGE,
            define_edge(b.next_edge, b.greg, b.located, b.ashford),
        ),
        b.dl(b.fa, 1, -300),
        b.rl(b.fa, 1, -300, false, &ash),
    ]);
    let w = expect_ok(&b, &l.bytes, "define edge + revised assert only");
    let edge = w
        .edge_id(entity(b.greg), "located_in", entity(b.ashford))
        .expect("edge interned by the Revised assert");
    assert_eq!(edge.seq(), b.next_edge);
    assert!(holds(&w, b.greg, "located_in", &ash, -300));
    assert!(!holds(&w, b.greg, "located_in", &b.vv(), -300));
}

#[test]
fn a_define_edge_followed_only_by_a_revised_retract_is_corrupt() {
    let b = base();
    let ash = r(b.ashford);
    let bad = [
        (
            K_DEFINE_EDGE,
            define_edge(b.next_edge, b.greg, b.located, b.ashford),
        ),
        b.dl(b.fr, 1, -250),
        b.rl(b.fr, 1, -250 - 1, true, &ash),
    ];
    // control: no DefineEdge for a retract of an un-interned triple
    let good = [b.dl(b.fr, 1, -250), b.rl(b.fr, 1, -251, true, &ash)];
    let w = pair(
        &b,
        &[],
        &bad,
        &good,
        "define edge with only a revised retract",
    );
    assert!(
        w.edge_id(entity(b.greg), "located_in", entity(b.ashford))
            .is_none()
    );
}

#[test]
fn a_define_edge_with_a_revised_assert_after_a_revised_retract_in_one_txn_opens() {
    let b = base();
    let ash = r(b.ashford);
    // fr (retract @-250) -> retract of Ashford @-251, then the retract fr2 -> assert Ashford @-100:
    // the DefineEdge is satisfied by the second chain's Revised assert
    let mut l = Log::new(&b);
    l.push(&[
        (
            K_DEFINE_EDGE,
            define_edge(b.next_edge, b.greg, b.located, b.ashford),
        ),
        b.dl(b.fr, 1, -250),
        b.rl(b.fr, 1, -251, true, &ash),
        b.dl(b.fr2, 1, -150),
        b.rl(b.fr2, 1, -100, false, &ash),
    ]);
    let w = expect_ok(&b, &l.bytes, "assert satisfies the define edge");
    assert!(
        w.edge_id(entity(b.greg), "located_in", entity(b.ashford))
            .is_some()
    );
}

// ------------------------------------------- bidirectional

#[test]
fn a_bidirectional_revised_with_entity_above_value_is_corrupt() {
    let b = base();
    let g = r(b.greg);
    let e = r(b.edwin);
    // same-key op change
    let rev = |entity: u64, v: &Value| {
        (
            K_REVISED,
            revised_payload(b.fpair, 1, entity, b.ally, 1, true, v),
        )
    };
    pair(
        &b,
        &[],
        &[rev(b.edwin, &g)],
        &[rev(b.greg, &e)],
        "bidirectional same-key revised reversed",
    );
    // moved
    let del = (
        K_DELETED,
        deleted_payload(b.fpair, 1, b.greg, b.ally, 1, &e),
    );
    let rev2 = |entity: u64, v: &Value| {
        (
            K_REVISED,
            revised_payload(b.fpair, 1, entity, b.ally, 2, false, v),
        )
    };
    pair(
        &b,
        &[],
        &[del.clone(), rev2(b.edwin, &g)],
        &[del, rev2(b.greg, &e)],
        "bidirectional moved revised reversed",
    );
}

#[test]
fn a_bidirectional_revised_that_renormalises_a_swapped_entity_opens() {
    let b = base();
    // value replaced with an entity below the stored entity cannot happen for greg
    // (the smallest id), so replace the pair by (edwin, varrow): edwin < varrow
    let del = (
        K_DELETED,
        deleted_payload(b.fpair, 1, b.greg, b.ally, 1, &r(b.edwin)),
    );
    let mut l = Log::new(&b);
    l.push(&[
        (
            K_DEFINE_EDGE,
            define_edge(b.next_edge, b.edwin, b.ally, b.varrow),
        ),
        del,
        (
            K_REVISED,
            revised_payload(b.fpair, 1, b.edwin, b.ally, 1, false, &r(b.varrow)),
        ),
    ]);
    let w = expect_ok(&b, &l.bytes, "pair moved to (edwin, varrow)");
    assert!(
        w.as_of(entity(b.varrow), "ally_of", Tick(1))
            .unwrap()
            .contains(&r(b.edwin))
    );
    assert!(
        w.as_of(entity(b.greg), "ally_of", Tick(1))
            .unwrap()
            .is_empty()
    );
}

// ------------------------------------------- validation

#[test]
fn a_revised_of_the_wrong_value_type_is_corrupt() {
    let b = base();
    let name = |v: &Value, retract: bool| {
        (
            K_REVISED,
            revised_payload(b.fname, 1, b.greg, b.name, 1, retract, v),
        )
    };
    pair(
        &b,
        &[],
        &[name(&Value::Bool(true), true)],
        &[name(&Value::Str("Greg".into()), true)],
        "bool value on a string attribute (same key)",
    );
    // moved: Deleted + Revised with an i64 on `name`
    let del = (
        K_DELETED,
        deleted_payload(b.fname, 1, b.greg, b.name, 1, &Value::Str("Greg".into())),
    );
    let mv = |v: &Value| {
        (
            K_REVISED,
            revised_payload(b.fname, 1, b.greg, b.name, 2, false, v),
        )
    };
    pair(
        &b,
        &[],
        &[del.clone(), mv(&Value::I64(7))],
        &[del, mv(&Value::Str("Gregor".into()))],
        "i64 on a string attribute (moved)",
    );
}

#[test]
fn a_revised_ref_to_an_undefined_entity_is_corrupt() {
    let b = base();
    let ghost = r(raw(EntityId::from_bytes((b.ashford + 40).to_be_bytes())));
    pair(
        &b,
        &[],
        &[b.dl(b.fa, 1, -300), b.rl(b.fa, 1, -300, true, &ghost)],
        &[
            b.dl(b.fa, 1, -300),
            b.rl(b.fa, 1, -300, true, &r(b.ashford)),
        ],
        "revised ref to an undefined entity",
    );
}

#[test]
fn a_revised_retract_on_an_event_attribute_is_corrupt() {
    let b = base();
    let ev = |tick: i64, retract: bool| {
        (
            K_REVISED,
            revised_payload(b.fev, 1, b.greg, b.bought, tick, retract, &b.vv()),
        )
    };
    let del = (
        K_DELETED,
        deleted_payload(b.fev, 1, b.greg, b.bought, 5, &b.vv()),
    );
    // moved, retract: corrupt; moved, assert: opens
    pair(
        &b,
        &[],
        &[del.clone(), ev(6, true)],
        &[del, ev(6, false)],
        "moved revised retract on an event",
    );
    // same key: op change to retract is the only possible change, and is corrupt
    let mut l = Log::new(&b);
    l.push(&[ev(5, true)]);
    expect_corrupt(&b, &l.bytes, "same-key revised retract on an event");
}

#[test]
fn a_revised_pointing_at_a_revised_entry_instead_of_the_base_is_corrupt() {
    let b = base();
    let mut l = Log::new(&b);
    let ids = l.push(&[b.rlv(b.fr, 1, -250, false)]);
    let mut bad = Log {
        bytes: l.bytes.clone(),
        next_entry: l.next_entry,
        next_tx: l.next_tx,
    };
    bad.push(&[b.rlv(ids[0], 2, -250, true)]);
    expect_corrupt(&b, &bad.bytes, "of = a Revised entry");
    l.push(&[b.rlv(b.fr, 2, -250, true)]);
    expect_ok(&b, &l.bytes, "of = the base");
}

#[test]
fn a_revised_of_a_non_fact_or_a_forward_entry_is_corrupt() {
    let b = base();
    let genesis_commit = 0u64; // entry ids start at 0 in genesis; any define works
    for (of, label) in [
        (genesis_commit, "of a genesis define"),
        (b.next_entry + 30, "of a forward id"),
        (b.next_entry, "of itself"),
    ] {
        let mut l = Log::new(&b);
        l.push(&[b.rlv(of, 1, -250, false)]);
        expect_corrupt(&b, &l.bytes, label);
    }
}

#[test]
fn a_revised_after_a_delete_only_chain_is_corrupt_in_a_later_txn() {
    let b = base();
    // a delete-only (same-op onto an occupied key) ends the chain; a later Revised is corrupt
    pair(
        &b,
        &[&[b.dl(b.fr, 1, -250)]],
        &[b.rlv(b.fr, 2, -250, false)],
        &[b.greg_name_fact(12, "Zed")],
        "revised of a deleted chain",
    );
}

#[test]
fn replayed_reads_match_at_every_canon_after_two_revisions() {
    let b = base();
    let mut l = Log::new(&b);
    l.push(&b.good_move(-280)); // rev 1
    l.push(&[b.dl(b.fr, 2, -280), b.rlv(b.fr, 2, -270, true)]); // rev 2
    let w = expect_ok(&b, &l.bytes, "two moves");
    let v = b.vv();
    let base_tx = b.next_tx - 1;
    let at = |tx: u64, tick: i64| {
        w.at_canon(TxId(tx))
            .as_of_value(entity(b.greg), "located_in", &v, Tick(tick))
            .unwrap()
    };
    // canon base: retract @-250
    assert!(at(base_tx, -260) && !at(base_tx, -250));
    // rev 1: retract @-280
    assert!(at(base_tx + 1, -290) && !at(base_tx + 1, -280) && !at(base_tx + 1, -260));
    // rev 2: retract @-270
    assert!(at(base_tx + 2, -280) && !at(base_tx + 2, -270) && !at(base_tx + 2, -260));
}
