//! Random interleavings of transactions on three handles of one world file
//! (slice 15b, stage 3), against a reference: a second world file written
//! by ONE writer, to which every transaction that commits is applied again,
//! whole, in commit order.
//!
//! The reference is told only what the specification says a commit means:
//!
//! - the transaction's operations, in order, as the caller made them;
//! - a tick-less write at the latest tick the transaction **began** with;
//! - entities named the way the caller named them ("the second entity this
//!   transaction created"), so the reference allocates its own IDs.
//!
//! After every commit the shared file must equal the reference file byte
//! for byte (same entries, entry IDs, tx numbers, edge IDs, checksums), the
//! commit result must equal the reference's, and the reported ID mapping
//! must send each provisional ID to the ID the reference allocated. If the
//! reference refuses an operation, the commit must fail with that same
//! error and write nothing.
//!
//! Reads are judged by this file's own matcher, written from the rules in
//! CLAUDE.md, not by kala-core's: an optimistic commit must be a
//! `Conflict` exactly when a transaction committed since it began wrote
//! through a key it recorded (never otherwise: the read sets here stay
//! under the bound), and a weak one never is.
//!
//! Each handle lives on its own thread (a `Txn` borrows its `World`); the
//! test drives them one command at a time, so every run is deterministic.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

use kala_core::doctor::{DoctorOptions, doctor, doctor_log};
use kala_core::log::EntryBody;
use kala_core::*;
use proptest::prelude::*;

const HANDLES: usize = 3;

/// What the runs reached, so a broken generator can't make the property
/// vacuous (checked by `the_generator_reaches_every_outcome`).
mod stats {
    use std::sync::atomic::{AtomicUsize, Ordering};

    macro_rules! counters {
        ($($name:ident),*) => {
            $(pub static $name: AtomicUsize = AtomicUsize::new(0);)*
        };
    }
    counters!(
        OVER_NEWER,
        CONFLICTS,
        WEAK_OVER_INVALIDATED_READ,
        EXPOSED,
        STORE_ERRORS,
        RENUMBERED,
        EDGES_MOVED,
        TICK_MOVED,
        BECAME_NOOP,
        READ_ONLY
    );

    pub fn bump(counter: &AtomicUsize) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::Relaxed)
    }
}
const TYPES: [&str; 2] = ["CHARACTER", "PLACE"];
/// Attributes a plain value is written to, then the `ref` ones.
const VALUE_ATTRS: [&str; 2] = ["tag", "name"];
const REF_ATTRS: [&str; 2] = ["located_in", "ally_of"];

