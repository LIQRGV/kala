//! Inside the executor, rows hold [`Val`]s: a [`Value`], an op or an
//! interval set (the three kinds of [`Datum`]), whose strings and interval
//! sets are shared (`Rc`) and interned once per query with a dense id
//! (hash-consing: equal contents, one id): a long string read once and
//! joined into many rows is stored once, not once per row, and every
//! row-set operation (set inserts, joins, anti/semi-joins, `Rel` indexes,
//! grouping, relations) compares and hashes it by id, in O(1) whatever its
//! length. Row sets are therefore kept in id order, not [`Datum`] order;
//! what needs real value order (`<`-style filters, `min`/`max`, and the final
//! [`ResultSet`], sorted once at the end) compares contents and charges that
//! work to the step budget: about one step per 64 bytes and per 4 intervals
//! compared. Only the final [`ResultSet`] holds plain [`Datum`]s.
//!
//! [`ResultSet`]: super::ResultSet

use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::mem::size_of;
use std::rc::Rc;

use kala_core::{EntityId, Interval, IntervalSet, Op as FactOp, Tick, Value, ValueType};

use kala_core::Precision;

use crate::datum::{Datum, op_rank};
use crate::ir::CmpOp;

use super::QueryError;

/// The kind of a [`Val`]: a value of one type, an op or an interval set.
/// Ordered as [`Datum`] orders its kinds (values by type in [`ValueType`]
/// order, which is the encoding's tag order; then ops; then interval sets).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum ValKind {
    Null,
    List,
    Value(ValueType),
    Op,
    Intervals,
    Precision,
}

impl ValKind {
    /// The kind's name in error messages.
    pub(super) fn name(self) -> &'static str {
        match self {
            ValKind::Null => "null",
            ValKind::List => "list",
            ValKind::Value(t) => t.name(),
            ValKind::Op => "op",
            ValKind::Precision => "precision",
            ValKind::Intervals => "interval set",
        }
    }
}

/// A per-query dense id of an interned string or interval set: equal
/// contents share one id (hash-consing, see [`Interner`]), so rows compare
/// and hash such values in O(1), whatever their length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct Id(u32);

/// A value inside the executor: a [`Datum`] whose string or interval set
/// is shared and interned, so copying a row copies a pointer, not the text.
///
/// Compared and hashed in O(1): a string or interval set by its [`Id`],
/// which is content equality (every one is interned by the query's
/// [`Interner`]). So row sets deduplicate exactly as the answer will, but
/// their internal *order* is not [`Datum`]'s: whatever needs real value
/// order (`<`-style filters, `min`/`max`, the final sort) uses
/// [`cmp_real`], which charges long comparisons to the step budget.
#[derive(Clone, Debug)]
pub(super) enum Val {
    Null,
    List(Id, Rc<Vec<Val>>),
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(Id, Rc<str>),
    Ref(EntityId),
    /// A change point's op (bound by a history read).
    Op(FactOp),
    Precision(Precision),
    /// An interval set (built by `During` or `Apply`).
    Intervals(Id, Rc<IntervalSet>),
}

impl Val {
    /// The plain value (a string is copied), if this is a value.
    pub(super) fn to_value(&self) -> Option<Value> {
        Some(match self {
            Val::Bool(b) => Value::Bool(*b),
            Val::I64(n) => Value::I64(*n),
            Val::F64(x) => Value::F64(*x),
            Val::Str(_, s) => Value::Str(s.to_string()),
            Val::Ref(e) => Value::Ref(*e),
            Val::Null | Val::List(..) | Val::Op(_) | Val::Precision(_) | Val::Intervals(..) => {
                return None;
            }
        })
    }

    /// The answer's datum (a string or interval set is copied).
    pub(super) fn to_datum(&self) -> Datum {
        match self {
            Val::Null => Datum::Null,
            Val::List(_, xs) => Datum::List(xs.iter().map(Val::to_datum).collect()),
            Val::Op(op) => Datum::Op(*op),
            Val::Precision(p) => Datum::Precision(*p),
            Val::Intervals(_, s) => Datum::Intervals(IntervalSet::clone(s)),
            v => Datum::Value(v.to_value().expect("invariant: every other Val is a value")),
        }
    }

    pub(super) fn kind(&self) -> ValKind {
        match self {
            Val::Null => ValKind::Null,
            Val::List(..) => ValKind::List,
            Val::Bool(_) => ValKind::Value(ValueType::Bool),
            Val::I64(_) => ValKind::Value(ValueType::I64),
            Val::F64(_) => ValKind::Value(ValueType::F64),
            Val::Str(..) => ValKind::Value(ValueType::Str),
            Val::Ref(_) => ValKind::Value(ValueType::Ref),
            Val::Op(_) => ValKind::Op,
            Val::Precision(_) => ValKind::Precision,
            Val::Intervals(..) => ValKind::Intervals,
        }
    }
}

