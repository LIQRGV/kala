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

use kala_core::*;
use proptest::prelude::*;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

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
}

const ATTRS: [A; 12] = [
    A {
        name: "loc",
        ty: ValueType::Ref,
        event: false,
        indexed: true,
        many: false,
        bidi: false,
        inverse: None,
    },
    A {
        name: "allies",
        ty: ValueType::Ref,
        event: false,
        indexed: true,
        many: true,
        bidi: false,
        inverse: None,
    },
    A {
        name: "name",
        ty: ValueType::Str,
        event: false,
        indexed: true,
        many: false,
        bidi: false,
        inverse: None,
    },
    A {
        name: "age",
        ty: ValueType::I64,
        event: false,
        indexed: true,
        many: true,
        bidi: false,
        inverse: None,
    },
    A {
        name: "alive",
        ty: ValueType::Bool,
        event: false,
        indexed: true,
        many: false,
        bidi: false,
        inverse: None,
    },
    A {
        name: "height",
        ty: ValueType::F64,
        event: false,
        indexed: true,
        many: false,
        bidi: false,
        inverse: None,
    },
    A {
        name: "bought",
        ty: ValueType::Ref,
        event: true,
        indexed: true,
        many: true,
        bidi: false,
        inverse: None,
    },
    A {
        name: "year",
        ty: ValueType::I64,
        event: true,
        indexed: true,
        many: false,
        bidi: false,
        inverse: None,
    },
    A {
        name: "bio",
        ty: ValueType::Str,
        event: false,
        indexed: false,
        many: false,
        bidi: false,
        inverse: None,
    },
    A {
        name: "friend_of",
        ty: ValueType::Ref,
        event: false,
        indexed: true,
        many: true,
        bidi: true,
        inverse: None,
    },
    A {
        name: "parent_of",
        ty: ValueType::Ref,
        event: false,
        indexed: true,
        many: true,
        bidi: false,
        inverse: Some("child_of"),
    },
    A {
        name: "spouse",
        ty: ValueType::Ref,
        event: false,
        indexed: true,
        many: false,
        bidi: true,
        inverse: None,
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
        s = s.attribute(d);
    }
    s
}

// ----------------------------------------------------------------- model

type Ent = (usize, u64); // (node type index, sequence number)

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug)]
struct MFact {
    e: Ent,
    a: usize,
    v: MV,
    tick: i64,
    tx: u64,
    assert: bool,
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
}

impl Model {
    fn edge_seq(&self, src: Ent, a: usize, tgt: Ent) -> Option<u64> {
        let (src, tgt) = norm(a, src, tgt);
        self.edges
            .iter()
            .position(|(s, x, t, _)| (*s, *x, *t) == (src, a, tgt))
            .map(|i| i as u64 + 1)
    }

    fn latest_tick(&self) -> i64 {
        self.facts.iter().map(|f| f.tick).max().unwrap_or(0)
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
        let same: Vec<&MFact> = m
            .facts
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
        });
        Ok((1, tick))
    }

    /// Commit into the model; `Err(Aborted)` if poisoned.
    fn commit(self, m: &mut Model) -> Result<(u64, usize), EK> {
        if self.poisoned {
            return Err(EK::Aborted);
        }
        let changed = self.staged.len();
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
        for mut f in self.staged {
            f.tx = m.tx;
            m.facts.push(f);
        }
        Ok((m.tx, changed))
    }
}