fn schema() -> Schema {
    let attr = AttrDef::new;
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(attr("tag", ValueType::Str, Kind::State, Cardinality::Many))
        .attribute(attr("name", ValueType::Str, Kind::State, Cardinality::One))
        .attribute(attr("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(attr("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional())
}

/// A new world with five entities and a few facts; returns the entities.
fn seed(path: &Path) -> (World, Vec<EntityId>) {
    let mut w = World::create(path, schema()).unwrap();
    let mut t = w.begin();
    let base: Vec<EntityId> = [
        ("CHARACTER", "greg"),
        ("CHARACTER", "edwin"),
        ("CHARACTER", "mara"),
        ("PLACE", "varrow"),
        ("PLACE", "ashford"),
    ]
    .into_iter()
    .map(|(ty, handle)| t.create_entity(ty, Some(handle)).unwrap())
    .collect();
    t.assert(base[0], "located_in", Value::Ref(base[3]), Some(Tick(1)))
        .unwrap();
    t.assert(base[1], "located_in", Value::Ref(base[3]), Some(Tick(2)))
        .unwrap();
    t.assert(base[1], "ally_of", Value::Ref(base[2]), Some(Tick(1)))
        .unwrap();
    t.assert(base[0], "tag", text(0), Some(Tick(3))).unwrap();
    t.commit().unwrap();
    (w, base)
}

fn text(n: u8) -> Value {
    Value::Str(format!("t{n}"))
}

// ------------------------------------------------------------ the program

/// An entity as a transaction names it.
#[derive(Clone, Copy, Debug)]
enum Ent {
    /// One of the seeded entities.
    Base(usize),
    /// The k-th entity this transaction allocated (create or find-or-create).
    Made(usize),
}

#[derive(Clone, Copy, Debug)]
enum Subject {
    Ent(Ent),
    /// The edge of a `ref` fact, as the transaction can resolve it.
    Edge(Ent, usize, Ent),
}

#[derive(Clone, Copy, Debug)]
enum Val {
    Text(u8),
    Ref(Ent),
}

#[derive(Clone, Copy, Debug)]
enum Fix {
    Delete,
    MoveTo(i64),
}

#[derive(Clone, Copy, Debug)]
enum TxOp {
    Create {
        ty: usize,
        handle: Option<u8>,
    },
    FindOrCreate {
        ty: usize,
        handle: u8,
    },
    Write {
        subject: Subject,
        /// An index into `VALUE_ATTRS` (a text value) or `REF_ATTRS`.
        attr: usize,
        value: Val,
        tick: Option<i64>,
        retract: bool,
    },
    Correct {
        pick: usize,
        fix: Fix,
    },
    Read(ReadOp),
}

#[derive(Clone, Copy, Debug)]
enum ReadOp {
    Forward(Ent, usize, bool),
    Reverse(Ent, usize),
    Value(usize, u8),
    Type(usize),
    Attribute(usize, bool),
    /// `Txn::values_at`: the read `[:set …]` makes.
    ValuesAt(Ent, usize),
}

#[derive(Clone, Copy, Debug)]
enum Step {
    Begin { h: usize, weak: bool, exposed: bool },
    Op { h: usize, op: TxOp },
    Commit { h: usize },
    Drop { h: usize },
    CatchUp { h: usize },
}

fn ent() -> impl Strategy<Value = Ent> {
    prop_oneof![
        (0..5usize).prop_map(Ent::Base),
        (0..3usize).prop_map(Ent::Made)
    ]
}

fn tick() -> impl Strategy<Value = Option<i64>> {
    prop_oneof![2 => Just(None), 3 => (0..6i64).prop_map(Some)]
}

fn write_op() -> impl Strategy<Value = TxOp> {
    let subject = prop_oneof![
        4 => ent().prop_map(Subject::Ent),
        1 => (ent(), 0..2usize, ent()).prop_map(|(s, a, t)| Subject::Edge(s, a, t)),
    ];
    let value = prop_oneof![(0..3u8).prop_map(Val::Text), ent().prop_map(Val::Ref)];
    (subject, 0..2usize, value, tick(), prop::bool::weighted(0.3)).prop_map(
        |(subject, attr, value, tick, retract)| TxOp::Write {
            subject,
            attr,
            value,
            tick,
            retract,
        },
    )
}

fn read_op() -> impl Strategy<Value = ReadOp> {
    prop_oneof![
        (ent(), 0..2usize, any::<bool>()).prop_map(|(e, a, r)| ReadOp::Forward(e, a, r)),
        (ent(), 0..2usize).prop_map(|(e, a)| ReadOp::Reverse(e, a)),
        (0..2usize, 0..3u8).prop_map(|(a, v)| ReadOp::Value(a, v)),
        (0..2usize).prop_map(ReadOp::Type),
        (0..2usize, any::<bool>()).prop_map(|(a, r)| ReadOp::Attribute(a, r)),
        (ent(), 0..2usize).prop_map(|(e, a)| ReadOp::ValuesAt(e, a)),
    ]
}

fn tx_op() -> impl Strategy<Value = TxOp> {
    prop_oneof![
        2 => (0..2usize, prop::option::of(0..3u8)).prop_map(|(ty, handle)| TxOp::Create { ty, handle }),
        1 => (0..2usize, 0..3u8).prop_map(|(ty, handle)| TxOp::FindOrCreate { ty, handle }),
        8 => write_op(),
        1 => (any::<usize>(), prop_oneof![Just(Fix::Delete), (0..6i64).prop_map(Fix::MoveTo)])
            .prop_map(|(pick, fix)| TxOp::Correct { pick, fix }),
        2 => read_op().prop_map(TxOp::Read),
    ]
}

fn step() -> impl Strategy<Value = Step> {
    let h = 0..HANDLES;
    prop_oneof![
        3 => (h.clone(), prop::bool::weighted(0.3), prop::bool::weighted(0.15))
            .prop_map(|(h, weak, exposed)| Step::Begin { h, weak, exposed }),
        12 => (h.clone(), tx_op()).prop_map(|(h, op)| Step::Op { h, op }),
        5 => h.clone().prop_map(|h| Step::Commit { h }),
        1 => h.clone().prop_map(|h| Step::Drop { h }),
        1 => h.prop_map(|h| Step::CatchUp { h }),
    ]
}

// ------------------------------------------------------------ the handles

enum Cmd {
    CatchUp,
    Begin { weak: bool, exposed: bool },
    Op(TxOp),
    Commit,
    Drop,
}

#[derive(Debug)]
enum Reply {
    Done,
    Began {
        base: Tick,
    },
    /// A write or correction: how much it staged, and the edge a stored
    /// `ref` assert uses (as the transaction sees it).
    Wrote {
        changed: usize,
        edge: Option<EdgeId>,
    },
    Made {
        id: EntityId,
        created: bool,
    },
    /// The operation couldn't be made (its edge doesn't exist here).
    Skipped,
    Failed(Error),
    Committed(Box<Result<CommitResult, Error>>),
    /// The handle's state once its thread ends.
    Final(Box<(IndexDump, Vec<EntryInfo>, bool)>),
}

fn name_of(attr: usize, value: Val) -> &'static str {
    match value {
        Val::Text(_) => VALUE_ATTRS[attr],
        Val::Ref(_) => REF_ATTRS[attr],
    }
}