impl Ord for Val {
    /// The executor's internal order: by kind ([`ValKind`]), then within the
    /// kind; floats as [`Value`] orders them (NaN included); strings and
    /// interval sets by [`Id`] (O(1), not their contents' order).
    fn cmp(&self, other: &Val) -> Ordering {
        match (self, other) {
            (Val::List(a, _), Val::List(b, _)) => a.cmp(b),
            (Val::Bool(a), Val::Bool(b)) => a.cmp(b),
            (Val::I64(a), Val::I64(b)) => a.cmp(b),
            (Val::F64(a), Val::F64(b)) => Value::F64(*a).cmp(&Value::F64(*b)),
            (Val::Str(a, _), Val::Str(b, _)) => a.cmp(b),
            (Val::Ref(a), Val::Ref(b)) => a.cmp(b),
            (Val::Precision(a), Val::Precision(b)) => (*a as u8).cmp(&(*b as u8)),
            (Val::Op(a), Val::Op(b)) => op_rank(*a).cmp(&op_rank(*b)),
            (Val::Intervals(a, _), Val::Intervals(b, _)) => a.cmp(b),
            _ => self.kind().cmp(&other.kind()),
        }
    }
}

impl PartialOrd for Val {
    fn partial_cmp(&self, other: &Val) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Val {
    fn eq(&self, other: &Val) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Val {}

impl Hash for Val {
    /// Hashes what `cmp` compares, so equal values hash alike.
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Val::Null => ValKind::Null.hash(state),
            Val::List(id, _) => (ValKind::List, id).hash(state),
            Val::Str(id, _) => (ValueType::Str, id).hash(state),
            Val::F64(x) => Value::F64(*x).hash(state),
            Val::Bool(b) => (ValueType::Bool, b).hash(state),
            Val::I64(n) => (ValueType::I64, n).hash(state),
            Val::Ref(e) => (ValueType::Ref, e.raw()).hash(state),
            Val::Op(op) => (ValKind::Op, op_rank(*op)).hash(state),
            Val::Precision(p) => (ValKind::Precision, *p as u8).hash(state),
            Val::Intervals(id, _) => (ValKind::Intervals, id).hash(state),
        }
    }
}

/// Bytes of string compared per step charged by [`cmp_real`].
pub(super) const CMP_BYTES_PER_STEP: u64 = 64;

/// Intervals compared per step charged by [`cmp_real`].
const CMP_INTERVALS_PER_STEP: u64 = 4;

/// Comparison work done in real value order ([`cmp_real`]) and not yet
/// charged as steps: the lengths of the prefixes compared.
#[derive(Default)]
pub(super) struct CmpWork {
    bytes: u64,
    intervals: u64,
}

impl CmpWork {
    /// The whole steps of work done so far, removed (the remainder stays,
    /// so the charge is deterministic however the work is split).
    pub(super) fn take_steps(&mut self) -> u64 {
        let steps = self.bytes / CMP_BYTES_PER_STEP + self.intervals / CMP_INTERVALS_PER_STEP;
        self.bytes %= CMP_BYTES_PER_STEP;
        self.intervals %= CMP_INTERVALS_PER_STEP;
        steps
    }
}

/// How many leading elements `a` and `b` share, compared in chunks of
/// `chunk` (slice equality, so long equal runs are cheap even unoptimized).
fn common_prefix<T: PartialEq>(a: &[T], b: &[T], chunk: usize) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + chunk <= n && a[i..i + chunk] == b[i..i + chunk] {
        i += chunk;
    }
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// [`Datum`]'s order (the answer's): by kind, then within the kind, strings
/// by their bytes and interval sets as [`IntervalSet`] orders them. Values
/// that are one interned value (same [`Id`]) are equal at once; otherwise
/// the prefix compared is added to `work`.
pub(super) fn cmp_real(a: &Val, b: &Val, work: &mut CmpWork) -> Ordering {
    match (a, b) {
        (Val::List(x, a), Val::List(y, b)) => {
            if x == y {
                return Ordering::Equal;
            }
            work.bytes = work
                .bytes
                .saturating_add((a.len().min(b.len()) as u64).saturating_mul(64));
            cmp_real_rows(a, b, work)
        }
        (Val::Str(x, s), Val::Str(y, t)) => {
            if x == y || Rc::ptr_eq(s, t) {
                return Ordering::Equal;
            }
            let (s, t) = (s.as_bytes(), t.as_bytes());
            let k = common_prefix(s, t, CMP_BYTES_PER_STEP as usize);
            work.bytes = work.bytes.saturating_add(k as u64 + 1);
            s[k..].cmp(&t[k..])
        }
        (Val::Intervals(x, s), Val::Intervals(y, t)) => {
            if x == y || Rc::ptr_eq(s, t) {
                return Ordering::Equal;
            }
            let (s, t) = (s.intervals(), t.intervals());
            let k = common_prefix(s, t, CMP_INTERVALS_PER_STEP as usize);
            work.intervals = work.intervals.saturating_add(k as u64 + 1);
            // Past the shared prefix: the first differing interval decides,
            // compared as `IntervalSet` does; a prefix sorts first.
            let key = |i: &Interval| (i.from, i.until.is_none(), i.until);
            s[k..].iter().map(key).cmp(t[k..].iter().map(key))
        }
        _ => a.cmp(b),
    }
}

