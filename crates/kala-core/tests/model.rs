//! Item 16: reference model. Random sequences of creates / asserts / retracts
//! / transactions run against a file-backed `World` and against a naive
//! model (a `Vec` of committed facts scanned linearly, plus the CLAUDE.md
//! rules written out again here), compared after every step, at every canon,
//! and again after `World::open` (and after more writes on the reopened
//! world, which proves numbering continues).
//!
//! The model is written from the spec ("Assert / retract, latest wins",
//! "Store-level rules", tick defaulting, "Transactions and commit markers"),
//! not from `world.rs`:
//!
//! - a value holds at T if the latest op with tick <= T (among facts with
//!   tx <= canon) is an assert; ticks decide, never tx;
//! - a write's no-op rule is judged at the write's own tick, against the
//!   committed facts and the transaction's own earlier writes;
//! - the opposite op for one (entity, attr, value) at one tick is a
//!   same-tick conflict; the same op again is a no-op;
//! - retracting an event is an error;
//! - tick = explicit, else transaction default, else the latest tick when the
//!   transaction began (max tick of committed facts, 0 if none);
//! - any write error aborts the transaction: nothing applies, no tx is used;
//! - a commit that stores nothing (all no-ops, no new entity) uses no tx.
//!
//! Slice 2 (item 9) extends the same harness:
//!
//! - ref attributes are directed, directed with an inverse name, or
//!   bidirectional (random spellings; the model stores `(min, max)`);
//! - the model interns an edge for the first *stored* assert of each
//!   (normalised) triple, in write order, and a dropped/aborted txn consumes
//!   none; edge IDs are compared after every check;
//! - edge properties: writes whose subject is an edge ID obtained from
//!   `edge_id` (or a never-defined edge, which must be `UnknownEntity`);
//! - reads on a bidirectional attribute merge both sides (partner as
//!   `Ref`), a self pair counting once;
//! - `neighbors` Out / In / Both (and the inverse name) at every sampled
//!   canon, against the model.
//!
//! Slice 3 (item 10) extends it again:
//!
//! - **Corrections.** `World::correct` (auto-commit) with Delete and Replace
//!   (same-key op flip, moved tick, moved value, bidirectional re-pairing),
//!   plus the error cases (unknown entry, a define entry, a Deleted marker,
//!   an already deleted chain, invalid replacement values, same-tick conflict
//!   at the new key). The model keeps every stored entry (fact, Revised,
//!   Deleted) with its tx and a revision chain per base fact; the *current
//!   entry of a key at canon N* is the entry at that key with the highest
//!   tx <= N, and a DELETED marker empties the whole key. Reads, the no-op /
//!   same-tick judgement of later writes, `latest_tick` and neighbors all
//!   work on those current entries. Entry IDs are learned from the results
//!   and checked through `World::entry` (tx, kind, tick, op, revision).
//! - **Random canons.** Besides a fixed sample, every check also reads at
//!   canons drawn by proptest; at the latest canon both the latest-revision
//!   path and `at_canon_main` are compared with the model.
//! - **Overlaps.** An extra attribute `ruled_by` (cardinality one, reverse
//!   one) joins the schema. The model computes overlaps by scanning each
//!   (entity, attr) view segment by segment between change ticks (no
//!   interval-set code): `World::overlaps` / `View::overlaps` at every
//!   canon, per-write `warnings` (after minus before, over the whole world),
//!   and doctor's `CardinalityOverlap` findings must equal it.
//! - **Doctor.** After every random sequence (and after each reopen) doctor
//!   has no Error findings; its warnings are exactly the model's overlaps
//!   plus `EdgePropertyOnUnassertedEdge` for every current edge-property
//!   assert whose edge triple doesn't hold at that tick (edge properties
//!   are modelled, at random ticks, so these occur; `entry` is the property
//!   chain's current entry id).

use kala_core::log::EntryBody;
use kala_core::*;
use proptest::prelude::*;
use proptest::test_runner::RngSeed;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

// ------------------------------------------------------------ coverage stats

/// Counters bumped by the harness, read by the generator-sanity test so a
/// broken generator can't make the property vacuous.
mod stats {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    pub const NAMES: [&str; 17] = [
        "correct delete",
        "correct op flip (same key)",
        "correct move (delete + revised)",
        "correct move onto same-op key (delete only)",
        "correct no-op",
        "correct bidirectional value move",
        "correct err UnknownEntry",
        "correct err NotAFact",
        "correct err AlreadyDeleted",
        "correct err SameTickConflict",
        "correct err validation",
        "warnings introduced non-empty",
        "warnings resolved non-empty",
        "doctor/overlap reverse side seen",
        "doctor/overlap forward bidirectional seen",
        "doctor edge-property finding seen",
        "read at an older canon with a deleted key",
    ];
    pub const DELETE: usize = 0;
    pub const FLIP: usize = 1;
    pub const MOVE: usize = 2;
    pub const MOVE_ONTO: usize = 3;
    pub const NOOP: usize = 4;
    pub const BIDI_MOVE: usize = 5;
    pub const E_UNKNOWN: usize = 6;
    pub const E_NOT_A_FACT: usize = 7;
    pub const E_ALREADY: usize = 8;
    pub const E_SAME_TICK: usize = 9;
    pub const E_VALID: usize = 10;
    pub const W_INTRO: usize = 11;
    pub const W_RESOLVED: usize = 12;
    pub const REVERSE: usize = 13;
    pub const BIDI_OV: usize = 14;
    pub const EDGE_FINDING: usize = 15;
    pub const OLD_CANON_DELETED: usize = 16;
    static COUNTS: [AtomicUsize; 17] = [const { AtomicUsize::new(0) }; 17];
    pub fn bump(i: usize) {
        COUNTS[i].fetch_add(1, Relaxed);
    }
    pub fn get(i: usize) -> usize {
        COUNTS[i].load(Relaxed)
    }
}

// ---------------------------------------------------------------- schema

const TYPES: [&str; 3] = ["CHARACTER", "PLACE", "ITEM"];

struct A {
    name: &'static str,
    ty: ValueType,
    event: bool,
    indexed: bool,
    many: bool,
    bidi: bool,
    inverse: Option<&'static str>,
    rev_one: bool,
}

const ATTRS: [A; 13] = [
    A {
        name: "loc",
        ty: ValueType::Ref,
        event: false,
        indexed: true,
        many: false,
        bidi: false,
        inverse: None,
        rev_one: false,
    },
    A {
        name: "allies",
        ty: ValueType::Ref,
        event: false,
        indexed: true,
        many: true,
        bidi: false,
        inverse: None,
        rev_one: false,
    },
    A {
        name: "name",
        ty: ValueType::Str,
        event: false,
        indexed: true,
        many: false,
        bidi: false,
        inverse: None,
        rev_one: false,
    },
    A {
        name: "age",
        ty: ValueType::I64,
        event: false,
        indexed: true,
        many: true,
        bidi: false,
        inverse: None,
        rev_one: false,
    },
    A {
        name: "alive",
        ty: ValueType::Bool,
        event: false,
        indexed: true,
        many: false,
        bidi: false,
        inverse: None,
        rev_one: false,
    },
    A {
        name: "height",
        ty: ValueType::F64,
        event: false,
        indexed: true,
        many: false,
        bidi: false,
        inverse: None,
        rev_one: false,
    },
    A {
        name: "bought",
        ty: ValueType::Ref,
        event: true,
        indexed: true,
        many: true,
        bidi: false,
        inverse: None,
        rev_one: false,
    },
    A {
        name: "year",
        ty: ValueType::I64,
        event: true,
        indexed: true,
        many: false,
        bidi: false,
        inverse: None,
        rev_one: false,
    },
    A {
        name: "bio",
        ty: ValueType::Str,
        event: false,
        indexed: false,
        many: false,
        bidi: false,
        inverse: None,
        rev_one: false,
    },
    A {
        name: "friend_of",
        ty: ValueType::Ref,
        event: false,
        indexed: true,
        many: true,
        bidi: true,
        inverse: None,
        rev_one: false,
    },
    A {
        name: "parent_of",
        ty: ValueType::Ref,
        event: false,
        indexed: true,
        many: true,
        bidi: false,
        inverse: Some("child_of"),
        rev_one: false,
    },
    A {
        name: "spouse",
        ty: ValueType::Ref,
        event: false,
        indexed: true,
        many: false,
        bidi: true,
        inverse: None,
        rev_one: false,
    },
    A {
        name: "ruled_by",
        ty: ValueType::Ref,
        event: false,
        indexed: true,
        many: false,
        bidi: false,
        inverse: None,
        rev_one: true,
    },
];
const A_LOC: usize = 0;
const A_NAME: usize = 2;
const A_HEIGHT: usize = 5;
const A_PARENT_OF: usize = 10;
/// Attributes an edge can carry as properties in this model (non-ref only,
/// so a property write never interns an edge of its own).
const EDGE_PROPS: [usize; 6] = [2, 3, 4, 5, 7, 8];
/// Pseudo node-type index for edges in the model's `Ent`.
const EDGE_T: usize = 3;

fn schema() -> Schema {
    let mut s = Schema::new();
    for t in TYPES {
        s = s.node_type(t);
    }
    for a in &ATTRS {
        let mut d = AttrDef::new(
            a.name,
            a.ty,
            if a.event { Kind::Event } else { Kind::State },
            if a.many {
                Cardinality::Many
            } else {
                Cardinality::One
            },
        );
        if !a.indexed {
            d = d.unindexed();
        }
        if a.bidi {
            d = d.bidirectional();
        }
        if let Some(inv) = a.inverse {
            d = d.inverse(inv);
        }
        if a.rev_one {
            d = d.reverse_one();
        }
        s = s.attribute(d);
    }
    s
}

// ----------------------------------------------------------------- model

type Ent = (usize, u64); // (node type index, sequence number)

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum MV {
    Bool(bool),
    I64(i64),
    F64(u64), // bits, -0.0 already normalized to 0.0
    Str(String),
    Ref(Ent),
}

fn mv_type(v: &MV) -> ValueType {
    match v {
        MV::Bool(_) => ValueType::Bool,
        MV::I64(_) => ValueType::I64,
        MV::F64(_) => ValueType::F64,
        MV::Str(_) => ValueType::Str,
        MV::Ref(_) => ValueType::Ref,
    }
}

/// Natural order of same-typed values (what the read order must follow).
fn cmp_mv(a: &MV, b: &MV) -> Ordering {
    match (a, b) {
        (MV::Bool(x), MV::Bool(y)) => x.cmp(y),
        (MV::I64(x), MV::I64(y)) => x.cmp(y),
        (MV::F64(x), MV::F64(y)) => f64::from_bits(*x).total_cmp(&f64::from_bits(*y)),
        (MV::Str(x), MV::Str(y)) => x.as_bytes().cmp(y.as_bytes()),
        (MV::Ref(x), MV::Ref(y)) => x.cmp(y),
        _ => panic!("compared values of different types"),
    }
}

fn from_value(v: &Value) -> MV {
    match v {
        Value::Bool(b) => MV::Bool(*b),
        Value::I64(i) => MV::I64(*i),
        Value::F64(f) => {
            assert!(!f.is_nan());
            assert!(
                f.to_bits() != (-0.0f64).to_bits(),
                "-0.0 must be normalized to 0.0 on write"
            );
            MV::F64(f.to_bits())
        }
        Value::Str(s) => MV::Str(s.clone()),
        Value::Ref(id) => MV::Ref((usize::from(id.node_type().0), id.seq())),
    }
}