/// How a transaction resolves the entities and values of an operation:
/// shared by the handles and the reference, which differ only in `made`.
struct Names<'a> {
    base: &'a [EntityId],
    made: &'a [EntityId],
}

impl Names<'_> {
    fn ent(&self, e: Ent) -> EntityId {
        match e {
            Ent::Base(i) => self.base[i],
            Ent::Made(k) => self.made[k],
        }
    }

    fn subject(&self, txn: &Txn<'_>, s: Subject) -> Option<EntityId> {
        match s {
            Subject::Ent(e) => Some(self.ent(e)),
            Subject::Edge(source, attr, target) => txn
                .edge_id(self.ent(source), REF_ATTRS[attr], self.ent(target))
                .map(EntityId::from),
        }
    }

    fn value(&self, v: Val) -> Value {
        match v {
            Val::Text(n) => text(n),
            Val::Ref(e) => Value::Ref(self.ent(e)),
        }
    }
}

/// Makes one write operation in `txn`: what a handle does at its snapshot
/// (`at` = the latest tick it began with is implied by `None`), and what
/// the reference does with every tick-less write pinned to `pinned`.
fn make(
    txn: &mut Txn<'_>,
    names: &Names<'_>,
    op: TxOp,
    pinned: Option<Tick>,
    facts: &[EntryId],
) -> Reply {
    let done = |r: Result<WriteResult, Error>, edge| match r {
        Ok(w) => Reply::Wrote {
            changed: w.changed,
            edge,
        },
        Err(e) => Reply::Failed(e),
    };
    match op {
        TxOp::Create { ty, handle } => {
            let handle = handle.map(|h| format!("h{h}"));
            match txn.create_entity(TYPES[ty], handle.as_deref()) {
                Ok(id) => Reply::Made { id, created: true },
                Err(e) => Reply::Failed(e),
            }
        }
        TxOp::FindOrCreate { ty, handle } => {
            match txn.find_or_create_entity(TYPES[ty], &format!("h{handle}")) {
                Ok((id, created)) => Reply::Made { id, created },
                Err(e) => Reply::Failed(e),
            }
        }
        TxOp::Write {
            subject,
            attr,
            value,
            tick,
            retract,
        } => {
            let Some(entity) = names.subject(txn, subject) else {
                return Reply::Skipped;
            };
            let attr = name_of(attr, value);
            let v = names.value(value);
            let tick = tick.map(Tick).or(pinned);
            let result = if retract {
                txn.retract(entity, attr, v.clone(), tick)
            } else {
                txn.assert(entity, attr, v.clone(), tick)
            };
            let edge = match (&v, retract) {
                (Value::Ref(target), false) => txn.edge_id(entity, attr, *target),
                _ => None,
            };
            done(result, edge)
        }
        TxOp::Correct { pick, fix } => {
            if facts.is_empty() {
                return Reply::Skipped;
            }
            let entry = facts[pick % facts.len()];
            let correction = match fix {
                Fix::Delete => Correction::Delete,
                Fix::MoveTo(tick) => Correction::Replace {
                    value: None,
                    tick: Some(Tick(tick)),
                    op: None,
                    precision: None,
                },
            };
            done(txn.correct(entry, correction), None)
        }
        TxOp::Read(_) => unreachable!("reads are made by `read`"),
    }
}

fn read(txn: &Txn<'_>, names: &Names<'_>, op: ReadOp, base_tick: Tick) {
    let pick = |attr: usize, is_ref: bool| {
        if is_ref {
            REF_ATTRS[attr]
        } else {
            VALUE_ATTRS[attr]
        }
    };
    match op {
        ReadOp::Forward(e, attr, is_ref) => {
            txn.note_read(names.ent(e), pick(attr, is_ref)).unwrap();
        }
        ReadOp::Reverse(e, attr) => txn
            .note_reverse_read(names.ent(e), REF_ATTRS[attr])
            .unwrap(),
        ReadOp::Value(attr, v) => txn.note_value_read(VALUE_ATTRS[attr], &text(v)).unwrap(),
        ReadOp::Type(ty) => txn.note_type_scan(TYPES[ty]).unwrap(),
        ReadOp::Attribute(attr, is_ref) => txn.note_read_attribute(pick(attr, is_ref)).unwrap(),
        ReadOp::ValuesAt(e, attr) => {
            let meter = WorkMeter::bounded(100_000, None);
            txn.values_at(
                names.ent(e),
                REF_ATTRS[attr],
                base_tick,
                64,
                1 << 20,
                &meter,
            )
            .unwrap();
        }
    }
}