/// [`cmp_real`] over two rows of equal length, lexicographically.
pub(super) fn cmp_real_rows(a: &[Val], b: &[Val], work: &mut CmpWork) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let o = cmp_real(x, y, work);
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// Every string and interval set the query holds, each once with its dense
/// [`Id`] (hash-consing): a value is hashed once, when it enters the table;
/// from then on rows share it and compare it by id.
#[derive(Default)]
pub(super) struct Interner {
    lists: HashMap<Rc<Vec<Val>>, Id>,
    /// Each string, its id, and whether it is charged to the byte budget
    /// (plan constants aren't: the front-end bounds the plan's size; a
    /// string read from the world is charged once, on first read).
    strings: HashMap<Rc<str>, (Id, bool)>,
    /// Each interval set and its id.
    sets: HashMap<Rc<IntervalSet>, Id>,
}

impl Interner {
    pub(super) fn list(&mut self, values: Vec<Val>) -> Result<(Val, bool), QueryError> {
        if let Some((v, id)) = self.lists.get_key_value(&values) {
            return Ok((Val::List(*id, v.clone()), false));
        }
        let id = Self::next_id(self.lists.len())?;
        let values = Rc::new(values);
        self.lists.insert(values.clone(), id);
        Ok((Val::List(id, values), true))
    }
    /// The next id for a table of `len` entries. Ids run out only past
    /// `u32::MAX` distinct values, beyond any sane byte budget, but limits
    /// are caller-chosen, so running out is an error, never a panic.
    fn next_id(len: usize) -> Result<Id, QueryError> {
        u32::try_from(len).map(Id).map_err(|_| {
            QueryError::Overflow("more than 2^32 distinct values in one query".to_owned())
        })
    }

    /// The interned copy of `s`, and whether the caller must charge it to
    /// the byte budget now: `charge` asks for that, and it is true only the
    /// first time a charged string enters (a constant's text included).
    pub(super) fn string(&mut self, s: &str, charge: bool) -> Result<(Val, bool), QueryError> {
        if let Some((shared, &(id, charged))) = self.strings.get_key_value(s) {
            let v = Val::Str(id, Rc::clone(shared));
            let newly = charge && !charged;
            if newly && let Some(e) = self.strings.get_mut(s) {
                e.1 = true;
            }
            return Ok((v, newly));
        }
        let id = Interner::next_id(self.strings.len())?;
        let shared: Rc<str> = Rc::from(s);
        self.strings.insert(Rc::clone(&shared), (id, charge));
        Ok((Val::Str(id, shared), charge))
    }

    /// `set` interned: an equal set already held is shared.
    pub(super) fn set(&mut self, set: IntervalSet) -> Result<Val, QueryError> {
        if let Some((shared, id)) = self.sets.get_key_value(&set) {
            return Ok(Val::Intervals(*id, Rc::clone(shared)));
        }
        let id = Interner::next_id(self.sets.len())?;
        let shared = Rc::new(set);
        self.sets.insert(Rc::clone(&shared), id);
        Ok(Val::Intervals(id, shared))
    }

    /// A plan constant (or a resolved param) as a value, its string
    /// interned but not charged.
    pub(super) fn constant(&mut self, v: &Value) -> Result<Val, QueryError> {
        Ok(match v {
            Value::Bool(b) => Val::Bool(*b),
            Value::I64(n) => Val::I64(*n),
            Value::F64(x) => Val::F64(*x),
            Value::Str(s) => self.string(s, false)?.0,
            Value::Ref(e) => Val::Ref(*e),
        })
    }
}