/// The facts a read of `(e, a)` at canon `c` is built from. For a
/// bidirectional attribute that is both stored sides, re-keyed so the entity
/// is the subject and the partner is the `Ref` value (a self pair once).
fn m_view(m: &Model, c: u64, e: Ent, a: usize) -> Vec<MFact> {
    let mut out = vec![];
    for f in m.facts.iter().filter(|f| f.tx <= c && f.a == a) {
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
    for f in m.facts.iter().filter(|f| f.tx <= c && f.a == a) {
        let MV::Ref(tgt) = f.v else { continue };
        if (want_out && f.e == x) || (want_in && tgt == x) {
            keys.push((f.e, tgt));
        }
    }
    keys.sort();
    keys.dedup();
    let mut out = std::collections::BTreeSet::new();
    for (src, tgt) in keys {
        let held = m
            .facts
            .iter()
            .filter(|f| f.tx <= c && f.a == a && f.e == src && f.v == MV::Ref(tgt) && f.tick <= t)
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
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    Commit,
    Drop,
    Auto,
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

fn txn_strategy() -> impl Strategy<Value = TxnSpec> {
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
        let before_tx = self.m.tx;
        let ops: Vec<OpSpec> = match spec.mode {
            Mode::Auto => vec![spec.ops[0].clone()],
            _ => spec.ops.clone(),
        };
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
                                assert_eq!((c, Tick(t)), (r.changed, r.tick), "{ctx}: write result")
                            }
                            (Err(k), Err(err)) => assert_eq!(k, ek(&err), "{ctx}: {err:?}"),
                            (e, g) => panic!("{ctx}: model {e:?} vs real {g:?}"),
                        }
                    }
                    OpSpec::Write { .. } => {
                        let (mw, attr, subj, val) = self.resolve_write(&mt, op);
                        let exp = mt.write(&self.m, &mw);
                        let tick = mw.tick.map(Tick);
                        let got = tgt.write(mw.assert, subj, &attr, val, tick);
                        match (exp, got) {
                            (Ok((c, t)), Ok(r)) => {
                                assert_eq!((c, Tick(t)), (r.changed, r.tick), "{ctx}: write result")
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
        match spec.mode {
            Mode::Commit => {
                let exp = mt.commit(&mut self.m);
                match (exp, commit_result.unwrap()) {
                    (Ok((tx, ch)), Ok(r)) => {
                        assert_eq!((TxId(tx), ch), (r.tx, r.changed), "{label}: commit result")
                    }
                    (Err(k), Err(err)) => assert_eq!(k, ek(&err), "{label}: commit {err:?}"),
                    (e, g) => panic!("{label}: commit: model {e:?} vs real {g:?}"),
                }
            }
            Mode::Drop => {
                let _ = &mt; // discarded: nothing applies, no tx, IDs and handles free
            }
            Mode::Auto => {
                // an auto-commit applies its one write if it succeeded
                let _ = mt.commit(&mut self.m);
            }
        }
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
        canons.dedup();
        self.compare_graph(label, &canons);
        for c in canons {
            let view = w.at_canon(TxId(c));
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
            let view = w.at_canon(TxId(c));

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
                        ticks.extend([f.tick.saturating_sub(1), f.tick, f.tick.saturating_add(1)]);
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

    fn reopen(&mut self) {
        drop(self.world.take());
        let w = World::open(&self.path()).expect("reopen");
        self.world = Some(w);
        // entity IDs are plain values: they stay valid across reopen
    }
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
    let mut sys = setup();
    sys.compare_all("setup");
    for (i, s) in batch1.iter().enumerate() {
        let label = format!("b1 txn#{i} {:?}", s.mode);
        sys.step(s, &label);
        if s.check {
            sys.compare_all(&label);
        }
    }
    sys.compare_all("end of batch 1");
    sys.reopen();
    sys.compare_all("after reopen 1");
    for (i, s) in batch2.iter().enumerate() {
        let label = format!("b2 txn#{i} {:?}", s.mode);
        sys.step(s, &label);
        if s.check {
            sys.compare_all(&label);
        }
    }
    sys.compare_all("end of batch 2");
    sys.reopen();
    sys.compare_all("after reopen 2");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

    #[test]
    fn world_matches_the_naive_model_before_and_after_reopen(
        batch1 in prop::collection::vec(txn_strategy(), 0..=28),
        batch2 in prop::collection::vec(txn_strategy(), 0..=12),
    ) {
        run(batch1, batch2);
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
