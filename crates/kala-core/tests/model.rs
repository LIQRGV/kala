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
}

const ATTRS: [A; 9] = [
    A {
        name: "loc",
        ty: ValueType::Ref,
        event: false,
        indexed: true,
        many: false,
    },
    A {
        name: "allies",
        ty: ValueType::Ref,
        event: false,
        indexed: true,
        many: true,
    },
    A {
        name: "name",
        ty: ValueType::Str,
        event: false,
        indexed: true,
        many: false,
    },
    A {
        name: "age",
        ty: ValueType::I64,
        event: false,
        indexed: true,
        many: true,
    },
    A {
        name: "alive",
        ty: ValueType::Bool,
        event: false,
        indexed: true,
        many: false,
    },
    A {
        name: "height",
        ty: ValueType::F64,
        event: false,
        indexed: true,
        many: false,
    },
    A {
        name: "bought",
        ty: ValueType::Ref,
        event: true,
        indexed: true,
        many: true,
    },
    A {
        name: "year",
        ty: ValueType::I64,
        event: true,
        indexed: true,
        many: false,
    },
    A {
        name: "bio",
        ty: ValueType::Str,
        event: false,
        indexed: false,
        many: false,
    },
];
const A_LOC: usize = 0;
const A_NAME: usize = 2;
const A_HEIGHT: usize = 5;

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
    tx: u64,
}

impl Model {
    fn latest_tick(&self) -> i64 {
        self.facts.iter().map(|f| f.tick).max().unwrap_or(0)
    }
}

/// A transaction as the model sees it.
struct MTxn {
    base_tick: i64,
    default: Option<i64>,
    staged: Vec<MFact>,
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
            staged_ents: vec![],
            staged_handles: vec![],
            poisoned: false,
        }
    }

    fn defined(&self, m: &Model, e: Ent) -> bool {
        m.ents.iter().any(|(x, _)| *x == e) || self.staged_ents.contains(&e)
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
        for mut f in self.staged {
            f.tx = m.tx;
            m.facts.push(f);
        }
        Ok((m.tx, changed))
    }
}

/// Model answers (canon `c`).
fn m_holds(m: &Model, c: u64, e: Ent, a: usize, v: &MV, t: i64) -> bool {
    m.facts
        .iter()
        .filter(|f| f.tx <= c && f.e == e && f.a == a && f.v == *v && f.tick <= t)
        .max_by_key(|f| f.tick)
        .is_some_and(|f| f.assert)
}

fn m_as_of(m: &Model, c: u64, e: Ent, a: usize, t: i64) -> Vec<MV> {
    let mut vals: Vec<MV> = vec![];
    for f in m.facts.iter().filter(|f| f.tx <= c && f.e == e && f.a == a) {
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
    let mut fs: Vec<&MFact> = m
        .facts
        .iter()
        .filter(|f| f.tx <= c && f.e == e && f.a == a)
        .collect();
    fs.sort_by(|x, y| x.tick.cmp(&y.tick).then_with(|| cmp_mv(&x.v, &y.v)));
    fs.into_iter()
        .map(|f| (f.tick, f.assert, f.v.clone()))
        .collect()
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