/// One handle: its own `World` on the shared file, driven by commands.
fn handle(path: PathBuf, base: Vec<EntityId>, commands: Receiver<Cmd>, replies: Sender<Reply>) {
    let mut world = World::open(&path).unwrap();
    let send = |r: Reply| replies.send(r).unwrap();
    // Opened: the driver waits for this before its first step (see `run`).
    send(Reply::Done);
    while let Ok(cmd) = commands.recv() {
        match cmd {
            Cmd::CatchUp => {
                world.catch_up().unwrap();
                send(Reply::Done);
            }
            Cmd::Begin { weak, exposed } => {
                world.catch_up().unwrap();
                let base_tick = world.latest_tick();
                let facts: Vec<EntryId> = correctable(&world);
                let mut txn = world.begin();
                if weak {
                    txn.set_concurrency(Concurrency::Weak);
                }
                txn.set_ids_exposed(exposed);
                send(Reply::Began { base: base_tick });
                let mut made = Vec::new();
                loop {
                    match commands.recv().unwrap() {
                        Cmd::Op(TxOp::Read(op)) => {
                            let names = Names {
                                base: &base,
                                made: &made,
                            };
                            read(&txn, &names, op, base_tick);
                            send(Reply::Done);
                        }
                        Cmd::Op(op) => {
                            let names = Names {
                                base: &base,
                                made: &made,
                            };
                            let reply = make(&mut txn, &names, op, None, &facts);
                            if let Reply::Made { id, .. } = reply {
                                made.push(id);
                            }
                            send(reply);
                        }
                        Cmd::Commit => {
                            send(Reply::Committed(Box::new(txn.commit())));
                            break;
                        }
                        Cmd::Drop => {
                            drop(txn);
                            send(Reply::Done);
                            break;
                        }
                        Cmd::CatchUp | Cmd::Begin { .. } => unreachable!("driver bug"),
                    }
                }
            }
            Cmd::Op(_) | Cmd::Commit | Cmd::Drop => unreachable!("driver bug"),
        }
    }
    world.catch_up().unwrap();
    let clean = doctor(&world, &DoctorOptions::default())
        .errors()
        .is_empty();
    send(Reply::Final(Box::new((
        world.index_dump(),
        world.entries().collect(),
        clean,
    ))));
}

fn edge_count(w: &World) -> u64 {
    w.entries()
        .filter(|e| matches!(e.body, EntryBody::DefineEdge { .. }))
        .count() as u64
}

/// The entries a correction can name, in log order.
fn correctable(w: &World) -> Vec<EntryId> {
    w.entries()
        .filter(|e| matches!(e.body, EntryBody::Fact { .. } | EntryBody::Revised { .. }))
        .map(|e| e.id)
        .collect()
}

// ------------------------------------------------------------ the read rules