/// Bidirectional pairs are stored as (min, max); directed ones as written.
fn norm(a: usize, x: Ent, y: Ent) -> (Ent, Ent) {
    if ATTRS[a].bidi && y < x {
        (y, x)
    } else {
        (x, y)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FK {
    /// a base `Fact` entry
    Base,
    /// a `Revised` entry
    Revised,
    /// a `DELETED` marker (`assert` is meaningless)
    Deleted,
}

/// One stored log entry at a key `(e, a, v, tick)`.
#[derive(Clone, Debug)]
struct MFact {
    e: Ent,
    a: usize,
    v: MV,
    tick: i64,
    tx: u64,
    assert: bool,
    kind: FK,
    /// index into `Model::chains`
    chain: usize,
    /// revision number (0 for a base fact)
    rev: u32,
    /// the entry ID the real world gave it
    real: Option<EntryId>,
}

/// A revision chain: a base fact and its corrections.
#[derive(Clone, Debug)]
struct MChain {
    /// base + Revised entry IDs, in log order
    ids: Vec<EntryId>,
    /// Deleted marker IDs, in log order
    dels: Vec<EntryId>,
    /// index into `Model::facts` of the current revision
    cur: usize,
    rev: u32,
    deleted: bool,
}

/// What the model expects a write error to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EK {
    UnknownAttribute,
    UnknownEntity,
    WrongValueType,
    Value,
    RetractEvent,
    SameTickConflict,
    Aborted,
    DuplicateHandle,
    InvalidHandle,
    UnknownNodeType,
    UnknownEntry,
    NotAFact,
    AlreadyDeleted,
}

fn ek(e: &Error) -> EK {
    match e {
        Error::UnknownAttribute(_) => EK::UnknownAttribute,
        Error::UnknownEntity(_) => EK::UnknownEntity,
        Error::WrongValueType { .. } => EK::WrongValueType,
        Error::Value(_) => EK::Value,
        Error::RetractEvent(_) => EK::RetractEvent,
        Error::SameTickConflict { .. } => EK::SameTickConflict,
        Error::TransactionAborted => EK::Aborted,
        Error::DuplicateHandle(_) => EK::DuplicateHandle,
        Error::InvalidHandle(_) => EK::InvalidHandle,
        Error::UnknownNodeType(_) => EK::UnknownNodeType,
        Error::UnknownEntry(_) => EK::UnknownEntry,
        Error::NotAFact(_) => EK::NotAFact,
        Error::AlreadyDeleted(_) => EK::AlreadyDeleted,
        other => panic!("unexpected error {other:?}"),
    }
}

/// The value of a model write: a real value, or one of the invalid inputs.
#[derive(Clone, Debug)]
enum Val {
    Mv(MV),
    NaN,
    UnknownRef,
}

#[derive(Clone, Debug)]
struct MWrite {
    assert: bool,
    subj: Option<Ent>,
    attr: Option<usize>,
    val: Val,
    tick: Option<i64>,
}

#[derive(Clone, Debug)]
struct MCreate {
    ty: Option<usize>,
    handle: Option<String>,
}

#[derive(Default)]
struct Model {
    counters: [u64; 3],
    ents: Vec<(Ent, u64)>, // entity and the tx that created it
    handles: HashSet<String>,
    facts: Vec<MFact>,
    /// Interned edges in seq order (seq = index + 1), triple normalised for
    /// bidirectional attributes, with the tx that interned it.
    edges: Vec<(Ent, usize, Ent, u64)>,
    tx: u64,
    chains: Vec<MChain>,
    /// `live(c)` results; cleared whenever `facts` changes
    cache: RefCell<HashMap<u64, Rc<Vec<MFact>>>>,
}

impl Model {
    fn edge_seq(&self, src: Ent, a: usize, tgt: Ent) -> Option<u64> {
        let (src, tgt) = norm(a, src, tgt);
        self.edges
            .iter()
            .position(|(s, x, t, _)| (*s, *x, *t) == (src, a, tgt))
            .map(|i| i as u64 + 1)
    }

    /// The current entries at canon `c`: per key the entry with the highest
    /// tx <= c, except that a key whose current entry is a DELETED marker
    /// holds nothing at all.
    fn live(&self, c: u64) -> Rc<Vec<MFact>> {
        if let Some(v) = self.cache.borrow().get(&c) {
            return v.clone();
        }
        let mut best: HashMap<(Ent, usize, MV, i64), usize> = HashMap::new();
        for (i, f) in self.facts.iter().enumerate() {
            if f.tx > c {
                continue;
            }
            let k = (f.e, f.a, f.v.clone(), f.tick);
            match best.get(&k) {
                Some(&j) if self.facts[j].tx >= f.tx => {}
                _ => {
                    best.insert(k, i);
                }
            }
        }
        let mut idx: Vec<usize> = best.into_values().collect();
        idx.sort_unstable();
        let out: Vec<MFact> = idx
            .into_iter()
            .map(|i| self.facts[i].clone())
            .filter(|f| f.kind != FK::Deleted)
            .collect();
        let out = Rc::new(out);
        self.cache.borrow_mut().insert(c, out.clone());
        out
    }

    /// The highest tick of any current fact at the latest canon (0 if none).
    fn latest_tick(&self) -> i64 {
        self.live(self.tx).iter().map(|f| f.tick).max().unwrap_or(0)
    }
}

/// A transaction as the model sees it.
struct MTxn {
    base_tick: i64,
    default: Option<i64>,
    staged: Vec<MFact>,
    staged_edges: Vec<(Ent, usize, Ent)>,
    staged_ents: Vec<Ent>,
    staged_handles: Vec<String>,
    poisoned: bool,
}

impl MTxn {
    fn begin(m: &Model, default: Option<i64>) -> MTxn {
        MTxn {
            base_tick: m.latest_tick(),
            default,
            staged: vec![],
            staged_edges: vec![],
            staged_ents: vec![],
            staged_handles: vec![],
            poisoned: false,
        }
    }

    fn defined(&self, m: &Model, e: Ent) -> bool {
        if e.0 == EDGE_T {
            return e.1 >= 1 && e.1 <= (m.edges.len() + self.staged_edges.len()) as u64;
        }
        m.ents.iter().any(|(x, _)| *x == e) || self.staged_ents.contains(&e)
    }

    /// Edges visible to this txn, in seq order.
    fn visible_edges(&self, m: &Model) -> Vec<(Ent, usize, Ent)> {
        m.edges
            .iter()
            .map(|(s, a, t, _)| (*s, *a, *t))
            .chain(self.staged_edges.iter().copied())
            .collect()
    }

    fn create(&mut self, m: &Model, c: &MCreate) -> Result<Ent, EK> {
        if self.poisoned {
            return Err(EK::Aborted);
        }
        let r = (|| {
            let ty = c.ty.ok_or(EK::UnknownNodeType)?;
            if let Some(h) = &c.handle {
                if h.is_empty() {
                    return Err(EK::InvalidHandle);
                }
                if m.handles.contains(h) || self.staged_handles.contains(h) {
                    return Err(EK::DuplicateHandle);
                }
            }
            let staged_of_ty = self.staged_ents.iter().filter(|e| e.0 == ty).count() as u64;
            let ent = (ty, m.counters[ty] + staged_of_ty + 1);
            self.staged_ents.push(ent);
            if let Some(h) = &c.handle {
                self.staged_handles.push(h.clone());
            }
            Ok(ent)
        })();
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }

    /// `Ok((changed, tick))` or the expected error.
    fn write(&mut self, m: &Model, w: &MWrite) -> Result<(usize, i64), EK> {
        if self.poisoned {
            return Err(EK::Aborted);
        }
        let r = self.write_inner(m, w);
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }

    fn write_inner(&mut self, m: &Model, w: &MWrite) -> Result<(usize, i64), EK> {
        let a = w.attr.ok_or(EK::UnknownAttribute)?;
        let subj = w
            .subj
            .filter(|e| self.defined(m, *e))
            .ok_or(EK::UnknownEntity)?;
        let mv = match &w.val {
            Val::NaN => return Err(EK::Value),
            Val::UnknownRef => return Err(EK::UnknownEntity),
            Val::Mv(v) => v,
        };
        if mv_type(mv) != ATTRS[a].ty {
            return Err(EK::WrongValueType);
        }
        if let MV::Ref(t) = mv
            && !self.defined(m, *t)
        {
            return Err(EK::UnknownEntity);
        }
        if let MV::Str(s) = mv
            && ATTRS[a].indexed
            && s.len() > 256
        {
            return Err(EK::Value);
        }
        if !w.assert && ATTRS[a].event {
            return Err(EK::RetractEvent);
        }
        let tick = w.tick.or(self.default).unwrap_or(self.base_tick);
        let (subj, mv) = match mv {
            MV::Ref(t) if subj.0 != EDGE_T => {
                let (x, y) = norm(a, subj, *t);
                (x, MV::Ref(y))
            }
            _ => (subj, mv.clone()),
        };
        let mv = &mv;
        let live = m.live(m.tx);
        let same: Vec<&MFact> = live
            .iter()
            .chain(self.staged.iter())
            .filter(|f| f.e == subj && f.a == a && f.v == *mv)
            .collect();
        if let Some(f) = same.iter().find(|f| f.tick == tick) {
            return if f.assert == w.assert {
                Ok((0, tick))
            } else {
                Err(EK::SameTickConflict)
            };
        }
        let holds = same
            .iter()
            .filter(|f| f.tick <= tick)
            .max_by_key(|f| f.tick)
            .is_some_and(|f| f.assert);
        if w.assert == holds {
            return Ok((0, tick));
        }
        if let MV::Ref(t) = mv
            && w.assert
            && subj.0 != EDGE_T
        {
            let known =
                m.edge_seq(subj, a, *t).is_some() || self.staged_edges.contains(&(subj, a, *t));
            if !known {
                self.staged_edges.push((subj, a, *t));
            }
        }
        self.staged.push(MFact {
            e: subj,
            a,
            v: mv.clone(),
            tick,
            tx: 0,
            assert: w.assert,
            kind: FK::Base,
            chain: usize::MAX,
            rev: 0,
            real: None,
        });
        Ok((1, tick))
    }

    /// Commit into the model; `Err(Aborted)` if poisoned.
    fn commit(self, m: &mut Model, ids: &[EntryId]) -> Result<(u64, usize), EK> {
        if self.poisoned {
            return Err(EK::Aborted);
        }
        let changed = self.staged.len();
        assert_eq!(ids.len(), changed, "entries accounting of the commit");
        if changed == 0 && self.staged_ents.is_empty() {
            return Ok((m.tx, 0));
        }
        m.tx += 1;
        for e in &self.staged_ents {
            m.counters[e.0] += 1;
            m.ents.push((*e, m.tx));
        }
        m.handles.extend(self.staged_handles);
        for (s, a, t) in self.staged_edges {
            m.edges.push((s, a, t, m.tx));
        }
        for (mut f, id) in self.staged.into_iter().zip(ids) {
            f.tx = m.tx;
            f.real = Some(*id);
            f.chain = m.chains.len();
            m.chains.push(MChain {
                ids: vec![*id],
                dels: vec![],
                cur: m.facts.len(),
                rev: 0,
                deleted: false,
            });
            m.facts.push(f);
        }
        m.cache.borrow_mut().clear();
        Ok((m.tx, changed))
    }
}

/// The facts a read of `(e, a)` at canon `c` is built from. For a
/// bidirectional attribute that is both stored sides, re-keyed so the entity
/// is the subject and the partner is the `Ref` value (a self pair once).
fn m_view(m: &Model, c: u64, e: Ent, a: usize) -> Vec<MFact> {
    let mut out = vec![];
    let live = m.live(c);
    for f in live.iter().filter(|f| f.a == a) {
        if f.e == e {
            out.push(f.clone());
        } else if ATTRS[a].bidi && f.v == MV::Ref(e) {
            let mut g = f.clone();
            g.e = e;
            g.v = MV::Ref(f.e);
            out.push(g);
        }
    }
    out
}

/// Model answers (canon `c`).
fn m_holds(m: &Model, c: u64, e: Ent, a: usize, v: &MV, t: i64) -> bool {
    m_view(m, c, e, a)
        .iter()
        .filter(|f| f.v == *v && f.tick <= t)
        .max_by_key(|f| f.tick)
        .is_some_and(|f| f.assert)
}

fn m_as_of(m: &Model, c: u64, e: Ent, a: usize, t: i64) -> Vec<MV> {
    let mut vals: Vec<MV> = vec![];
    for f in m_view(m, c, e, a) {
        if !vals.contains(&f.v) {
            vals.push(f.v.clone());
        }
    }
    let mut out: Vec<MV> = vals
        .into_iter()
        .filter(|v| m_holds(m, c, e, a, v, t))
        .collect();
    out.sort_by(cmp_mv);
    out
}

fn m_history(m: &Model, c: u64, e: Ent, a: usize) -> Vec<(i64, bool, MV)> {
    let mut fs = m_view(m, c, e, a);
    fs.sort_by(|x, y| x.tick.cmp(&y.tick).then_with(|| cmp_mv(&x.v, &y.v)));
    fs.into_iter()
        .map(|f| (f.tick, f.assert, f.v.clone()))
        .collect()
}

/// `neighbors(x, a, dir, t)` at canon `c`: (neighbor, edge seq), sorted.
/// Directed: Out = x is the stored source, In = x is the stored target.
/// Bidirectional: every direction reads both sides.
fn m_neighbors(m: &Model, c: u64, x: Ent, a: usize, dir: Direction, t: i64) -> Vec<(Ent, u64)> {
    let want_out = matches!(dir, Direction::Out | Direction::Both) || ATTRS[a].bidi;
    let want_in = matches!(dir, Direction::In | Direction::Both) || ATTRS[a].bidi;
    let mut keys: Vec<(Ent, Ent)> = vec![];
    let live = m.live(c);
    for f in live.iter().filter(|f| f.a == a) {
        let MV::Ref(tgt) = f.v else { continue };
        if (want_out && f.e == x) || (want_in && tgt == x) {
            keys.push((f.e, tgt));
        }
    }
    keys.sort();
    keys.dedup();
    let mut out = std::collections::BTreeSet::new();
    for (src, tgt) in keys {
        let held = live
            .iter()
            .filter(|f| f.a == a && f.e == src && f.v == MV::Ref(tgt) && f.tick <= t)
            .max_by_key(|f| f.tick)
            .is_some_and(|f| f.assert);
        if !held {
            continue;
        }
        let edge = m.edge_seq(src, a, tgt).expect("a stored fact has an edge");
        if want_out && src == x {
            out.insert((tgt, edge));
        }
        if want_in && tgt == x {
            out.insert((src, edge));
        }
    }
    out.into_iter().collect()
}

/// What a correction will store, decided by the model before the real call.
#[derive(Debug)]
struct Plan {
    changed: usize,
    tick: i64,
    /// a Deleted marker and/or a Revised fact, in log order
    entries: Vec<MFact>,
    /// a new ref triple whose edge the correction interns
    edge: Option<(Ent, usize, Ent)>,
}

impl Model {
    /// Stores a plan's entries (with the IDs the real world assigned) as one
    /// new transaction.
    fn apply(&mut self, ci: usize, plan: Plan, ids: &[EntryId]) {
        assert_eq!(ids.len(), plan.entries.len());
        self.tx += 1;
        if let Some((s, a, t)) = plan.edge {
            self.edges.push((s, a, t, self.tx));
        }
        self.chains[ci].rev += 1;
        for (mut f, id) in plan.entries.into_iter().zip(ids) {
            f.tx = self.tx;
            f.real = Some(*id);
            let idx = self.facts.len();
            match f.kind {
                FK::Deleted => {
                    self.chains[ci].dels.push(*id);
                    self.chains[ci].deleted = true;
                }
                FK::Revised => {
                    self.chains[ci].ids.push(*id);
                    self.chains[ci].cur = idx;
                    self.chains[ci].deleted = false;
                }
                FK::Base => unreachable!(),
            }
            self.facts.push(f);
        }
        self.cache.borrow_mut().clear();
    }
}

// ------------------------------------------------- overlaps (naive, per tick)

/// One overlap as the model sees it: a pair of values of a cardinality-one
/// side holding together over `[from, until)`.
#[derive(Clone, Debug, PartialEq)]
struct MOv {
    attr: usize,
    ent: Ent,
    reverse: bool,
    a: MV,
    b: MV,
    from: i64,
    until: Option<i64>,
}

/// `view` holds facts all on one subject: does `v` hold at tick `t`
/// (latest op with tick <= t is an assert)?
fn holds_in(view: &[MFact], v: &MV, t: i64) -> bool {
    view.iter()
        .filter(|f| f.v == *v && f.tick <= t)
        .max_by_key(|f| f.tick)
        .is_some_and(|f| f.assert)
}

/// Overlaps of one subject's view: for every pair of values, walk the
/// segments between consecutive change ticks of the two values and join the
/// consecutive segments where both hold.
fn view_overlaps(view: &[MFact], attr: usize, ent: Ent, reverse: bool, out: &mut Vec<MOv>) {
    let mut vals: Vec<MV> = vec![];
    for f in view {
        if !vals.contains(&f.v) {
            vals.push(f.v.clone());
        }
    }
    vals.sort_by(cmp_mv);
    for (i, x) in vals.iter().enumerate() {
        for y in &vals[i + 1..] {
            let mut ticks: Vec<i64> = view
                .iter()
                .filter(|f| f.v == *x || f.v == *y)
                .map(|f| f.tick)
                .collect();
            ticks.sort_unstable();
            ticks.dedup();
            // segment k is [ticks[k], ticks[k+1]) (the last one is unbounded)
            let mut run: Option<i64> = None;
            for &t0 in &ticks {
                let both = holds_in(view, x, t0) && holds_in(view, y, t0);
                match (both, run) {
                    (true, None) => run = Some(t0),
                    (false, Some(from)) => {
                        out.push(MOv {
                            attr,
                            ent,
                            reverse,
                            a: x.clone(),
                            b: y.clone(),
                            from,
                            until: Some(t0),
                        });
                        run = None;
                    }
                    _ => {}
                }
            }
            if let Some(from) = run {
                out.push(MOv {
                    attr,
                    ent,
                    reverse,
                    a: x.clone(),
                    b: y.clone(),
                    from,
                    until: None,
                });
            }
        }
    }
}

fn mov_cmp(x: &MOv, y: &MOv) -> Ordering {
    x.attr
        .cmp(&y.attr)
        .then(x.ent.cmp(&y.ent))
        .then(x.reverse.cmp(&y.reverse))
        .then_with(|| cmp_mv(&x.a, &y.a))
        .then_with(|| cmp_mv(&x.b, &y.b))
        .then(x.from.cmp(&y.from))
}

/// Every overlap of attribute `a` at canon `c`, in the documented order
/// (entity, side, a, b, from).
fn m_overlaps(m: &Model, c: u64, a: usize) -> Vec<MOv> {
    let at = &ATTRS[a];
    let mut out = vec![];
    if at.event {
        return out;
    }
    let live = m.live(c);
    let facts: Vec<&MFact> = live.iter().filter(|f| f.a == a).collect();
    if !at.many {
        let mut subjects: Vec<Ent> = facts.iter().map(|f| f.e).collect();
        if at.bidi {
            subjects.extend(facts.iter().filter_map(|f| match f.v {
                MV::Ref(t) => Some(t),
                _ => None,
            }));
        }
        subjects.sort();
        subjects.dedup();
        for e in subjects {
            let view = m_view(m, c, e, a);
            view_overlaps(&view, a, e, false, &mut out);
        }
    }
    if at.rev_one {
        let mut targets: Vec<Ent> = facts
            .iter()
            .filter_map(|f| match f.v {
                MV::Ref(t) => Some(t),
                _ => None,
            })
            .collect();
        targets.sort();
        targets.dedup();
        for t in targets {
            let view: Vec<MFact> = facts
                .iter()
                .filter(|f| f.v == MV::Ref(t))
                .map(|f| {
                    let mut g = (*f).clone();
                    g.e = t;
                    g.v = MV::Ref(f.e);
                    g
                })
                .collect();
            view_overlaps(&view, a, t, true, &mut out);
        }
    }
    out.sort_by(mov_cmp);
    out
}

/// Every overlap of every attribute, attribute by attribute.
fn m_overlaps_all(m: &Model, c: u64) -> Vec<MOv> {
    (0..ATTRS.len()).flat_map(|a| m_overlaps(m, c, a)).collect()
}

/// `(entry, edge seq, attr name, tick)` of every current edge-property
/// assert (at canon `c`) whose edge's triple doesn't hold at its tick.
fn m_edge_findings(m: &Model, c: u64) -> Vec<(u64, u64, String, i64)> {
    let live = m.live(c);
    let mut out = vec![];
    for f in live.iter().filter(|f| f.e.0 == EDGE_T && f.assert) {
        let (src, a, tgt, _) = m.edges[f.e.1 as usize - 1];
        let holds = live
            .iter()
            .filter(|g| g.e == src && g.a == a && g.v == MV::Ref(tgt) && g.tick <= f.tick)
            .max_by_key(|g| g.tick)
            .is_some_and(|g| g.assert);
        if !holds {
            out.push((
                f.real.expect("committed").0,
                f.e.1,
                ATTRS[f.a].name.to_string(),
                f.tick,
            ));
        }
    }
    out.sort();
    out
}

// ------------------------------------------------------------ test specs

#[derive(Clone, Debug)]
enum OpSpec {
    Create {
        ty: usize,
        handle: u8,
    },
    Write {
        assert: bool,
        ent: usize,
        attr: usize,
        val: usize,
        tick: Option<i64>,
        defect: u8,
    },
    /// A property write on an edge (`edge` picks among the edges visible
    /// to the transaction; `bogus` uses a never-defined edge ID instead).
    EdgeWrite {
        assert: bool,
        edge: usize,
        attr: usize,
        val: usize,
        tick: Option<i64>,
        bogus: bool,
    },
    /// `World::correct` on an entry of the chain `pick % chains` (only in
    /// `Mode::Correct`; elsewhere skipped on both sides).
    Correct {
        pick: usize,
        which: usize,
        delete: bool,
        value: Option<usize>,
        tick: Option<i64>,
        op: Option<bool>,
        defect: u8,
        /// aim the replacement at the tick of another fact of the same
        /// (entity, attr, value), to land on an occupied key
        collide: bool,
    },
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    Commit,
    Drop,
    Auto,
    /// one auto-committed `World::correct` (`ops[0]` is an `OpSpec::Correct`)
    Correct,
}

#[derive(Clone, Debug)]
struct TxnSpec {
    mode: Mode,
    default: Option<i64>,
    ops: Vec<OpSpec>,
    check: bool,
}

fn tick_strategy() -> impl Strategy<Value = Option<i64>> {
    prop_oneof![
        25 => Just(None),
        60 => (-3i64..=3).prop_map(Some),
        3 => Just(Some(i64::MIN)),
        2 => Just(Some(i64::MAX)),
        5 => prop_oneof![Just(-250i64), Just(600i64), Just(-208i64)].prop_map(Some),
    ]
}

fn op_strategy() -> impl Strategy<Value = OpSpec> {
    prop_oneof![
        1 => (0usize..4, 0u8..6).prop_map(|(ty, handle)| OpSpec::Create { ty, handle }),
        9 => (
            any::<bool>(),
            0usize..64,
            0usize..ATTRS.len(),
            prop_oneof![30 => 0usize..3, 3 => 3usize..5],
            tick_strategy(),
            prop_oneof![90 => Just(0u8), 10 => 1u8..=6],
        )
            .prop_map(|(assert, ent, attr, val, tick, defect)| OpSpec::Write {
                assert, ent, attr, val, tick, defect
            }),
        4 => (
            any::<bool>(),
            0usize..64,
            0usize..EDGE_PROPS.len(),
            0usize..5,
            tick_strategy(),
            prop_oneof![92 => Just(false), 8 => Just(true)],
        )
            .prop_map(|(assert, edge, attr, val, tick, bogus)| OpSpec::EdgeWrite {
                assert, edge, attr, val, tick, bogus
            }),
    ]
}

fn correct_strategy() -> impl Strategy<Value = TxnSpec> {
    (
        0usize..1000,
        0usize..4,
        prop_oneof![25 => Just(true), 75 => Just(false)],
        prop_oneof![50 => Just(None), 50 => (0usize..64).prop_map(Some)],
        tick_strategy(),
        prop_oneof![65 => Just(None), 35 => any::<bool>().prop_map(Some)],
        prop_oneof![80 => Just(0u8), 20 => 1u8..=7],
        prop_oneof![75 => Just(false), 25 => Just(true)],
        prop_oneof![80 => Just(false), 20 => Just(true)],
    )
        .prop_map(
            |(pick, which, delete, value, tick, op, defect, collide, check)| TxnSpec {
                mode: Mode::Correct,
                default: None,
                ops: vec![OpSpec::Correct {
                    pick,
                    which,
                    delete,
                    value,
                    tick,
                    op,
                    defect,
                    collide,
                }],
                check,
            },
        )
}

fn txn_strategy() -> impl Strategy<Value = TxnSpec> {
    prop_oneof![
        65 => plain_txn_strategy(),
        35 => correct_strategy(),
    ]
}

fn plain_txn_strategy() -> impl Strategy<Value = TxnSpec> {
    (
        prop_oneof![60 => Just(Mode::Commit), 15 => Just(Mode::Drop), 25 => Just(Mode::Auto)],
        prop_oneof![70 => Just(None), 30 => tick_strategy()],
        prop::collection::vec(op_strategy(), 1..=5),
        prop_oneof![85 => Just(false), 15 => Just(true)],
    )
        .prop_map(|(mode, default, ops, check)| TxnSpec {
            mode,
            default,
            ops,
            check,
        })
}

// --------------------------------------------------------------- harness

#[allow(clippy::large_enum_variant)]
enum Target<'a> {
    T(Txn<'a>),
    W(&'a mut World),
}

impl Target<'_> {
    fn create(&mut self, ty: &str, h: Option<&str>) -> Result<EntityId, Error> {
        match self {
            Target::T(t) => t.create_entity(ty, h),
            Target::W(w) => w.create_entity(ty, h),
        }
    }

    fn edge_id(&self, s: EntityId, a: &str, t: EntityId) -> Option<EdgeId> {
        match self {
            Target::T(x) => x.edge_id(s, a, t),
            Target::W(x) => x.edge_id(s, a, t),
        }
    }

    fn write(
        &mut self,
        assert: bool,
        e: EntityId,
        a: &str,
        v: Value,
        t: Option<Tick>,
    ) -> Result<WriteResult, Error> {
        match (self, assert) {
            (Target::T(x), true) => x.assert(e, a, v, t),
            (Target::T(x), false) => x.retract(e, a, v, t),
            (Target::W(x), true) => x.assert(e, a, v, t),
            (Target::W(x), false) => x.retract(e, a, v, t),
        }
    }
}

struct Sys {
    dir: tempfile::TempDir,
    world: Option<World>,
    m: Model,
    real: HashMap<Ent, EntityId>,
    /// the model's overlaps at its latest canon (kept to diff per write)
    ov: Vec<MOv>,
    /// canons drawn by proptest, read in addition to the fixed sample
    extra_canons: Vec<u64>,
}

/// A non-ref value for an edge property, from a small pool.
fn edge_prop_value(ty: ValueType, val: usize) -> (MV, Value) {
    match ty {
        ValueType::Bool => (MV::Bool(val % 2 == 1), Value::Bool(val % 2 == 1)),
        ValueType::I64 => {
            let i = [0, -1, 1, i64::MIN, i64::MAX][val % 5];
            (MV::I64(i), Value::I64(i))
        }
        ValueType::F64 => {
            let x = [0.0f64, 0.0, 1.5, -2.5, f64::INFINITY][val % 5];
            let real = if val == 1 { -0.0 } else { x };
            (MV::F64(x.to_bits()), Value::F64(real))
        }
        ValueType::Str => {
            let s = match val % 5 {
                0 => String::new(),
                1 => "a".into(),
                2 => "Edwin é".into(),
                3 => "x".repeat(256),
                _ => "y".repeat(257),
            };
            (MV::Str(s.clone()), Value::Str(s))
        }
        ValueType::Ref => unreachable!("edge properties are non-ref in this model"),
    }
}

fn bogus_entity() -> EntityId {
    EntityId::new(NodeTypeId(0), 999_999).unwrap()
}

impl Sys {
    fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("w.kala")
    }

    fn w(&mut self) -> &mut World {
        self.world.as_mut().unwrap()
    }

    /// Resolve a spec against the entities visible right now.
    fn resolve_create(
        &self,
        ty: usize,
        handle: u8,
    ) -> (MCreate, Option<&'static str>, Option<String>) {
        let name = if ty < 3 { Some(TYPES[ty]) } else { None };
        let h = match handle {
            0..=2 => None,
            3 => Some("h3".to_string()),
            4 => Some("h4".to_string()),
            _ => Some(String::new()),
        };
        (
            MCreate {
                ty: (ty < 3).then_some(ty),
                handle: h.clone(),
            },
            name,
            h,
        )
    }

    fn resolve_write(&self, t: &MTxn, op: &OpSpec) -> (MWrite, String, EntityId, Value) {
        let OpSpec::Write {
            assert,
            ent,
            attr,
            val,
            tick,
            defect,
        } = op
        else {
            unreachable!()
        };
        let visible: Vec<Ent> = self
            .m
            .ents
            .iter()
            .map(|(e, _)| *e)
            .chain(t.staged_ents.iter().copied())
            .collect();
        let pick = |i: usize| visible[i % visible.len()];
        let (assert, mut defect, mut attr) = (*assert, *defect, *attr % ATTRS.len());
        if !assert && ATTRS[attr].event && (defect == 2 || defect == 6) {
            defect = 0; // would be two errors at once; the order is unspecified
        }
        match defect {
            3 => attr = A_HEIGHT,
            4 => attr = A_LOC,
            5 => attr = A_NAME,
            _ => {}
        }
        let mut subj = Some(pick(*ent));
        let mut attr_idx = Some(attr);
        let mut attr_name = ATTRS[attr].name.to_string();
        let mut ty = ATTRS[attr].ty;
        if defect == 2 {
            ty = match ty {
                ValueType::Bool => ValueType::I64,
                ValueType::I64 => ValueType::Str,
                ValueType::F64 => ValueType::Bool,
                ValueType::Str => ValueType::F64,
                ValueType::Ref => ValueType::I64,
            };
        }
        let mut neg0 = false;
        let mut mval = match ty {
            ValueType::Bool => Val::Mv(MV::Bool(val % 2 == 1)),
            ValueType::I64 => Val::Mv(MV::I64([0, -1, 1, i64::MIN, i64::MAX][val % 5])),
            ValueType::F64 => {
                neg0 = *val == 1;
                let x = [0.0f64, 0.0, 1.5, -2.5, f64::INFINITY][val % 5];
                Val::Mv(MV::F64(x.to_bits()))
            }
            ValueType::Str => Val::Mv(MV::Str(match val % 5 {
                0 => String::new(),
                1 => "a".into(),
                2 => "Edwin é".into(),
                3 => "x".repeat(256),
                _ => "y".repeat(257),
            })),
            ValueType::Ref => Val::Mv(MV::Ref(pick(*val))),
        };
        match defect {
            1 => {
                attr_idx = None;
                attr_name = "nope".into();
            }
            3 => mval = Val::NaN,
            4 => mval = Val::UnknownRef,
            5 => mval = Val::Mv(MV::Str("é".repeat(129))), // 258 bytes
            6 => subj = None,
            _ => {}
        }
        let real_subj = subj.map_or_else(bogus_entity, |e| self.real[&e]);
        let real_val = match &mval {
            Val::NaN => Value::F64(f64::NAN),
            Val::UnknownRef => Value::Ref(bogus_entity()),
            Val::Mv(MV::Bool(b)) => Value::Bool(*b),
            Val::Mv(MV::I64(i)) => Value::I64(*i),
            Val::Mv(MV::F64(bits)) => {
                let x = f64::from_bits(*bits);
                Value::F64(if neg0 && x == 0.0 { -0.0 } else { x })
            }
            Val::Mv(MV::Str(s)) => Value::Str(s.clone()),
            Val::Mv(MV::Ref(e)) => Value::Ref(self.real[e]),
        };
        (
            MWrite {
                assert,
                subj,
                attr: attr_idx,
                val: mval,
                tick: *tick,
            },
            attr_name,
            real_subj,
            real_val,
        )
    }

    /// Run one spec on both sides and compare every result.
    fn step(&mut self, spec: &TxnSpec, label: &str) {
        if matches!(spec.mode, Mode::Correct) {
            self.step_correct(spec, label);
            return;
        }
        let before_tx = self.m.tx;
        let ops: Vec<OpSpec> = match spec.mode {
            Mode::Auto => vec![spec.ops[0].clone()],
            _ => spec.ops.clone(),
        };
        let mut last_ok: Option<WriteResult> = None;
        let mut mt = MTxn::begin(
            &self.m,
            if matches!(spec.mode, Mode::Auto) {
                None
            } else {
                spec.default
            },
        );
        let mut new_real: Vec<(Ent, EntityId)> = vec![];
        let mut commit_result: Option<Result<CommitResult, Error>> = None;

        // resolve first (needs &self), then drive the real world
        let mut world = self.world.take().unwrap();
        {
            let mut tgt = if matches!(spec.mode, Mode::Auto) {
                Target::W(&mut world)
            } else {
                Target::T(world.begin())
            };
            if let (Target::T(t), Some(d)) = (&mut tgt, spec.default) {
                t.set_default_tick(Tick(d));
            }
            for (i, op) in ops.iter().enumerate() {
                let ctx = format!("{label} op#{i} {op:?}");
                match op {
                    OpSpec::Create { ty, handle } => {
                        let (mc, name, h) = self.resolve_create(*ty, *handle);
                        let name = name.unwrap_or("NOPE");
                        let exp = mt.create(&self.m, &mc);
                        let got = tgt.create(name, h.as_deref());
                        match (exp, got) {
                            (Ok(e), Ok(id)) => {
                                assert_eq!(
                                    format!("{}#{}", TYPES[e.0], e.1),
                                    fmt_id(&self.m, id),
                                    "{ctx}: entity id"
                                );
                                new_real.push((e, id));
                                self.real.insert(e, id);
                            }
                            (Err(k), Err(err)) => assert_eq!(k, ek(&err), "{ctx}: {err:?}"),
                            (e, g) => panic!("{ctx}: model {e:?} vs real {g:?}"),
                        }
                    }
                    OpSpec::EdgeWrite {
                        assert,
                        edge,
                        attr,
                        val,
                        tick,
                        bogus,
                    } => {
                        let edges = mt.visible_edges(&self.m);
                        if edges.is_empty() && !*bogus {
                            continue; // nothing to write on; skipped on both sides
                        }
                        let ai = EDGE_PROPS[*attr % EDGE_PROPS.len()];
                        let (mval, rval) = edge_prop_value(ATTRS[ai].ty, *val);
                        let (subj, real_subj) = if *bogus {
                            let seq = 9_999u64;
                            ((EDGE_T, seq), EntityId::from(EdgeId::new(seq).unwrap()))
                        } else {
                            let k = *edge % edges.len();
                            let (s, a, t) = edges[k];
                            let id = tgt
                                .edge_id(self.real[&s], ATTRS[a].name, self.real[&t])
                                .unwrap_or_else(|| panic!("{ctx}: no edge id for {:?}", edges[k]));
                            assert_eq!(id.seq(), k as u64 + 1, "{ctx}: edge seq");
                            ((EDGE_T, id.seq()), EntityId::from(id))
                        };
                        let mw = MWrite {
                            assert: *assert,
                            subj: Some(subj),
                            attr: Some(ai),
                            val: Val::Mv(mval),
                            tick: *tick,
                        };
                        let exp = mt.write(&self.m, &mw);
                        let got =
                            tgt.write(*assert, real_subj, ATTRS[ai].name, rval, tick.map(Tick));
                        match (exp, got) {
                            (Ok((c, t)), Ok(r)) => {
                                assert_eq!(
                                    (c, Tick(t)),
                                    (r.changed, r.tick),
                                    "{ctx}: write result"
                                );
                                last_ok = Some(r);
                            }
                            (Err(k), Err(err)) => assert_eq!(k, ek(&err), "{ctx}: {err:?}"),
                            (e, g) => panic!("{ctx}: model {e:?} vs real {g:?}"),
                        }
                    }
                    OpSpec::Correct { .. } => {} // only in Mode::Correct
                    OpSpec::Write { .. } => {
                        let (mw, attr, subj, val) = self.resolve_write(&mt, op);
                        let exp = mt.write(&self.m, &mw);
                        let tick = mw.tick.map(Tick);
                        let got = tgt.write(mw.assert, subj, &attr, val, tick);
                        match (exp, got) {
                            (Ok((c, t)), Ok(r)) => {
                                assert_eq!(
                                    (c, Tick(t)),
                                    (r.changed, r.tick),
                                    "{ctx}: write result"
                                );
                                last_ok = Some(r);
                            }
                            (Err(k), Err(err)) => assert_eq!(k, ek(&err), "{ctx}: {err:?}"),
                            (e, g) => panic!("{ctx}: model {e:?} vs real {g:?}"),
                        }
                    }
                }
            }
            match (spec.mode, tgt) {
                (Mode::Commit, Target::T(t)) => commit_result = Some(t.commit()),
                (_, t) => drop(t), // Drop mode, or Auto (already committed per write)
            }
        }
        self.world = Some(world);

        // model side of the end of the transaction
        let edges_before = self.m.edges.len();
        let new_edges = mt.staged_edges.len();
        let mut real_warnings: Option<Warnings> = None;
        let mut real_edges: Option<Vec<EdgeId>> = None;
        match spec.mode {
            Mode::Commit => {
                let real = commit_result.unwrap();
                let ids = real.as_ref().map(|r| r.entries.clone()).unwrap_or_default();
                let exp = mt.commit(&mut self.m, &ids);
                match (exp, real) {
                    (Ok((tx, ch)), Ok(r)) => {
                        assert_eq!((TxId(tx), ch), (r.tx, r.changed), "{label}: commit result");
                        real_warnings = Some(r.warnings);
                        real_edges = Some(r.edges);
                    }
                    (Err(k), Err(err)) => assert_eq!(k, ek(&err), "{label}: commit {err:?}"),
                    (e, g) => panic!("{label}: commit: model {e:?} vs real {g:?}"),
                }
            }
            Mode::Drop | Mode::Correct => {
                let _ = &mt; // discarded: nothing applies, no tx, IDs and handles free
            }
            Mode::Auto => {
                // an auto-commit applies its one write if it succeeded
                let ids = last_ok
                    .as_ref()
                    .map(|r| r.entries.clone())
                    .unwrap_or_default();
                if mt.commit(&mut self.m, &ids).is_ok() {
                    real_warnings = Some(
                        last_ok
                            .as_ref()
                            .map(|r| r.warnings.clone())
                            .unwrap_or_default(),
                    );
                    real_edges = Some(
                        last_ok
                            .as_ref()
                            .map(|r| r.edges.clone())
                            .unwrap_or_default(),
                    );
                }
            }
        }
        if let Some(edges) = real_edges {
            let got: Vec<u64> = edges.iter().map(|e| e.seq()).collect();
            let want: Vec<u64> =
                (edges_before as u64 + 1..=(edges_before + new_edges) as u64).collect();
            assert_eq!(got, want, "{label}: newly interned edges");
        }
        self.check_warnings(label, real_warnings);
        let _ = before_tx;
        assert_eq!(self.w().latest_tx(), TxId(self.m.tx), "{label}: latest_tx");
        let lt = Tick(self.m.latest_tick());
        assert_eq!(self.w().latest_tick(), lt, "{label}: latest_tick");
        let _ = new_real;
    }

    /// Everything readable, at sampled canons, against the model.
    fn compare_all(&self, label: &str) {
        let w = self.world.as_ref().unwrap();
        let m = &self.m;
        assert_eq!(w.latest_tx(), TxId(m.tx), "{label}: latest_tx");
        assert_eq!(
            w.latest_tick(),
            Tick(m.latest_tick()),
            "{label}: latest_tick"
        );
        let n = m.tx;
        let mut canons: Vec<u64> = if n <= 8 {
            (0..=n).collect()
        } else {
            vec![0, 1, n / 3, n / 2, 2 * n / 3, n - 1, n]
        };
        canons.extend(self.extra_canons.iter().map(|x| x % (n + 1)));
        canons.sort_unstable();
        canons.dedup();
        self.compare_graph(label, &canons);
        self.compare_overlaps(label, &canons);
        self.check_entries(label);
        if canons
            .iter()
            .any(|&c| m.facts.iter().any(|f| f.kind == FK::Deleted && f.tx > c))
        {
            stats::bump(stats::OLD_CANON_DELETED);
        }
        for c in canons {
            for view in views(w, c, n) {
                for (e, created) in m.ents.iter() {
                    if *created > c {
                        continue;
                    }
                    let id = self.real[e];
                    for (ai, a) in ATTRS.iter().enumerate() {
                        let ctx = format!("{label} canon {c} {e:?} {}", a.name);
                        let hist = view.history(id, a.name).unwrap();
                        let got_h: Vec<(i64, bool, MV)> = hist
                            .iter()
                            .map(|ch| (ch.tick.0, ch.op == Op::Assert, from_value(&ch.value)))
                            .collect();
                        let exp_h = m_history(m, c, *e, ai);
                        assert_eq!(got_h, exp_h, "{ctx}: history");
                        let mut ticks: Vec<i64> = vec![i64::MIN, i64::MAX, -1, 0, 1];
                        for (t, _, _) in &exp_h {
                            ticks.extend([t.saturating_sub(1), *t, t.saturating_add(1)]);
                        }
                        ticks.sort_unstable();
                        ticks.dedup();
                        let vals: Vec<MV> = {
                            let mut v: Vec<MV> = vec![];
                            for (_, _, x) in &exp_h {
                                if !v.contains(x) {
                                    v.push(x.clone());
                                }
                            }
                            v
                        };
                        for t in ticks {
                            let got: Vec<MV> = view
                                .as_of(id, a.name, Tick(t))
                                .unwrap()
                                .iter()
                                .map(from_value)
                                .collect();
                            assert_eq!(got, m_as_of(m, c, *e, ai, t), "{ctx}: as_of @{t}");
                            for v in &vals {
                                let real_v = to_value(v);
                                let got = view.as_of_value(id, a.name, &real_v, Tick(t)).unwrap();
                                assert_eq!(
                                    got,
                                    m_holds(m, c, *e, ai, v, t),
                                    "{ctx}: as_of_value {v:?} @{t}"
                                );
                                if c == n {
                                    let g = w.as_of_value(id, a.name, &real_v, Tick(t)).unwrap();
                                    assert_eq!(g, got, "{ctx}: world vs view @{t}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Edges, edge properties and `neighbors`, against the model.
    fn compare_graph(&self, label: &str, canons: &[u64]) {
        let w = self.world.as_ref().unwrap();
        let m = &self.m;
        let n = m.tx;
        let ref_attrs: Vec<usize> = (0..ATTRS.len())
            .filter(|&a| ATTRS[a].ty == ValueType::Ref)
            .collect();

        // the edge table: IDs, triples, and nothing extra
        for (i, (s, a, t, _)) in m.edges.iter().enumerate() {
            let (rs, rt, name) = (self.real[s], self.real[t], ATTRS[*a].name);
            let id = w
                .edge_id(rs, name, rt)
                .unwrap_or_else(|| panic!("{label}: edge {i} {s:?} {name} {t:?} has no id"));
            assert_eq!(id.seq(), i as u64 + 1, "{label}: edge seq");
            assert_eq!(
                w.edge(id),
                Some((rs, name.to_string(), rt)),
                "{label}: edge()"
            );
            if ATTRS[*a].bidi {
                assert_eq!(w.edge_id(rt, name, rs), Some(id), "{label}: either order");
            }
        }
        assert!(
            w.edge(EdgeId::new(m.edges.len() as u64 + 1).unwrap())
                .is_none(),
            "{label}: no extra edge"
        );
        for (x, _) in m.ents.iter() {
            for (y, _) in m.ents.iter() {
                for &a in &ref_attrs {
                    let got = w
                        .edge_id(self.real[x], ATTRS[a].name, self.real[y])
                        .map(|e| e.seq());
                    assert_eq!(
                        got,
                        m.edge_seq(*x, a, *y),
                        "{label}: edge_id({x:?}, {}, {y:?})",
                        ATTRS[a].name
                    );
                }
            }
        }

        for &c in canons {
            for view in views(w, c, n) {
                // edge properties
                for (i, (_, _, _, etx)) in m.edges.iter().enumerate() {
                    if *etx > c {
                        continue;
                    }
                    let ent: Ent = (EDGE_T, i as u64 + 1);
                    let id = EntityId::from(EdgeId::new(ent.1).unwrap());
                    for &pa in &EDGE_PROPS {
                        let name = ATTRS[pa].name;
                        let ctx = format!("{label} canon {c} edge#{} {name}", ent.1);
                        let exp_h = m_history(m, c, ent, pa);
                        let got_h: Vec<(i64, bool, MV)> = view
                            .history(id, name)
                            .unwrap()
                            .iter()
                            .map(|ch| (ch.tick.0, ch.op == Op::Assert, from_value(&ch.value)))
                            .collect();
                        assert_eq!(got_h, exp_h, "{ctx}: history");
                        let mut ticks: Vec<i64> = vec![i64::MIN, i64::MAX, 0];
                        for (t, _, _) in &exp_h {
                            ticks.extend([t.saturating_sub(1), *t, t.saturating_add(1)]);
                        }
                        ticks.sort_unstable();
                        ticks.dedup();
                        for t in ticks {
                            let got: Vec<MV> = view
                                .as_of(id, name, Tick(t))
                                .unwrap()
                                .iter()
                                .map(from_value)
                                .collect();
                            assert_eq!(got, m_as_of(m, c, ent, pa, t), "{ctx}: as_of @{t}");
                            for (_, _, v) in &exp_h {
                                assert_eq!(
                                    view.as_of_value(id, name, &to_value(v), Tick(t)).unwrap(),
                                    m_holds(m, c, ent, pa, v, t),
                                    "{ctx}: as_of_value {v:?} @{t}"
                                );
                            }
                        }
                    }
                }

                // neighbors
                for (e, created) in m.ents.iter() {
                    if *created > c {
                        continue;
                    }
                    let id = self.real[e];
                    for &a in &ref_attrs {
                        let name = ATTRS[a].name;
                        let mut ticks: Vec<i64> = vec![i64::MIN, i64::MAX, 0];
                        for f in m
                            .facts
                            .iter()
                            .filter(|f| f.tx <= c && f.a == a && (f.e == *e || f.v == MV::Ref(*e)))
                        {
                            ticks.extend([
                                f.tick.saturating_sub(1),
                                f.tick,
                                f.tick.saturating_add(1),
                            ]);
                        }
                        ticks.sort_unstable();
                        ticks.dedup();
                        let to_ent = |(id, edge): (EntityId, EdgeId)| {
                            ((usize::from(id.node_type().0), id.seq()), edge.seq())
                        };
                        for t in ticks {
                            for dir in [Direction::Out, Direction::In, Direction::Both] {
                                let ctx = format!("{label} canon {c} {e:?} {name} {dir:?} @{t}");
                                let exp = m_neighbors(m, c, *e, a, dir, t);
                                let got: Vec<(Ent, u64)> = view
                                    .neighbors(id, name, dir, Tick(t))
                                    .unwrap()
                                    .into_iter()
                                    .map(to_ent)
                                    .collect();
                                assert_eq!(got, exp, "{ctx}: neighbors");
                                if c == n {
                                    let g: Vec<(Ent, u64)> = w
                                        .neighbors(id, name, dir, Tick(t))
                                        .unwrap()
                                        .into_iter()
                                        .map(to_ent)
                                        .collect();
                                    assert_eq!(g, exp, "{ctx}: world neighbors");
                                }
                                if let Some(inv) = ATTRS[a].inverse {
                                    let flipped = match dir {
                                        Direction::Out => Direction::In,
                                        Direction::In => Direction::Out,
                                        Direction::Both => Direction::Both,
                                    };
                                    let g: Vec<(Ent, u64)> = view
                                        .neighbors(id, inv, dir, Tick(t))
                                        .unwrap()
                                        .into_iter()
                                        .map(to_ent)
                                        .collect();
                                    assert_eq!(
                                        g,
                                        m_neighbors(m, c, *e, a, flipped, t),
                                        "{ctx}: inverse {inv}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ------------------------------------------------ slice 3 additions

    /// Model entity of a real ID (an edge or an entity).
    fn ent_of(id: EntityId) -> Ent {
        match id.as_edge() {
            Some(e) => (EDGE_T, e.seq()),
            None => (usize::from(id.node_type().0), id.seq()),
        }
    }

    fn conv_ov(o: &Overlap) -> MOv {
        MOv {
            attr: ATTRS
                .iter()
                .position(|a| a.name == o.attr)
                .unwrap_or_else(|| panic!("overlap on unknown attribute {}", o.attr)),
            ent: Self::ent_of(o.entity),
            reverse: o.side == Side::Reverse,
            a: from_value(&o.a),
            b: from_value(&o.b),
            from: o.during.from.0,
            until: o.during.until.map(|t| t.0),
        }
    }

    /// After a step: the write's warnings must be (overlaps after) minus
    /// (overlaps before), and (before) minus (after), over the whole world;
    /// `None` means nothing was committed, so nothing may have changed.
    fn check_warnings(&mut self, label: &str, real: Option<Warnings>) {
        let after = m_overlaps_all(&self.m, self.m.tx);
        let mut introduced: Vec<MOv> = after
            .iter()
            .filter(|o| !self.ov.contains(o))
            .cloned()
            .collect();
        let mut resolved: Vec<MOv> = self
            .ov
            .iter()
            .filter(|o| !after.contains(o))
            .cloned()
            .collect();
        introduced.sort_by(mov_cmp);
        resolved.sort_by(mov_cmp);
        let real_w = real.clone();
        match real {
            Some(w) => {
                let mut got_i: Vec<MOv> = w.introduced.iter().map(Self::conv_ov).collect();
                let mut got_r: Vec<MOv> = w.resolved.iter().map(Self::conv_ov).collect();
                got_i.sort_by(mov_cmp);
                got_r.sort_by(mov_cmp);
                assert_eq!(got_i, introduced, "{label}: warnings.introduced");
                assert_eq!(got_r, resolved, "{label}: warnings.resolved");
            }
            None => {
                assert!(
                    introduced.is_empty() && resolved.is_empty(),
                    "{label}: no commit but overlaps changed"
                );
            }
        }
        if let Some(w) = &real_w {
            if !w.introduced.is_empty() {
                stats::bump(stats::W_INTRO);
            }
            if !w.resolved.is_empty() {
                stats::bump(stats::W_RESOLVED);
            }
        }
        self.ov = after;
    }

    /// One auto-committed `World::correct`, against the model.
    fn step_correct(&mut self, spec: &TxnSpec, label: &str) {
        let OpSpec::Correct {
            pick,
            which,
            delete,
            value,
            tick,
            op,
            defect,
            collide,
        } = &spec.ops[0]
        else {
            panic!("{label}: Mode::Correct needs a Correct op")
        };
        if self.m.chains.is_empty() {
            return; // nothing to correct yet; skipped on both sides
        }
        let ci = pick % self.m.chains.len();
        let chain = self.m.chains[ci].clone();
        let cur = self.m.facts[chain.cur].clone();
        let a = cur.a;
        let ty = ATTRS[a].ty;

        // which entry ID the caller passes, and the error it must produce
        let mut defect = *defect;
        let mut forced: Option<EK> = None;
        let entry = match defect {
            1 => {
                forced = Some(EK::UnknownEntry);
                EntryId(1_000_000_000)
            }
            2 => {
                forced = Some(EK::NotAFact);
                let id = EntryId(1);
                let info = self
                    .world
                    .as_ref()
                    .unwrap()
                    .entry(id)
                    .expect("entry#1 exists");
                assert!(
                    !matches!(info.body, EntryBody::Fact { .. }),
                    "entry#1 is a genesis DEFINE, not a fact: {info:?}"
                );
                id
            }
            _ => match which % 4 {
                2 => chain.ids[0],
                3 if !chain.dels.is_empty() => {
                    forced = Some(EK::NotAFact);
                    *chain.dels.last().unwrap()
                }
                _ => *chain.ids.last().unwrap(),
            },
        };
        if forced.is_none() && chain.deleted {
            forced = Some(EK::AlreadyDeleted);
        }
        if forced.is_some() || *delete {
            defect = 0; // one error at a time
        }

        // the replacement
        let mut neg0 = false;
        let mut new_value: Option<Val> = value.map(|v| match ty {
            ValueType::Bool => Val::Mv(MV::Bool(v % 2 == 1)),
            ValueType::I64 => Val::Mv(MV::I64([0, -1, 1, i64::MIN, i64::MAX][v % 5])),
            ValueType::F64 => {
                neg0 = v % 5 == 1;
                Val::Mv(MV::F64(
                    [0.0f64, 0.0, 1.5, -2.5, f64::INFINITY][v % 5].to_bits(),
                ))
            }
            ValueType::Str => Val::Mv(MV::Str(match v % 5 {
                0 => String::new(),
                1 => "a".into(),
                2 => "Edwin é".into(),
                3 => "x".repeat(256),
                _ => "y".repeat(257),
            })),
            ValueType::Ref => {
                let n = self.m.ents.len();
                Val::Mv(MV::Ref(self.m.ents[v % n].0))
            }
        });
        let mut new_op = *op;
        match defect {
            3 => {
                let wrong = match ty {
                    ValueType::Bool => MV::I64(1),
                    ValueType::I64 => MV::Str("s".into()),
                    ValueType::F64 => MV::Bool(true),
                    ValueType::Str => MV::F64(1.5f64.to_bits()),
                    ValueType::Ref => MV::I64(1),
                };
                new_value = Some(Val::Mv(wrong));
            }
            4 if ty == ValueType::F64 => new_value = Some(Val::NaN),
            5 if ty == ValueType::Ref => new_value = Some(Val::UnknownRef),
            6 if ty == ValueType::Str && ATTRS[a].indexed => {
                new_value = Some(Val::Mv(MV::Str("é".repeat(129))));
            }
            7 if ATTRS[a].event => new_op = Some(false),
            _ => {}
        }
        let mut new_tick = *tick;
        let other = self
            .m
            .live(self.m.tx)
            .iter()
            .find(|g| g.e == cur.e && g.a == a && g.v == cur.v && g.tick != cur.tick)
            .map(|g| g.tick);
        if let Some(t) = other
            && *collide
            && !*delete
            && defect == 0
        {
            new_tick = Some(t);
            new_value = None;
        }

        // ---- model plan
        let kind_label = if *delete { "delete" } else { "replace" };
        let ctx = format!("{label} {kind_label} chain#{ci} entry {entry:?} (cur {cur:?})");
        let plan: Result<Plan, EK> = (|| {
            if let Some(k) = forced {
                return Err(k);
            }
            if *delete {
                let f = MFact {
                    kind: FK::Deleted,
                    assert: false,
                    real: None,
                    tx: 0,
                    rev: chain.rev + 1,
                    chain: ci,
                    ..cur.clone()
                };
                return Ok(Plan {
                    changed: 1,
                    tick: cur.tick,
                    entries: vec![f],
                    edge: None,
                });
            }
            // validate like a write
            let mut nv: Option<MV> = None;
            match &new_value {
                None => {}
                Some(Val::NaN) => return Err(EK::Value),
                Some(Val::UnknownRef) => return Err(EK::UnknownEntity),
                Some(Val::Mv(v)) => {
                    if mv_type(v) != ty {
                        return Err(EK::WrongValueType);
                    }
                    if let MV::Str(s) = v
                        && ATTRS[a].indexed
                        && s.len() > 256
                    {
                        return Err(EK::Value);
                    }
                    nv = Some(v.clone());
                }
            }
            let op_assert = new_op.unwrap_or(cur.assert);
            if !op_assert && ATTRS[a].event {
                return Err(EK::RetractEvent);
            }
            let t = new_tick.unwrap_or(cur.tick);
            let (ne, v) = match nv {
                None => (cur.e, cur.v.clone()),
                Some(MV::Ref(t)) if cur.e.0 != EDGE_T => {
                    let (x, y) = norm(a, cur.e, t);
                    (x, MV::Ref(y))
                }
                Some(v) => (cur.e, v),
            };
            let mk = |kind: FK, e: Ent, v: MV, tick: i64, assert: bool| MFact {
                e,
                a,
                v,
                tick,
                tx: 0,
                assert,
                kind,
                chain: ci,
                rev: chain.rev + 1,
                real: None,
            };
            let deleted = mk(FK::Deleted, cur.e, cur.v.clone(), cur.tick, false);
            let same_key = (ne, &v, t) == (cur.e, &cur.v, cur.tick);
            if same_key {
                if op_assert == cur.assert {
                    return Ok(Plan {
                        changed: 0,
                        tick: t,
                        entries: vec![],
                        edge: None,
                    });
                }
                // flipping a retract (of a triple that never had an assert, e.g.
                // one moved here by an earlier correction) to an assert
                // interns the triple's edge
                let edge = match &v {
                    MV::Ref(tgt)
                        if op_assert
                            && ne.0 != EDGE_T
                            && self.m.edge_seq(ne, a, *tgt).is_none() =>
                    {
                        Some((ne, a, *tgt))
                    }
                    _ => None,
                };
                return Ok(Plan {
                    changed: 1,
                    tick: t,
                    entries: vec![mk(FK::Revised, ne, v, t, op_assert)],
                    edge,
                });
            }
            let live = self.m.live(self.m.tx);
            if let Some(o) = live
                .iter()
                .find(|o| o.e == ne && o.a == a && o.v == v && o.tick == t)
            {
                if o.assert != op_assert {
                    return Err(EK::SameTickConflict);
                }
                return Ok(Plan {
                    changed: 1,
                    tick: t,
                    entries: vec![deleted],
                    edge: None,
                });
            }
            let edge = match &v {
                MV::Ref(tgt)
                    if op_assert && ne.0 != EDGE_T && self.m.edge_seq(ne, a, *tgt).is_none() =>
                {
                    Some((ne, a, *tgt))
                }
                _ => None,
            };
            Ok(Plan {
                changed: 2,
                tick: t,
                entries: vec![deleted, mk(FK::Revised, ne, v, t, op_assert)],
                edge,
            })
        })();

        // ---- real call
        let correction = if *delete {
            Correction::Delete
        } else {
            let real_value = new_value.as_ref().map(|v| match v {
                Val::NaN => Value::F64(f64::NAN),
                Val::UnknownRef => Value::Ref(bogus_entity()),
                Val::Mv(MV::Bool(b)) => Value::Bool(*b),
                Val::Mv(MV::I64(i)) => Value::I64(*i),
                Val::Mv(MV::F64(bits)) => {
                    let x = f64::from_bits(*bits);
                    Value::F64(if neg0 && x == 0.0 { -0.0 } else { x })
                }
                Val::Mv(MV::Str(s)) => Value::Str(s.clone()),
                Val::Mv(MV::Ref(e)) => Value::Ref(self.real[e]),
            });
            Correction::Replace {
                value: real_value,
                tick: new_tick.map(Tick),
                op: new_op.map(|b| if b { Op::Assert } else { Op::Retract }),
            }
        };
        let got = self.w().correct(entry, correction);
        let mut real_warnings = None;
        match (plan, got) {
            (Ok(plan), Ok(r)) => {
                assert_eq!(
                    (plan.changed, Tick(plan.tick)),
                    (r.changed, r.tick),
                    "{ctx}: correct result"
                );
                assert_eq!(r.entries.len(), plan.changed, "{ctx}: entries");
                stats::bump(
                    match (*delete, plan.changed, plan.entries.last().map(|f| f.kind)) {
                        (true, ..) => stats::DELETE,
                        (_, 0, _) => stats::NOOP,
                        (_, 1, Some(FK::Revised)) => stats::FLIP,
                        (_, 1, _) => stats::MOVE_ONTO,
                        _ => stats::MOVE,
                    },
                );
                if ATTRS[a].bidi && value.is_some() && plan.changed > 0 {
                    stats::bump(stats::BIDI_MOVE);
                }
                let want_edge: Vec<u64> = plan
                    .edge
                    .iter()
                    .map(|_| self.m.edges.len() as u64 + 1)
                    .collect();
                let got_edge: Vec<u64> = r.edges.iter().map(|e| e.seq()).collect();
                assert_eq!(got_edge, want_edge, "{ctx}: edges");
                real_warnings = Some(r.warnings.clone());
                if plan.changed > 0 {
                    self.m.apply(ci, plan, &r.entries);
                    let tx = self.w().latest_tx();
                    assert_eq!(tx, TxId(self.m.tx), "{ctx}: tx");
                } else {
                    assert!(r.warnings == Warnings::default(), "{ctx}: no-op warnings");
                }
            }
            (Err(k), Err(err)) => {
                assert_eq!(k, ek(&err), "{ctx}: {err:?}");
                stats::bump(match k {
                    EK::UnknownEntry => stats::E_UNKNOWN,
                    EK::NotAFact => stats::E_NOT_A_FACT,
                    EK::AlreadyDeleted => stats::E_ALREADY,
                    EK::SameTickConflict => stats::E_SAME_TICK,
                    _ => stats::E_VALID,
                });
            }
            (e, g) => panic!("{ctx}: model {e:?} vs real {g:?}"),
        }
        self.check_warnings(label, real_warnings);
        assert_eq!(self.w().latest_tx(), TxId(self.m.tx), "{label}: latest_tx");
        let lt = Tick(self.m.latest_tick());
        assert_eq!(self.w().latest_tick(), lt, "{label}: latest_tick");
    }

    /// Every stored entry the model knows is a committed entry of the world
    /// with the right tx, kind, tick, op and revision.
    fn check_entries(&self, label: &str) {
        let w = self.world.as_ref().unwrap();
        let mut last = 0u64;
        for f in &self.m.facts {
            let id = f.real.expect("every stored entry has an entry id");
            assert!(
                id.0 > last,
                "{label}: entry ids ascend ({last} then {})",
                id.0
            );
            last = id.0;
            let info = w
                .entry(id)
                .unwrap_or_else(|| panic!("{label}: entry {id:?} of {f:?} is unknown"));
            assert_eq!(info.id, id);
            assert_eq!(info.tx, TxId(f.tx), "{label}: tx of {id:?}");
            let base = self.m.chains[f.chain].ids[0];
            match (&info.body, f.kind) {
                (EntryBody::Fact { tick, op, .. }, FK::Base) => {
                    assert_eq!(tick.0, f.tick, "{label}: {id:?}");
                    assert_eq!(*op == Op::Assert, f.assert, "{label}: {id:?}");
                    assert_eq!(info.revision(), None);
                }
                (
                    EntryBody::Revised {
                        tick, op, of, rev, ..
                    },
                    FK::Revised,
                ) => {
                    assert_eq!(tick.0, f.tick, "{label}: {id:?}");
                    assert_eq!(*op == Op::Assert, f.assert, "{label}: {id:?}");
                    assert_eq!((*of, *rev), (base, f.rev), "{label}: {id:?}");
                    assert_eq!(info.revision(), Some((base, f.rev)));
                }
                (EntryBody::Deleted { tick, of, rev, .. }, FK::Deleted) => {
                    assert_eq!(tick.0, f.tick, "{label}: {id:?}");
                    assert_eq!((*of, *rev), (base, f.rev), "{label}: {id:?}");
                    assert_eq!(info.revision(), Some((base, f.rev)));
                }
                (b, k) => panic!("{label}: {id:?} is {b:?}, the model says {k:?}"),
            }
        }
        assert!(w.entry(EntryId(last + 1_000_000)).is_none());
    }

    /// `overlaps` of every attribute at the given canons (and, at the latest
    /// canon, through the world and the main-index path) against the model.
    fn compare_overlaps(&self, label: &str, canons: &[u64]) {
        let w = self.world.as_ref().unwrap();
        let n = self.m.tx;
        for &c in canons {
            let mut views = vec![w.at_canon(TxId(c))];
            if c == n {
                views.push(w.at_canon_main(TxId(c)));
            }
            for (ai, a) in ATTRS.iter().enumerate() {
                let exp = m_overlaps(&self.m, c, ai);
                for view in &views {
                    let got: Vec<MOv> = view
                        .overlaps(a.name)
                        .unwrap()
                        .iter()
                        .map(Self::conv_ov)
                        .collect();
                    assert_eq!(got, exp, "{label} canon {c} overlaps({})", a.name);
                }
                if c == n {
                    let got: Vec<MOv> = w
                        .overlaps(a.name)
                        .unwrap()
                        .iter()
                        .map(Self::conv_ov)
                        .collect();
                    assert_eq!(got, exp, "{label}: World::overlaps({})", a.name);
                }
            }
        }
    }

    /// Doctor at `canon`: no Error findings; the warnings are exactly the
    /// model's overlaps and unasserted-edge properties.
    fn check_doctor(&self, label: &str, canon: Option<u64>) {
        let w = self.world.as_ref().unwrap();
        let c = canon.unwrap_or(self.m.tx);
        let rep = doctor(
            w,
            &DoctorOptions {
                canon: canon.map(TxId),
                since: None,
            },
        );
        assert!(
            rep.errors().is_empty(),
            "{label}: doctor errors {:#?}",
            rep.errors()
        );
        let mut got_ov: Vec<MOv> = vec![];
        let mut got_ep: Vec<(u64, u64, String, i64)> = vec![];
        for f in &rep.findings {
            match f {
                Finding::CardinalityOverlap(o) => got_ov.push(Self::conv_ov(o)),
                Finding::EdgePropertyOnUnassertedEdge {
                    entry,
                    edge,
                    attr,
                    tick,
                } => got_ep.push((entry.0, edge.seq(), attr.clone(), tick.0)),
                other => panic!("{label}: unexpected doctor finding {other:?}"),
            }
        }
        got_ov.sort_by(mov_cmp);
        got_ep.sort();
        if got_ov.iter().any(|o| o.reverse) {
            stats::bump(stats::REVERSE);
        }
        if got_ov.iter().any(|o| ATTRS[o.attr].bidi) {
            stats::bump(stats::BIDI_OV);
        }
        if !got_ep.is_empty() {
            stats::bump(stats::EDGE_FINDING);
        }
        let mut exp_ov = m_overlaps_all(&self.m, c);
        exp_ov.sort_by(mov_cmp);
        assert_eq!(
            got_ov, exp_ov,
            "{label} canon {c}: doctor CardinalityOverlap"
        );
        assert_eq!(
            got_ep,
            m_edge_findings(&self.m, c),
            "{label} canon {c}: doctor EdgePropertyOnUnassertedEdge"
        );
    }

    fn reopen(&mut self) {
        drop(self.world.take());
        let w = World::open(&self.path()).expect("reopen");
        self.world = Some(w);
        // entity IDs are plain values: they stay valid across reopen
    }
}

/// The views to read canon `c` through: the normal one and, at the latest
/// canon, also the one forced onto the main indexes.
fn views(w: &World, c: u64, n: u64) -> Vec<View<'_>> {
    let mut v = vec![w.at_canon(TxId(c))];
    if c == n {
        v.push(w.at_canon_main(TxId(c)));
    }
    v
}

fn to_value(v: &MV) -> Value {
    match v {
        MV::Bool(b) => Value::Bool(*b),
        MV::I64(i) => Value::I64(*i),
        MV::F64(b) => Value::F64(f64::from_bits(*b)),
        MV::Str(s) => Value::Str(s.clone()),
        MV::Ref(e) => Value::Ref(EntityId::new(NodeTypeId(e.0 as u16), e.1).unwrap()),
    }
}

/// "CHARACTER#3" from a real ID, using only the schema's type order.
fn fmt_id(_m: &Model, id: EntityId) -> String {
    format!("{}#{}", TYPES[usize::from(id.node_type().0)], id.seq())
}

fn setup() -> Sys {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let world = World::create(&path, schema()).unwrap();
    let mut sys = Sys {
        dir,
        world: Some(world),
        m: Model::default(),
        real: HashMap::new(),
        ov: vec![],
        extra_canons: vec![],
    };
    // a few entities up front, each its own auto-commit
    for ty in [0usize, 0, 0, 1, 1, 2] {
        let id = sys.w().create_entity(TYPES[ty], None).unwrap();
        sys.m.counters[ty] += 1;
        let ent = (ty, sys.m.counters[ty]);
        sys.m.tx += 1;
        sys.m.ents.push((ent, sys.m.tx));
        assert_eq!(format!("{}#{}", TYPES[ty], ent.1), fmt_id(&sys.m, id));
        sys.real.insert(ent, id);
    }
    sys
}

fn run(batch1: Vec<TxnSpec>, batch2: Vec<TxnSpec>) {
    run_with(batch1, batch2, vec![]);
}

fn run_with(batch1: Vec<TxnSpec>, batch2: Vec<TxnSpec>, extra_canons: Vec<u64>) {
    let mut sys = setup();
    sys.extra_canons = extra_canons;
    sys.compare_all("setup");
    for (i, s) in batch1.iter().enumerate() {
        let label = format!("b1 txn#{i} {:?}", s.mode);
        sys.step(s, &label);
        if s.check {
            sys.compare_all(&label);
        }
    }
    sys.compare_all("end of batch 1");
    sys.check_doctor("end of batch 1", None);
    let mid = sys.extra_canons.first().map(|c| c % (sys.m.tx + 1));
    sys.check_doctor("end of batch 1, older canon", mid);
    sys.reopen();
    sys.compare_all("after reopen 1");
    sys.check_doctor("after reopen 1", None);
    for (i, s) in batch2.iter().enumerate() {
        let label = format!("b2 txn#{i} {:?}", s.mode);
        sys.step(s, &label);
        if s.check {
            sys.compare_all(&label);
        }
    }
    sys.compare_all("end of batch 2");
    sys.check_doctor("end of batch 2", None);
    let mid = sys.extra_canons.get(1).map(|c| c % (sys.m.tx + 1));
    sys.check_doctor("end of batch 2, older canon", mid);
    sys.reopen();
    sys.compare_all("after reopen 2");
    sys.check_doctor("after reopen 2", None);
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        rng_seed: RngSeed::Fixed(0x4b41_4c41_0003),
        .. ProptestConfig::default()
    })]

    #[test]
    fn world_matches_the_naive_model_before_and_after_reopen(
        batch1 in prop::collection::vec(txn_strategy(), 0..=28),
        batch2 in prop::collection::vec(txn_strategy(), 0..=12),
        canons in prop::collection::vec(any::<u64>(), 0..=4),
    ) {
        run_with(batch1, batch2, canons);
    }
}

/// Sanity: the harness itself exercises the interesting paths on a fixed
/// script (not random), so a broken generator can't make the property vacuous.
#[test]
fn fixed_script_hits_noop_conflict_abort_default_and_reopen() {
    let w = |assert, ent, attr, val, tick, defect| OpSpec::Write {
        assert,
        ent,
        attr,
        val,
        tick,
        defect,
    };
    let c = |ops: Vec<OpSpec>| TxnSpec {
        mode: Mode::Commit,
        default: None,
        ops,
        check: true,
    };
    let script = vec![
        // assert at 5, retract at 7, retract again at 6 (no-op? holds at 6 -> stored)
        c(vec![
            w(true, 0, A_LOC, 0, Some(5), 0),
            w(false, 0, A_LOC, 0, Some(7), 0),
        ]),
        c(vec![w(false, 0, A_LOC, 0, Some(6), 0)]),
        // same-tick conflict aborts the txn, nothing applies
        c(vec![
            w(true, 1, 3, 0, Some(2), 0),
            w(false, 1, 3, 0, Some(2), 0),
        ]),
        // retract event
        c(vec![w(false, 1, 7, 0, Some(1), 0)]),
        // tick-less writes land on the txn-begin latest tick
        c(vec![w(true, 2, 3, 1, None, 0), w(false, 2, 3, 1, None, 0)]),
        c(vec![
            w(true, 2, 3, 2, None, 0),
            w(true, 2, 3, 2, Some(i64::MIN), 0),
            w(true, 2, 4, 1, Some(i64::MAX), 0),
        ]),
        c(vec![
            w(true, 3, A_HEIGHT, 1, Some(0), 0),
            w(true, 3, A_HEIGHT, 0, Some(0), 0),
        ]),
    ];
    run(
        script,
        vec![c(vec![
            OpSpec::Create { ty: 0, handle: 3 },
            w(true, 40, 3, 1, None, 0),
        ])],
    );
}

/// Sanity for the slice-2 parts: a fixed script that interns edges through
/// both spellings, hits a bidirectional no-op and same-tick conflict, a self
/// pair, an inverse-named attribute, edge properties (including on an edge
/// staged in the same txn, a retracted event property and a never-defined
/// edge), and an aborted txn that must not consume an edge. The model is
/// inspected so a broken generator can't make the property vacuous.
#[test]
fn fixed_graph_script_interns_edges_and_checks_neighbors_and_properties() {
    const A_FRIEND: usize = 9;
    let w = |assert, ent, attr, val, tick| OpSpec::Write {
        assert,
        ent,
        attr,
        val,
        tick,
        defect: 0,
    };
    let ew = |assert, edge, attr, val, tick, bogus| OpSpec::EdgeWrite {
        assert,
        edge,
        attr,
        val,
        tick,
        bogus,
    };
    let c = |ops: Vec<OpSpec>| TxnSpec {
        mode: Mode::Commit,
        default: None,
        ops,
        check: true,
    };
    // visible entities: 0 C1, 1 C2, 2 C3, 3 P1, 4 P2, 5 I1
    let script = vec![
        // C2 friend_of C1 (larger first) -> stored (C1, C2), edge#1
        c(vec![w(true, 1, A_FRIEND, 0, Some(-5))]),
        // other spelling at a later tick: already holds, no-op, no new edge
        c(vec![w(true, 0, A_FRIEND, 1, Some(-3))]),
        // retract with the other spelling, then the same-tick opposite aborts
        c(vec![w(false, 0, A_FRIEND, 1, Some(-1))]),
        c(vec![w(true, 1, A_FRIEND, 0, Some(-1))]),
        // self pair and a cross-type pair (CHARACTER < PLACE)
        c(vec![
            w(true, 2, A_FRIEND, 2, Some(0)),
            w(true, 3, A_FRIEND, 0, Some(4)),
        ]),
        // directed with an inverse name: C1 parent_of C2, and C2 parent_of C1
        c(vec![
            w(true, 0, A_PARENT_OF, 1, Some(2)),
            w(true, 1, A_PARENT_OF, 0, Some(2)),
        ]),
        // edge properties: on edge#1, on an edge staged in this txn, and an
        // event property retract (error), then a bogus edge (error)
        c(vec![
            ew(true, 0, 0, 1, Some(-4), false),
            ew(true, 0, 0, 2, Some(-2), false),
            ew(false, 0, 0, 1, Some(-3), false),
        ]),
        c(vec![
            w(true, 4, A_LOC, 0, Some(1)),
            ew(true, 63, 4, 1, Some(1), false), // `year` is an event
            ew(false, 63, 4, 1, Some(1), false), // retract event: error, aborts
        ]),
        c(vec![ew(true, 0, 1, 0, Some(0), true)]),
        // aborted txn staging a new edge: the edge ID must be reused later
        c(vec![
            w(true, 5, A_FRIEND, 4, Some(9)),
            w(true, 5, 99, 0, Some(9)), // attr index wraps to a valid one: fine
        ]),
        TxnSpec {
            mode: Mode::Drop,
            default: None,
            ops: vec![w(true, 4, A_FRIEND, 0, Some(11))],
            check: true,
        },
        c(vec![w(true, 4, A_FRIEND, 1, Some(12))]),
    ];
    let mut sys = setup();
    for (i, s) in script.iter().enumerate() {
        let label = format!("graph script txn#{i}");
        sys.step(s, &label);
        if s.check {
            sys.compare_all(&label);
        }
    }
    sys.reopen();
    sys.compare_all("graph script after reopen");
    // generator sanity: edges exist, with edge-subject facts and both sides
    assert!(sys.m.edges.len() >= 6, "edges: {:?}", sys.m.edges);
    assert!(sys.m.facts.iter().any(|f| f.e.0 == EDGE_T));
    assert!(
        sys.m
            .facts
            .iter()
            .any(|f| ATTRS[f.a].bidi && f.e == (0, 1) && f.v == MV::Ref((0, 2)))
    );
    // C2~C1 is one edge#1 with 3 stored changes? assert -5, retract -1 only
    assert_eq!(sys.m.edge_seq((0, 2), A_FRIEND, (0, 1)), Some(1));
    assert_eq!(sys.m.edge_seq((0, 1), A_FRIEND, (0, 2)), Some(1));
    let stored = sys
        .m
        .facts
        .iter()
        .filter(|f| f.a == A_FRIEND && f.e == (0, 1) && f.v == MV::Ref((0, 2)))
        .count();
    assert_eq!(stored, 2, "assert -5 and retract -1; the rest were no-ops");
}

/// A fixed script through every correction path and its errors, with
/// overlaps appearing, moving and resolving (visible entities: 0 C1, 1 C2,
/// 2 C3, 3 P1, 4 P2, 5 I1; chain k = the k-th stored base fact).
fn correction_script() -> Vec<TxnSpec> {
    const A_SPOUSE: usize = 11;
    const A_RULED_BY: usize = 12;
    let w = |assert, ent, attr, val, tick| OpSpec::Write {
        assert,
        ent,
        attr,
        val,
        tick: Some(tick),
        defect: 0,
    };
    let c = |ops: Vec<OpSpec>| TxnSpec {
        mode: Mode::Commit,
        default: None,
        ops,
        check: true,
    };
    #[allow(clippy::too_many_arguments)]
    let cr = |pick, which, delete, value, tick, op, defect, collide| TxnSpec {
        mode: Mode::Correct,
        default: None,
        ops: vec![OpSpec::Correct {
            pick,
            which,
            delete,
            value,
            tick,
            op,
            defect,
            collide,
        }],
        check: true,
    };
    vec![
        // chain 0..2: C1 loc P1 @200, loc P2 @601 (overlap), retract P1 @250 (resolves it)
        c(vec![w(true, 0, A_LOC, 3, 200)]),
        c(vec![w(true, 0, A_LOC, 4, 601)]),
        c(vec![w(false, 0, A_LOC, 3, 250)]),
        // move the retract from 250 to 700: the overlap [601, 700) appears
        cr(2, 0, false, None, Some(700), None, 0, false),
        // aim chain 0 (assert P1 @200) at the retract's tick 700: opposite op -> conflict
        cr(0, 0, false, None, None, None, 0, true),
        // ... the same op there (retract @700) -> only the DELETED is written
        cr(0, 0, false, None, None, Some(false), 0, true),
        // chain 0 is deleted now: correcting it, or its Deleted marker, fails
        cr(0, 0, true, None, None, None, 0, false),
        cr(0, 3, true, None, None, None, 0, false),
        // unknown entry, a define entry, an invalid replacement
        cr(1, 0, false, None, None, None, 1, false),
        cr(1, 0, false, None, None, None, 2, false),
        cr(1, 0, false, Some(1), None, None, 3, false),
        // reverse side: C1 rules P1 @1 and P2 @2 (chains 3, 4) -> a reverse overlap
        c(vec![w(true, 3, A_RULED_BY, 0, 1)]),
        c(vec![w(true, 4, A_RULED_BY, 0, 2)]),
        // delete one of them: the overlap is resolved
        cr(4, 0, true, None, None, None, 0, false),
        // bidirectional: C2~C1 @5, C1~C3 @6 (chains 5, 6): C1 has two partners
        c(vec![w(true, 1, A_SPOUSE, 0, 5)]),
        c(vec![w(true, 0, A_SPOUSE, 2, 6)]),
        // re-pair chain 6 with C2 (smaller than... same pair as chain 5), then with I1
        cr(6, 0, false, Some(1), None, None, 0, false),
        cr(6, 0, false, Some(5), Some(3), None, 0, false),
        // an op flip at the same key: retract chain 5's pair
        cr(5, 0, false, None, None, Some(false), 0, false),
        cr(5, 0, false, None, None, Some(true), 0, false),
        // an edge property outside its edge's life (doctor warns), then moved inside
        c(vec![OpSpec::EdgeWrite {
            assert: true,
            edge: 0,
            attr: 0,
            val: 1,
            tick: Some(-5),
            bogus: false,
        }]),
        cr(7, 0, false, None, Some(300), None, 0, false),
        // no-op replace
        cr(7, 0, false, None, Some(300), None, 0, false),
    ]
}

#[test]
fn fixed_correction_script_covers_every_correction_path_against_the_model() {
    run_with(correction_script(), vec![], vec![0, 3, 7, 12]);
}

/// The generator really reaches the corrections, errors, overlaps and
/// findings the property is about (different seed from the property).
#[test]
fn the_generator_exercises_corrections_overlaps_and_doctor_findings() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::{Config, TestRunner};
    let mut runner = TestRunner::new(Config {
        rng_seed: RngSeed::Fixed(0x6b61_6c61_0001),
        ..Config::default()
    });
    let strat = (
        prop::collection::vec(txn_strategy(), 0..=28),
        prop::collection::vec(txn_strategy(), 0..=12),
        prop::collection::vec(any::<u64>(), 0..=4),
    );
    run_with(correction_script(), vec![], vec![1, 5, 9]);
    for _ in 0..48 {
        let (b1, b2, c) = strat.new_tree(&mut runner).unwrap().current();
        run_with(b1, b2, c);
    }
    let mut missing = vec![];
    for (i, name) in stats::NAMES.iter().enumerate() {
        eprintln!("stat {name}: {}", stats::get(i));
        if stats::get(i) == 0 {
            missing.push(*name);
        }
    }
    assert!(missing.is_empty(), "never reached: {missing:?}");
}