/// `(entity, value)` pairs read from an index.
pub(super) type Pairs = Vec<(EntityId, Val)>;

/// Change points `(entity, value, tick, op)` read from an index, sorted by
/// entity (then as [`View::history`](kala_core::View::history) orders one entity's).
pub(super) type Points = Vec<(EntityId, Val, Tick, FactOp, Precision)>;

/// Charged per change point of a held history scan.
pub(super) const POINT_BYTES: usize = size_of::<(EntityId, Val, Tick, FactOp, Precision)>();

/// The interval sets of an attribute's history scan, one per `(entity,
/// value)` that ever held, sorted by entity (then value).
pub(super) type Sets = Vec<(EntityId, Val, Val)>;

/// Charged per `(entity, value, set)` entry of a held grouped scan, beyond
/// the set itself.
pub(super) const SET_ENTRY_BYTES: usize = size_of::<(EntityId, Val, Val)>();

/// Charged per interval set created, beyond its intervals: the `Rc` header
/// and the set's own `Vec`.
const SET_OVERHEAD_BYTES: usize = 2 * size_of::<usize>() + size_of::<IntervalSet>();

/// What an interval set costs while held: [`SET_OVERHEAD_BYTES`] plus its
/// intervals.
pub(super) fn set_bytes(s: &IntervalSet) -> usize {
    s.len()
        .saturating_mul(size_of::<Interval>())
        .saturating_add(SET_OVERHEAD_BYTES)
}

/// A row: one optional value per slot. Every row of one operator binds the
/// same slots (checked at compile time), so `None` means "not bound here".
pub(super) type Row = Vec<Option<Val>>;

/// Bookkeeping charged per row beyond its slots: the B-tree set's share of
/// node space and pointers. A fixed estimate, so the budget is
/// deterministic.
pub(super) const ROW_OVERHEAD_BYTES: usize = 16;

/// Charged per distinct string the query holds, beyond its text: the
/// `Rc` header and the intern table's slot.
pub(super) const STRING_OVERHEAD_BYTES: usize = 32;

/// Charged per `(entity, value)` pair of a held scan.
pub(super) const PAIR_BYTES: usize = size_of::<(EntityId, Val)>();

/// Conservative hash entry charge: key slots use the same fixed cost as
/// row sets, plus a row-reference slot and the bucket's vector/header.
pub(super) fn index_entry_bytes(keys: usize) -> usize {
    row_bytes(keys)
        .saturating_add(size_of::<Vec<&Row>>())
        .saturating_add(size_of::<&Row>())
}

/// What one tuple of an `arity`-column relation costs while it is held.
pub(super) fn tuple_bytes(arity: usize) -> usize {
    size_of::<Vec<Val>>() + arity * size_of::<Val>() + ROW_OVERHEAD_BYTES
}

/// What one row of `width` slots costs in an intermediate set.
pub(super) fn row_bytes(width: usize) -> usize {
    size_of::<Row>() + width * size_of::<Option<Val>>() + ROW_OVERHEAD_BYTES
}

/// What one row of the final [`ResultSet`](super::ResultSet) costs: a `Vec` of [`Datum`]s
/// whose strings and interval sets are copies.
pub(super) fn result_row_bytes(row: &[Val]) -> usize {
    let heap: usize = row
        .iter()
        .map(|v| match v {
            Val::Str(_, s) => s.len(),
            Val::Intervals(_, s) => s.len().saturating_mul(size_of::<Interval>()),
            Val::List(_, xs) => result_row_bytes(xs),
            _ => 0,
        })
        .sum();
    size_of::<Vec<Datum>>() + row.len() * size_of::<Datum>() + heap
}

/// `l op r`. Equality across types is just false; ordering needs one type
/// and is real value order ([`cmp_real`], its work added to `work`).
pub(super) fn compare(op: CmpOp, l: &Val, r: &Val, work: &mut CmpWork) -> Result<bool, QueryError> {
    if !matches!(op, CmpOp::Eq | CmpOp::Ne) && l.kind() != r.kind() {
        return Err(QueryError::TypeMismatch(format!(
            "can't order {} against {}",
            l.kind().name(),
            r.kind().name()
        )));
    }
    Ok(match op {
        CmpOp::Eq => l == r,
        CmpOp::Ne => l != r,
        CmpOp::Lt => cmp_real(l, r, work).is_lt(),
        CmpOp::Le => cmp_real(l, r, work).is_le(),
        CmpOp::Gt => cmp_real(l, r, work).is_gt(),
        CmpOp::Ge => cmp_real(l, r, work).is_ge(),
    })
}