/// A key a transaction recorded, in this file's own terms.
#[derive(Clone, Debug)]
enum Key {
    Forward(EntityId, &'static str),
    Reverse(EntityId, &'static str),
    Value(&'static str, Value),
    Attribute(&'static str),
    Type(&'static str),
    Handle(String),
    /// The chain of this base entry.
    Chain(EntryId),
}

/// What one committed transaction wrote, as its log entries say it.
#[derive(Clone, Debug)]
enum Written {
    Fact {
        entity: EntityId,
        attr: String,
        value: Value,
        /// The base entry, for a correction.
        of: Option<EntryId>,
    },
    Entity {
        node_type: String,
        handle: Option<String>,
    },
}

fn written(w: &World, tx: TxId) -> Vec<Written> {
    w.entries()
        .filter(|e| e.tx == tx)
        .filter_map(|e| {
            let of = e.revision().map(|(of, _)| of);
            match e.body {
                EntryBody::Fact {
                    entity,
                    attr,
                    value,
                    ..
                }
                | EntryBody::Deleted {
                    entity,
                    attr,
                    value,
                    ..
                }
                | EntryBody::Revised {
                    entity,
                    attr,
                    value,
                    ..
                } => Some(Written::Fact {
                    entity,
                    attr: w.attr_name(attr).unwrap().to_owned(),
                    value,
                    of,
                }),
                EntryBody::DefineEntity { id, handle } => Some(Written::Entity {
                    node_type: w.format_id(id).split('#').next().unwrap().to_owned(),
                    handle,
                }),
                _ => None,
            }
        })
        .collect()
}

/// CLAUDE.md, "The goal is serializable": does `entry` go through `key`?
fn invalidates(key: &Key, entry: &Written) -> bool {
    match (key, entry) {
        (Key::Chain(base), Written::Fact { of: Some(of), .. }) => base == of,
        (
            key,
            Written::Fact {
                entity,
                attr,
                value,
                ..
            },
        ) => {
            // A bidirectional pair is stored once: either end reads it.
            let both = attr == "ally_of";
            let points_at = |e: &EntityId| *value == Value::Ref(*e);
            match key {
                Key::Forward(e, a) => a == attr && (entity == e || (both && points_at(e))),
                Key::Reverse(t, a) => a == attr && (points_at(t) || (both && entity == t)),
                Key::Value(a, v) => a == attr && v == value,
                Key::Attribute(a) => a == attr,
                Key::Type(_) | Key::Handle(_) | Key::Chain(_) => false,
            }
        }
        (Key::Type(ty), Written::Entity { node_type, .. }) => ty == node_type,
        (Key::Handle(h), Written::Entity { handle, .. }) => handle.as_ref() == Some(h),
        (_, Written::Entity { .. }) => false,
    }
}

// ------------------------------------------------------------ the driver

/// A transaction in progress on one handle, as the driver knows it.
struct Open {
    weak: bool,
    exposed: bool,
    /// The latest tick it began with.
    base_tick: Tick,
    /// How many transactions had been committed when it began.
    begun_at: usize,
    /// How many edges the world had when it began: an edge ID past it is
    /// one the transaction interned itself.
    edges_at_begin: u64,
    /// The correctable entries when it began.
    facts: Vec<EntryId>,
    /// The base entry of each of them.
    bases: Vec<EntryId>,
    /// The write operations it made (skipped ones left out), each with the
    /// edge its handle reported.
    ops: Vec<(TxOp, Option<EdgeId>)>,
    keys: Vec<Key>,
    /// The provisional IDs of the entities it allocated.
    made: Vec<EntityId>,
    /// An entity it allocated that existed at its snapshot isn't its own.
    made_new: Vec<bool>,
    poisoned: bool,
    /// Whether it staged anything (else its commit is a read-only one).
    staged: bool,
    tickless: bool,
}

struct Driver {
    path: PathBuf,
    base: Vec<EntityId>,
    reference: World,
    reference_path: PathBuf,
    /// What each committed transaction wrote, in commit order.
    commits: Vec<Vec<Written>>,
    commands: Vec<Sender<Cmd>>,
    replies: Vec<Receiver<Reply>>,
    open: Vec<Option<Open>>,
}

impl Driver {
    fn ask(&self, h: usize, cmd: Cmd) -> Reply {
        self.commands[h].send(cmd).unwrap();
        self.replies[h].recv().expect("a handle thread died")
    }

    fn files_agree(&self, what: &str) {
        assert!(
            std::fs::read(&self.path).unwrap() == std::fs::read(&self.reference_path).unwrap(),
            "{what}: the shared file differs from the reference"
        );
    }

    fn step(&mut self, step: Step) {
        match step {
            Step::Begin { h, weak, exposed } => {
                if self.open[h].is_some() {
                    return;
                }
                let Reply::Began { base } = self.ask(h, Cmd::Begin { weak, exposed }) else {
                    panic!("begin");
                };
                assert_eq!(base, self.reference.latest_tick());
                let facts = correctable(&self.reference);
                let bases = facts
                    .iter()
                    .map(|id| {
                        let entry = self.reference.entry(*id).unwrap();
                        entry.revision().map_or(*id, |(of, _)| of)
                    })
                    .collect();
                self.open[h] = Some(Open {
                    weak,
                    exposed,
                    base_tick: base,
                    begun_at: self.commits.len(),
                    edges_at_begin: edge_count(&self.reference),
                    facts,
                    bases,
                    ops: Vec::new(),
                    keys: Vec::new(),
                    made: Vec::new(),
                    made_new: Vec::new(),
                    poisoned: false,
                    staged: false,
                    tickless: false,
                });
            }
            Step::Op { h, op } => self.op(h, op),
            Step::Commit { h } => self.commit(h),
            Step::Drop { h } => {
                if self.open[h].take().is_some() {
                    self.ask(h, Cmd::Drop);
                    self.files_agree("a dropped transaction");
                }
            }
            Step::CatchUp { h } => {
                if self.open[h].is_none() {
                    self.ask(h, Cmd::CatchUp);
                }
            }
        }
    }

    /// Names only entities the transaction has: `Made(k)` wraps around, or
    /// falls back to a seeded entity.
    fn settle(open: &Open, e: Ent) -> Ent {
        match e {
            Ent::Made(_) if open.made.is_empty() => Ent::Base(0),
            Ent::Made(k) => Ent::Made(k % open.made.len()),
            base => base,
        }
    }

    fn op(&mut self, h: usize, op: TxOp) {
        let base = self.base.clone();
        if self.open[h].is_none() {
            // An operation on an idle handle begins a transaction, in a
            // mode that varies with how far the run is.
            let turn = h + self.commits.len();
            self.step(Step::Begin {
                h,
                weak: turn.is_multiple_of(3),
                exposed: turn.is_multiple_of(5),
            });
        }
        let Some(open) = self.open[h].as_mut() else {
            return;
        };
        if open.poisoned {
            return;
        }
        let fix = |e| Self::settle(open, e);
        let op = match op {
            TxOp::Write {
                subject,
                attr,
                value,
                tick,
                retract,
            } => TxOp::Write {
                subject: match subject {
                    Subject::Ent(e) => Subject::Ent(fix(e)),
                    Subject::Edge(s, a, t) => Subject::Edge(fix(s), a, fix(t)),
                },
                attr,
                value: match value {
                    Val::Ref(e) => Val::Ref(fix(e)),
                    text => text,
                },
                tick,
                retract,
            },
            TxOp::Read(read) => TxOp::Read(match read {
                ReadOp::Forward(e, a, r) => ReadOp::Forward(fix(e), a, r),
                ReadOp::Reverse(e, a) => ReadOp::Reverse(fix(e), a),
                ReadOp::ValuesAt(e, a) => ReadOp::ValuesAt(fix(e), a),
                other => other,
            }),
            other => other,
        };
        // The keys a read records. An entity only this transaction knows
        // can't have been written by anyone else: no key.
        if let TxOp::Read(read) = op {
            let pick = |attr: usize, is_ref: bool| {
                if is_ref {
                    REF_ATTRS[attr]
                } else {
                    VALUE_ATTRS[attr]
                }
            };
            let known = |e: Ent| match e {
                Ent::Base(i) => Some(base[i]),
                Ent::Made(k) => (!open.made_new[k]).then_some(open.made[k]),
            };
            let key = match read {
                ReadOp::Forward(e, a, r) => known(e).map(|e| Key::Forward(e, pick(a, r))),
                ReadOp::Reverse(e, a) => known(e).map(|e| Key::Reverse(e, REF_ATTRS[a])),
                ReadOp::ValuesAt(e, a) => known(e).map(|e| Key::Forward(e, REF_ATTRS[a])),
                ReadOp::Value(a, v) => Some(Key::Value(VALUE_ATTRS[a], text(v))),
                ReadOp::Type(ty) => Some(Key::Type(TYPES[ty])),
                ReadOp::Attribute(a, r) => Some(Key::Attribute(pick(a, r))),
            };
            open.keys.extend(key);
        }
        if let TxOp::Correct { pick, .. } = op
            && !open.facts.is_empty()
        {
            // A correction reads the chain it corrects.
            open.keys
                .push(Key::Chain(open.bases[pick % open.facts.len()]));
        }
        let reply = {
            self.commands[h].send(Cmd::Op(op)).unwrap();
            self.replies[h].recv().expect("a handle thread died")
        };
        let open = self.open[h].as_mut().unwrap();
        match reply {
            Reply::Done | Reply::Skipped => {}
            Reply::Failed(_) => open.poisoned = true,
            Reply::Made { id, created } => {
                if let TxOp::FindOrCreate { handle, .. } = op
                    && created
                {
                    // It found the handle free.
                    open.keys.push(Key::Handle(format!("h{handle}")));
                }
                open.made.push(id);
                open.made_new.push(created);
                open.staged |= created;
                open.ops.push((op, None));
            }
            Reply::Wrote { changed, edge } => {
                open.staged |= changed > 0;
                if let TxOp::Write { tick: None, .. } = op {
                    open.tickless = true;
                }
                open.ops.push((op, edge));
            }
            other => panic!("unexpected reply {other:?}"),
        }
    }

    fn commit(&mut self, h: usize) {
        let Some(open) = self.open[h].take() else {
            return;
        };
        let Reply::Committed(result) = self.ask(h, Cmd::Commit) else {
            panic!("commit");
        };
        let result = *result;
        let context = format!("handle {h}: {result:?}");
        if open.poisoned {
            assert_eq!(result, Err(Error::TransactionAborted), "{context}");
            return self.files_agree(&context);
        }
        if !open.staged {
            // Nothing staged at its snapshot: a read-only transaction,
            // serializable where it began. No lock, nothing written.
            let result = result.expect(&context);
            assert!(!result.committed && result.finalized.is_none(), "{context}");
            stats::bump(&stats::READ_ONLY);
            return self.files_agree(&context);
        }
        let since = &self.commits[open.begun_at..];
        let invalidated = open.keys.iter().any(|key| {
            since
                .iter()
                .any(|commit| commit.iter().any(|entry| invalidates(key, entry)))
        });
        if invalidated && !open.weak {
            assert!(
                matches!(&result, Err(Error::Conflict(c)) if c.key.kind() != "exposed-id"),
                "a read was invalidated, the commit must conflict: {context}\nkeys {:?}\nsince {since:?}",
                open.keys
            );
            stats::bump(&stats::CONFLICTS);
            return self.files_agree(&context);
        }
        if invalidated {
            stats::bump(&stats::WEAK_OVER_INVALIDATED_READ);
        }

        // The reference: the same operations by one writer, now.
        let edges_before = edge_count(&self.reference);
        let latest_before = self.reference.latest_tick();
        let mut expected: Result<(), Error> = Ok(());
        let mut exposed_id = false;
        let mut made = Vec::new();
        // (the edge as the handle saw it, the edge the reference used)
        let mut edges = Vec::new();
        // The revision each correctable chain has now, where a commit since
        // the transaction began corrected it: a correction of such a chain
        // is refused (weak mode gets here; optimistic has conflicted).
        let corrected_since: Vec<Option<u32>> =
            open.bases
                .iter()
                .map(|base| {
                    let moved = since.iter().flatten().any(
                        |entry| matches!(entry, Written::Fact { of: Some(of), .. } if of == base),
                    );
                    moved.then(|| {
                        self.reference
                            .entries()
                            .filter_map(|e| e.revision())
                            .filter(|(of, _)| of == base)
                            .map(|(_, rev)| rev)
                            .max()
                            .expect("a corrected chain has a revision")
                    })
                })
                .collect();
        let mut t = self.reference.begin();
        for (op, provisional_edge) in &open.ops {
            let names = Names {
                base: &self.base,
                made: &made,
            };
            if let TxOp::Correct { pick, .. } = op
                && let k = pick % open.facts.len()
                && let Some(rev) = corrected_since[k]
            {
                expected = Err(Error::AlreadyCorrected {
                    entry: open.facts[k],
                    rev,
                });
                break;
            }
            match make(&mut t, &names, *op, Some(open.base_tick), &open.facts) {
                Reply::Failed(e) => {
                    expected = Err(e);
                    break;
                }
                Reply::Made { id, created } => {
                    let k = made.len();
                    made.push(id);
                    // An ID the transaction handed out moved, or names an
                    // entity another commit made.
                    if open.exposed && open.made_new[k] && (id != open.made[k] || !created) {
                        exposed_id = true;
                        break;
                    }
                }
                Reply::Wrote { edge, .. } => {
                    edges.extend(provisional_edge.zip(edge));
                    if let (Some(provisional), Some(edge)) = (provisional_edge, edge)
                        && open.exposed
                        && provisional.seq() > open.edges_at_begin
                        && (*provisional != edge || edge.seq() <= edges_before)
                    {
                        exposed_id = true;
                        break;
                    }
                }
                Reply::Skipped => panic!("the reference can't resolve an edge: {op:?}\n{context}"),
                other => panic!("unexpected {other:?}"),
            }
        }
        if exposed_id {
            drop(t);
            assert!(
                matches!(&result, Err(Error::Conflict(c)) if c.key.kind() == "exposed-id"),
                "an exposed ID moved: {context}"
            );
            stats::bump(&stats::EXPOSED);
            return self.files_agree(&context);
        }
        if let Err(e) = expected {
            drop(t);
            assert_eq!(result, Err(e), "the reference refused an operation");
            if !since.is_empty() {
                stats::bump(&stats::STORE_ERRORS);
            }
            return self.files_agree(&context);
        }
        let reference = t.commit().unwrap();
        let result = match result {
            Ok(result) => result,
            Err(e) => panic!(
                "the reference committed, the handle got {e:?}\nkeys {:?}\nsince {since:?}",
                open.keys
            ),
        };
        self.files_agree(&context);
        assert_eq!(
            (result.committed, result.tx, result.changed, &result.entries),
            (
                reference.committed,
                reference.tx,
                reference.changed,
                &reference.entries
            ),
            "{context}"
        );
        assert_eq!(result.edges, reference.edges, "{context}");
        assert_eq!(result.warnings, reference.warnings, "{context}");
        assert_eq!(result.finalized.is_some(), !since.is_empty(), "{context}");
        // The mapping: every provisional ID goes where the reference put it.
        for (provisional, fin) in open.made.iter().zip(&made) {
            assert_eq!(result.ids.resolve(*provisional), *fin, "{context}");
        }
        for (provisional, fin) in &result.ids.entities {
            assert_ne!(provisional, fin, "only moved IDs are listed: {context}");
            assert!(open.made.contains(provisional), "{context}");
        }
        for (provisional, fin) in &edges {
            assert_eq!(result.ids.resolve_edge(*provisional), *fin, "{context}");
            assert_eq!(
                result.ids.resolve(EntityId::from(*provisional)),
                EntityId::from(*fin),
                "{context}"
            );
        }
        for (provisional, fin) in &result.ids.edges {
            assert_ne!(provisional, fin, "only moved IDs are listed: {context}");
            assert!(provisional.seq() > open.edges_at_begin, "{context}");
        }
        let moved = open.tickless && latest_before != open.base_tick;
        assert_eq!(
            result.latest_tick_moved,
            moved.then_some(TickMoved {
                from: open.base_tick,
                to: latest_before
            }),
            "{context}"
        );
        if !since.is_empty() {
            stats::bump(&stats::OVER_NEWER);
            if !result.ids.entities.is_empty() {
                stats::bump(&stats::RENUMBERED);
            }
            if !result.ids.edges.is_empty() {
                stats::bump(&stats::EDGES_MOVED);
            }
            if moved {
                stats::bump(&stats::TICK_MOVED);
            }
            let staged: usize = result
                .finalized
                .iter()
                .flat_map(|f| &f.ops)
                .map(|op| op.changed)
                .sum();
            assert_eq!(staged, result.changed, "{context}");
            if !result.committed {
                stats::bump(&stats::BECAME_NOOP);
            }
        }
        if reference.committed {
            self.commits.push(written(&self.reference, reference.tx));
        }
    }
}

fn run(steps: Vec<Step>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let reference_path = dir.path().join("reference.kala");
    let (first, base) = seed(&path);
    drop(first);
    let (reference, _) = seed(&reference_path);
    let mut commands = Vec::new();
    let mut replies = Vec::new();
    let mut threads = Vec::new();
    for _ in 0..HANDLES {
        let (command_tx, command_rx) = channel();
        let (reply_tx, reply_rx) = channel();
        let (path, base) = (path.clone(), base.clone());
        threads.push(std::thread::spawn(move || {
            handle(path, base, command_rx, reply_tx)
        }));
        commands.push(command_tx);
        replies.push(reply_rx);
    }
    // Every handle is open before the first step. An open takes the writer
    // lock for a moment at its end, and a catch-up that finds the lock
    // taken applies nothing ("not yet") instead of reading a tail that may
    // not be durable (PR #20 review, item C): the driver, which compares
    // each `begin` with the reference, must not race the opens.
    for opened in &replies {
        assert!(matches!(opened.recv().unwrap(), Reply::Done));
    }
    let mut driver = Driver {
        path,
        base,
        reference,
        reference_path,
        commits: Vec::new(),
        commands,
        replies,
        open: (0..HANDLES).map(|_| None).collect(),
    };
    driver.files_agree("the seeds");
    for step in steps {
        driver.step(step);
    }
    // Close what is still open, then every handle must agree with the
    // reference and be clean.
    for h in 0..HANDLES {
        driver.step(Step::Drop { h });
    }
    driver.files_agree("the end");
    let Driver {
        commands,
        replies,
        reference,
        path,
        ..
    } = driver;
    drop(commands);
    for (thread, replies) in threads.into_iter().zip(replies) {
        let Reply::Final(state) = replies.recv().unwrap() else {
            panic!("final");
        };
        let (dump, entries, clean) = *state;
        thread.join().unwrap();
        assert!(clean, "doctor found errors on a handle");
        assert!(dump == reference.index_dump(), "a handle's indexes differ");
        assert!(entries == reference.entries().collect::<Vec<_>>());
    }
    assert!(
        doctor(&reference, &DoctorOptions::default())
            .errors()
            .is_empty()
    );
    assert!(
        doctor_log(&path, &DoctorOptions::default())
            .unwrap()
            .errors()
            .is_empty()
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 192, ..ProptestConfig::default() })]

    #[test]
    fn interleaved_transactions_equal_the_reference_in_commit_order(
        steps in prop::collection::vec(step(), 1..60)
    ) {
        run(steps);
    }
}

/// The generator reaches what the property is about, counted over a fixed
/// set of runs: commits over newer commits, renumbered entities and edges,
/// conflicts, weak commits over invalidated reads, exposed IDs, store-rule
/// errors, a moved latest tick, writes that became redundant.
#[test]
fn the_generator_reaches_every_outcome() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let strategy = prop::collection::vec(step(), 1..60);
    for _ in 0..400 {
        run(strategy.new_tree(&mut runner).unwrap().current());
    }
    let mut failed = false;
    for (name, counter, at_least) in [
        ("commits over newer commits", &stats::OVER_NEWER, 100),
        ("conflicts", &stats::CONFLICTS, 30),
        (
            "weak over an invalidated read",
            &stats::WEAK_OVER_INVALIDATED_READ,
            5,
        ),
        ("exposed IDs", &stats::EXPOSED, 3),
        ("store errors at commit", &stats::STORE_ERRORS, 3),
        ("renumbered entities", &stats::RENUMBERED, 10),
        ("moved edges", &stats::EDGES_MOVED, 3),
        ("moved latest tick", &stats::TICK_MOVED, 3),
        ("all writes became redundant", &stats::BECAME_NOOP, 1),
        ("read-only transactions", &stats::READ_ONLY, 10),
    ] {
        let reached = stats::get(counter);
        eprintln!("{name}: {reached}");
        failed |= reached < at_least;
    }
    assert!(!failed, "the generator misses an outcome (counts above)");
}

/// Two handles create an entity each from one snapshot and write on it.
#[test]
fn two_creates_from_one_snapshot() {
    let create = TxOp::Create {
        ty: 0,
        handle: None,
    };
    let write = TxOp::Write {
        subject: Subject::Ent(Ent::Made(0)),
        attr: 0,
        value: Val::Ref(Ent::Base(3)),
        tick: None,
        retract: false,
    };
    let mut steps = Vec::new();
    for h in 0..2 {
        steps.push(Step::Begin {
            h,
            weak: false,
            exposed: false,
        });
        steps.push(Step::Op { h, op: create });
        steps.push(Step::Op { h, op: write });
    }
    steps.push(Step::Commit { h: 0 });
    steps.push(Step::Commit { h: 1 });
    run(steps);
}
