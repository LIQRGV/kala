//! The eval pass: runs a compiled program against the view.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use kala_core::{
    EdgeId, EntityId, Error as CoreError, IntervalSet, MeteredView, Tick, Value, WorkMeter, World,
};

use crate::ir::{Access, CmpOp, Func};

use super::QueryError;
use super::budget::Budget;
use super::program::{Attr, Pos, Step, TickPos, pop};
use super::val::{
    CMP_BYTES_PER_STEP, Interner, PAIR_BYTES, Pairs, Points, Row, STRING_OVERHEAD_BYTES, Sets, Val,
    compare, index_entry_bytes,
};
pub(super) use fixpoint::Relation;
use fixpoint::{Frame, Read};

mod fixpoint;
mod read;

pub(super) struct Exec<'a, 'w> {
    pub(super) world: &'a World,
    /// The query's view, metered by `meter`.
    pub(super) view: &'a MeteredView<'w>,
    /// Index keys examined by `view`'s reads not yet charged as steps.
    pub(super) meter: &'a WorkMeter<'a>,
    pub(super) width: usize,
    pub(super) budget: Budget<'a>,
    /// Every string and interval set the query holds, each once with its
    /// id: rows share these and compare them by id.
    pub(super) interner: Interner,
    /// `attr_scan` results by attribute name, then tick, read once per query.
    /// Only scans at constant, parameter or default ticks are kept here; a
    /// per-row tick's scan lives only while its rows expand (see
    /// `Step::Expand`). Keyed by name first so a lookup needn't allocate.
    pub(super) scans: HashMap<String, HashMap<Tick, Rc<Pairs>>>,
    /// `history_scan` results by attribute name, read once per query and
    /// charged to the byte budget while held (until the query ends).
    pub(super) history_scans: HashMap<String, Rc<Points>>,
    /// The cached `history_scan`s grouped into interval sets by `(entity,
    /// value)`, by attribute name, built once per query and charged while
    /// held (until the query ends).
    pub(super) during_scans: HashMap<String, Rc<Sets>>,
    /// Types whose visibility lookup has paid its historical one-step
    /// charge. Individual IDs can be checked in constant time.
    pub(super) checked_types: HashSet<u16>,
    /// Every relation of the plan, by id (empty outside its `Fixpoint`).
    pub(super) rels: Vec<Relation>,
}

impl Exec<'_, '_> {
    /// Bound this read by the steps still available. Updating the allowance
    /// does not change successful reads' historical key/step accounting.
    fn read_view(&self) -> MeteredView<'_> {
        self.meter.set_max_keys(
            self.budget
                .limits
                .max_steps
                .saturating_sub(self.budget.steps),
        );
        *self.view
    }

    /// Charges the index keys the view's reads examined since the last
    /// charge as steps (one per key). Called after every kala-core read.
    pub(super) fn charge_index_work(&mut self) -> Result<(), QueryError> {
        self.meter.check()?;
        self.budget.step(self.meter.take_keys())
    }

    /// Charges the up-front part of a memo-missing kala-core read that takes
    /// the bound value `v`: about one step per [`CMP_BYTES_PER_STEP`] bytes
    /// of a string (it is copied for the call), before the copy is made, so
    /// a long value can't be copied many times before the budget trips.
    /// Returns that per-copy charge for [`Exec::charge_bound_read`].
    fn charge_bound_value(&mut self, v: &Val) -> Result<u64, QueryError> {
        let per_key = match v {
            Val::Str(_, s) => s.len() as u64 / CMP_BYTES_PER_STEP,
            _ => 0,
        };
        self.budget.step(per_key)?;
        Ok(per_key)
    }

    /// Like [`Exec::charge_index_work`], for a read that took a bound value
    /// charged `per_key` up front ([`Exec::charge_bound_value`]): every index
    /// key it examined carries (and compares) the whole value, so each costs
    /// one step plus `per_key`.
    fn charge_bound_read(&mut self, per_key: u64) -> Result<(), QueryError> {
        self.meter.check()?;
        let keys = self.meter.take_keys();
        self.budget
            .step(keys.saturating_add(keys.saturating_mul(per_key)))
    }

    /// `v` as an executor value, its string interned: a string the query
    /// already holds is shared; a new one is charged once (text plus
    /// [`STRING_OVERHEAD_BYTES`]).
    ///
    /// Only for values a kala-core read returned: a string the query already
    /// holds still cost that read a copy and this a re-hash, so it is
    /// charged about one step per [`CMP_BYTES_PER_STEP`] bytes (per-row
    /// reads are memoized, so this is paid per distinct read, not per row).
    fn intern(&mut self, v: Value) -> Result<Val, QueryError> {
        Ok(match v {
            Value::Str(s) => {
                let (v, charge) = self.interner.string(&s, true)?;
                if charge {
                    self.budget
                        .charge_bytes(s.len().saturating_add(STRING_OVERHEAD_BYTES))?;
                } else {
                    self.budget.step(s.len() as u64 / CMP_BYTES_PER_STEP)?;
                }
                v
            }
            Value::Bool(b) => Val::Bool(b),
            Value::I64(n) => Val::I64(n),
            Value::F64(x) => Val::F64(x),
            Value::Ref(e) => Val::Ref(e),
        })
    }

    /// The query's shared copy of `s` ([`Exec::intern`]), hashed once
    /// here; a string new to the budget (a plan constant's text included)
    /// is charged once.
    fn intern_str(&mut self, s: &str) -> Result<Val, QueryError> {
        let (v, charge) = self.interner.string(s, true)?;
        if charge {
            self.budget
                .charge_bytes(s.len().saturating_add(STRING_OVERHEAD_BYTES))?;
        }
        Ok(v)
    }

    /// Runs a compiled program (see [`Step`]) and returns its root's rows.
    /// Every row set is charged to the byte budget while it is held (as its
    /// rows are emitted) and released once its consumer has run.
    ///
    /// The program is walked with a program counter rather than an
    /// iterator, so a step may move it (semi-naive evaluation will jump
    /// back to re-run a `Fixpoint`'s rules).
    pub(super) fn eval(&mut self, prog: &[Step]) -> Result<BTreeSet<Row>, QueryError> {
        // The row sets of operators whose consumer hasn't run yet (for an
        // open union: its accumulated output).
        let mut stack: Vec<BTreeSet<Row>> = Vec::new();
        // The open `Fixpoint`s, innermost last.
        let mut frames: Vec<Frame> = Vec::new();
        // The input rows of the open `Not`s, innermost last (still charged).
        let mut seeds: Vec<BTreeSet<Row>> = Vec::new();
        let mut pc = 0;
        while let Some(step) = prog.get(pc) {
            pc += 1;
            match step {
                Step::Unit => {
                    let mut out = BTreeSet::new();
                    self.budget.emit(&mut out, vec![None; self.width])?;
                    stack.push(out);
                }
                Step::Join { shared } => {
                    let r = pop(&mut stack);
                    let l = pop(&mut stack);
                    let out = self.join(&l, &r, shared)?;
                    self.budget.release_rows(l.len() + r.len());
                    stack.push(out);
                }
                Step::UnionBegin => stack.push(BTreeSet::new()),
                Step::UnionAdd => {
                    let rows = pop(&mut stack);
                    let n = rows.len();
                    let acc = stack
                        .last_mut()
                        .expect("invariant: UnionAdd follows its UnionBegin");
                    for row in rows {
                        self.budget.emit(acc, row)?;
                    }
                    self.budget.release_rows(n);
                }
                // The accumulated union is already on top.
                Step::UnionEnd => {}
                Step::NotBegin => seeds.push(pop(&mut stack)),
                Step::Seed => {
                    let seed = seeds
                        .last()
                        .expect("invariant: a Seed runs inside its Not body");
                    let mut out = BTreeSet::new();
                    for row in seed {
                        self.budget.emit(&mut out, row.clone())?;
                    }
                    stack.push(out);
                }
                Step::NotEnd { key } => {
                    let body = pop(&mut stack);
                    let input = seeds
                        .pop()
                        .expect("invariant: a NotEnd closes its NotBegin");
                    let out = self.anti_semi(&input, &body, key, false)?;
                    self.budget.release_rows(input.len() + body.len());
                    stack.push(out);
                }
                Step::Difference { shared } | Step::Intersect { shared } => {
                    let r = pop(&mut stack);
                    let l = pop(&mut stack);
                    let keep_matched = matches!(step, Step::Intersect { .. });
                    let out = self.anti_semi(&l, &r, shared, keep_matched)?;
                    self.budget.release_rows(l.len() + r.len());
                    stack.push(out);
                }
                // A Fixpoint evaluated again starts from empty relations.
                Step::FixBegin { rels } => {
                    for &id in rels {
                        self.clear_rel(id);
                    }
                    frames.push(Frame {
                        loop_pc: 0,
                        round: 0,
                        variant: 0,
                        delta_occ: None,
                    });
                }
                Step::RuleEnd { rel, cols } => {
                    let rows = pop(&mut stack);
                    let n = rows.len();
                    self.add_tuples(*rel, cols, &rows)?;
                    self.budget.release_rows(n);
                    let f = frames
                        .last_mut()
                        .expect("invariant: a RuleEnd runs inside its Fixpoint");
                    if f.round > 0 {
                        // A variant is done: the next one, or the round's end.
                        let Step::FixLoop { variants, .. } = &prog[f.loop_pc] else {
                            unreachable!("invariant: loop_pc is its Fixpoint's FixLoop")
                        };
                        pc = match self.next_variant(variants, f.variant + 1) {
                            Some(i) => {
                                f.variant = i;
                                f.delta_occ = Some(variants[i].occ);
                                variants[i].start
                            }
                            None => f.loop_pc,
                        };
                    }
                }
                Step::FixLoop { rels, variants } => {
                    let f = frames
                        .last_mut()
                        .expect("invariant: a FixLoop runs inside its Fixpoint");
                    f.loop_pc = pc - 1;
                    for &id in rels {
                        self.merge_new(id, !variants.is_empty())?;
                    }
                    f.round += 1;
                    // Only variants reading a non-empty delta can derive
                    // anything new; with none, the relations are complete.
                    match self.next_variant(variants, 0) {
                        Some(i) => {
                            f.variant = i;
                            f.delta_occ = Some(variants[i].occ);
                            pc = variants[i].start;
                        }
                        None => {
                            for &id in rels {
                                self.clear_delta(id);
                            }
                            frames.pop();
                        }
                    }
                }
                Step::Rel { rel, args, occ } => {
                    let read = match occ {
                        None => Read::All,
                        Some(o) => {
                            let f = frames
                                .last()
                                .expect("invariant: a recursive Rel runs inside its Fixpoint");
                            if f.round == 0 {
                                Read::Nothing
                            } else if f.delta_occ == Some(*o) {
                                Read::Delta
                            } else {
                                Read::All
                            }
                        }
                    };
                    let rows = pop(&mut stack);
                    let n = rows.len();
                    let mut out = BTreeSet::new();
                    self.rel(*rel, args, read, rows, &mut out)?;
                    self.budget.release_rows(n);
                    stack.push(out);
                }
                // The body's rows stay on top as the Fixpoint's.
                Step::FixEnd { rels } => {
                    for &id in rels {
                        self.clear_rel(id);
                    }
                }
                unary => {
                    let rows = pop(&mut stack);
                    let n = rows.len();
                    let out = self.eval_unary(unary, rows)?;
                    self.budget.release_rows(n);
                    stack.push(out);
                }
            }
        }
        let out = pop(&mut stack);
        debug_assert!(stack.is_empty() && seeds.is_empty());
        Ok(out)
    }

    /// The rows of `l` that match some row of `r` on the `shared` slots
    /// (bound on both sides) if `keep_matched`, else those matching none:
    /// a semi-join or an anti-join. With no shared slot, every row matches
    /// any row, so the answer is all of `l` or nothing. One step per probe.
    fn anti_semi(
        &mut self,
        l: &BTreeSet<Row>,
        r: &BTreeSet<Row>,
        shared: &[usize],
        keep_matched: bool,
    ) -> Result<BTreeSet<Row>, QueryError> {
        let mut keys = HashSet::new();
        let entry_bytes = index_entry_bytes(shared.len());
        let mut held = 0usize;
        for row in r {
            // Charge before allocating even the temporary key. Duplicate
            // keys release their temporary charge immediately.
            self.budget.charge_bytes(entry_bytes)?;
            if keys.insert(key_of(row, shared)) {
                held = held.saturating_add(entry_bytes);
            } else {
                self.budget.release_bytes(entry_bytes);
            }
        }
        let mut out = BTreeSet::new();
        for row in l {
            self.budget.step(1)?;
            if keys.contains(&key_of(row, shared)) == keep_matched {
                self.budget.emit(&mut out, row.clone())?;
            }
        }
        drop(keys);
        self.budget.release_bytes(held);
        Ok(out)
    }

    /// Hash join on the shared slots (all of them bound on both sides); no
    /// shared slots is a Cartesian product.
    fn join(
        &mut self,
        l: &BTreeSet<Row>,
        r: &BTreeSet<Row>,
        shared: &[usize],
    ) -> Result<BTreeSet<Row>, QueryError> {
        let mut out = BTreeSet::new();
        let mut by_key: HashMap<Vec<&Val>, Vec<&Row>> = HashMap::new();
        let entry_bytes = index_entry_bytes(shared.len());
        let mut held = 0usize;
        for row in r {
            self.budget.charge_bytes(entry_bytes)?;
            held = held.saturating_add(entry_bytes);
            by_key.entry(key_of(row, shared)).or_default().push(row);
        }
        for lrow in l {
            let Some(matches) = by_key.get(&key_of(lrow, shared)) else {
                continue;
            };
            for rrow in matches {
                let merged: Row = lrow
                    .iter()
                    .zip(rrow.iter())
                    .map(|(a, b)| a.clone().or_else(|| b.clone()))
                    .collect();
                self.budget.emit(&mut out, merged)?;
            }
        }
        drop(by_key);
        self.budget.release_bytes(held);
        Ok(out)
    }

    /// Runs a one-input step on its input's rows.
    fn eval_unary(
        &mut self,
        step: &Step,
        rows: BTreeSet<Row>,
    ) -> Result<BTreeSet<Row>, QueryError> {
        let mut out = BTreeSet::new();
        match step {
            Step::NodeScan {
                slot,
                bound,
                node_type,
            } => return self.eval_node_scan(slot, bound, node_type, rows),
            Step::NodeSeek {
                slot,
                bound,
                entity,
            } => return self.eval_node_seek(slot, bound, entity, rows),
            Step::Expand {
                entity,
                attr,
                value,
                tick,
                access,
            } => return self.eval_expand(entity, attr, value, tick, access, rows),
            Step::Handle { entity, value } => return self.eval_handle(entity, value, rows),
            Step::History {
                entity,
                attr,
                value,
                tick,
                op,
                precision,
            } => {
                for row in rows {
                    self.history_row(&row, entity, attr, value, tick, op, precision, &mut out)?;
                }
            }
            Step::During {
                entity,
                attr,
                value,
                out: set,
            } => {
                for row in rows {
                    self.during_row(&row, entity, attr, value, set, &mut out)?;
                }
            }
            Step::Apply {
                func,
                args,
                out: res,
            } => {
                for row in rows {
                    if let Some(r) = self.apply_row(row, *func, args, res.as_ref())? {
                        self.budget.emit(&mut out, r)?;
                    }
                }
            }
            Step::Filter { op, left, right } => return self.eval_filter(op, left, right, rows),
            Step::Project { keep } => return self.eval_project(keep, rows),
            Step::Aggregate { keys, aggs } => return self.aggregate(rows, keys, aggs),
            Step::Unit
            | Step::Join { .. }
            | Step::UnionBegin
            | Step::UnionAdd
            | Step::UnionEnd
            | Step::NotBegin
            | Step::Seed
            | Step::NotEnd { .. }
            | Step::Difference { .. }
            | Step::Intersect { .. }
            | Step::FixBegin { .. }
            | Step::RuleEnd { .. }
            | Step::FixLoop { .. }
            | Step::Rel { .. }
            | Step::FixEnd { .. } => {
                unreachable!("invariant: eval runs only one-input steps through eval_unary")
            }
        }
        Ok(out)
    }

    /// Runs a `NodeScan` on its input's rows.
    fn eval_node_scan(
        &mut self,
        slot: &usize,
        bound: &bool,
        node_type: &str,
        rows: BTreeSet<Row>,
    ) -> Result<BTreeSet<Row>, QueryError> {
        let mut out = BTreeSet::new();
        self.budget.step(1)?;
        let all: Vec<EntityId> = self
            .read_view()
            .entities_of_type(node_type)?
            .collect::<Result<_, _>>()?;
        self.charge_index_work()?;
        for row in rows {
            if *bound {
                if let Some(Val::Ref(e)) = &row[*slot]
                    && all.binary_search(e).is_ok()
                {
                    self.budget.emit(&mut out, row)?;
                }
                continue;
            }
            for &e in &all {
                let mut r = row.clone();
                r[*slot] = Some(Val::Ref(e));
                self.budget.emit(&mut out, r)?;
            }
        }
        Ok(out)
    }

    /// Runs a `NodeSeek` on its input's rows.
    fn eval_node_seek(
        &mut self,
        slot: &usize,
        bound: &bool,
        entity: &Pos,
        rows: BTreeSet<Row>,
    ) -> Result<BTreeSet<Row>, QueryError> {
        let mut out = BTreeSet::new();
        for mut row in rows {
            let e = match resolve(entity, &row) {
                Some(Val::Ref(e)) => *e,
                Some(other) => return Err(not_an_entity(other)),
                None => unreachable!("invariant: compile checked NodeSeek's entity"),
            };
            if !self.defined_at_canon(e)? {
                continue;
            }
            if *bound {
                if row[*slot] != Some(Val::Ref(e)) {
                    continue;
                }
            } else {
                row[*slot] = Some(Val::Ref(e));
            }
            self.budget.emit(&mut out, row)?;
        }
        Ok(out)
    }

    /// Runs an `Expand` on its input's rows.
    fn eval_expand(
        &mut self,
        entity: &Pos,
        attr: &Attr,
        value: &Pos,
        tick: &TickPos,
        access: &Access,
        rows: BTreeSet<Row>,
    ) -> Result<BTreeSet<Row>, QueryError> {
        let mut out = BTreeSet::new();
        match (access, tick) {
            // A scan at a per-row tick: group the rows by tick and hold
            // one tick's scan at a time, so a query over many distinct
            // ticks never keeps every scan (long unindexed strings
            // included) alive at once. Still one scan per distinct tick.
            (Access::Scan, TickPos::Var(s)) => {
                let mut by_tick: BTreeMap<Tick, Vec<Row>> = BTreeMap::new();
                for row in rows {
                    let t = tick_of(&row, *s)?;
                    by_tick.entry(t).or_default().push(row);
                }
                for (t, rows) in by_tick {
                    let (all, fresh) = self.scan_once(&attr.name, t)?;
                    for row in rows {
                        let pairs =
                            self.expand_row(&row, entity, attr, value, t, *access, Some(&all))?;
                        for (e, v) in pairs {
                            if let Some(r) = bind(&row, entity, value, Val::Ref(e), v) {
                                self.budget.emit(&mut out, r)?;
                            }
                        }
                    }
                    // `all` is dropped here, before the next tick's scan.
                    if fresh {
                        self.budget.release_bytes(all.len() * PAIR_BYTES);
                    }
                }
            }
            _ => {
                for row in rows {
                    let t = match tick {
                        TickPos::Const(t) => *t,
                        TickPos::Var(s) => tick_of(&row, *s)?,
                    };
                    let pairs = self.expand_row(&row, entity, attr, value, t, *access, None)?;
                    for (e, v) in pairs {
                        if let Some(r) = bind(&row, entity, value, Val::Ref(e), v) {
                            self.budget.emit(&mut out, r)?;
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// Runs a `:handle` read on its input's rows.
    fn eval_handle(
        &mut self,
        entity: &Pos,
        value: &Pos,
        rows: BTreeSet<Row>,
    ) -> Result<BTreeSet<Row>, QueryError> {
        let mut out = BTreeSet::new();
        for row in rows {
            self.budget.step(1)?;
            let pair = match (resolve(entity, &row), resolve(value, &row)) {
                (Some(Val::Ref(e)), _) => {
                    let e = *e;
                    if !self.defined_at_canon(e)? {
                        None
                    } else {
                        match self.world.handle_of(e) {
                            Some(h) => Some((e, self.intern_str(h)?)),
                            None => None,
                        }
                    }
                }
                (Some(other), _) => return Err(not_an_entity(other)),
                (None, Some(v @ Val::Str(_, h))) => match self.world.entity_by_handle(h) {
                    Some(e) if self.defined_at_canon(e)? => Some((e, v.clone())),
                    _ => None,
                },
                (None, Some(other)) => {
                    return Err(QueryError::TypeMismatch(format!(
                        "a handle must be a string, got {}",
                        other.kind().name()
                    )));
                }
                (None, None) => unreachable!("invariant: compile checked handle access"),
            };
            if let Some((e, v)) = pair
                && let Some(r) = bind(&row, entity, value, Val::Ref(e), v)
            {
                self.budget.emit(&mut out, r)?;
            }
        }
        Ok(out)
    }

    /// Runs a `Filter` on its input's rows.
    fn eval_filter(
        &mut self,
        op: &CmpOp,
        left: &Pos,
        right: &Pos,
        rows: BTreeSet<Row>,
    ) -> Result<BTreeSet<Row>, QueryError> {
        let mut out = BTreeSet::new();
        for row in rows {
            let (Some(l), Some(r)) = (resolve(left, &row), resolve(right, &row)) else {
                unreachable!("invariant: compile checked filter operands are bound")
            };
            let keep = compare(*op, l, r, &mut self.budget.cmp)?;
            self.budget.charge_cmp()?;
            if keep {
                self.budget.emit(&mut out, row)?;
            }
        }
        Ok(out)
    }

    /// Runs a `Project` on its input's rows.
    fn eval_project(
        &mut self,
        keep: &[usize],
        rows: BTreeSet<Row>,
    ) -> Result<BTreeSet<Row>, QueryError> {
        let mut out = BTreeSet::new();
        for row in rows {
            let mut r = vec![None; self.width];
            for &s in keep {
                r[s] = row[s].clone();
            }
            self.budget.emit(&mut out, r)?;
        }
        Ok(out)
    }

    /// `row` with `func(args)` bound to `res` (`None` if a bound `res`
    /// differs), or for a filter (no `res`) `row` itself if it is true. One
    /// step, plus one per interval the set operations sweep; a result set is
    /// charged by size.
    fn apply_row(
        &mut self,
        row: Row,
        func: Func,
        args: &[Pos],
        res: Option<&Pos>,
    ) -> Result<Option<Row>, QueryError> {
        let arg = |i: usize| {
            resolve(&args[i], &row).expect("invariant: compile checked Apply's arguments are bound")
        };
        match func {
            Func::Intersect | Func::Union | Func::Difference => {
                let (a, b) = (as_set(func, arg(0))?, as_set(func, arg(1))?);
                let swept = a.len().saturating_add(b.len()) as u64;
                self.budget.step(swept.saturating_add(1))?;
                let r = match func {
                    Func::Intersect => a.intersect(b),
                    Func::Union => a.union(b),
                    _ => a.difference(b),
                };
                let r = self.hold_set(r)?;
                let res = res.expect("invariant: compile checked set functions have an output");
                Ok(bind_all(&row, [(res, r)]))
            }
            Func::NotEmpty => {
                self.budget.step(1)?;
                let keep = !as_set(func, arg(0))?.is_empty();
                Ok(keep.then_some(row))
            }
            Func::Contains => {
                self.budget.step(1)?;
                let t = match arg(1) {
                    Val::I64(n) => Tick(*n),
                    other => {
                        return Err(QueryError::TypeMismatch(format!(
                            "Apply contains needs an i64 tick, got {}",
                            other.kind().name()
                        )));
                    }
                };
                let keep = as_set(func, arg(0))?.contains(t);
                Ok(keep.then_some(row))
            }
        }
    }

    /// Whether `id` (an entity or an edge) is defined at the view's canon.
    fn defined_at_canon(&mut self, id: EntityId) -> Result<bool, QueryError> {
        if id.is_edge() {
            return Ok(id
                .as_edge()
                .is_some_and(|edge: EdgeId| self.read_view().edge(edge).is_some()));
        }
        let ty = id.node_type();
        if !self.checked_types.contains(&ty.0) {
            if self.world.node_types().nth(usize::from(ty.0)).is_none() {
                return Ok(false);
            }
            self.budget.step(1)?;
            self.checked_types.insert(ty.0);
        }
        Ok(self.read_view().is_defined(id))
    }
}

/// The tick in slot `s` of `row` (a bound tick variable).
fn tick_of(row: &Row, s: usize) -> Result<Tick, QueryError> {
    match &row[s] {
        Some(Val::I64(n)) => Ok(Tick(*n)),
        Some(other) => Err(QueryError::TypeMismatch(format!(
            "a tick must be i64, got {}",
            other.kind().name()
        ))),
        None => unreachable!("invariant: compile checked the tick var"),
    }
}

/// A position's value in `row`: constants and bound vars; `None` for free
/// vars and `_`.
fn resolve<'r>(p: &'r Pos, row: &'r Row) -> Option<&'r Val> {
    match p {
        Pos::Const(v) => Some(v),
        Pos::Bound(s) => row[*s].as_ref(),
        Pos::Free(_) | Pos::Any => None,
    }
}

/// `row` extended with `entity = e` and `value = v`, or `None` if a bound
/// position (or the same free variable twice) disagrees.
fn bind(row: &Row, entity: &Pos, value: &Pos, e: Val, v: Val) -> Option<Row> {
    bind_all(row, [(entity, e), (value, v)])
}

/// `row` extended with each position bound to its value, or `None` if a
/// bound position (or a free variable met twice) disagrees.
fn bind_all<const N: usize>(row: &Row, binds: [(&Pos, Val); N]) -> Option<Row> {
    let mut r = row.clone();
    for (p, x) in binds {
        match p {
            Pos::Any => {}
            Pos::Const(c) => {
                if *c != x {
                    return None;
                }
            }
            Pos::Bound(s) => {
                if r[*s].as_ref() != Some(&x) {
                    return None;
                }
            }
            // The first occurrence binds, a second one (same var in both
            // positions) must agree.
            Pos::Free(s) => match &r[*s] {
                Some(prev) if *prev != x => return None,
                Some(_) => {}
                None => r[*s] = Some(x),
            },
        }
    }
    Some(r)
}

fn key_of<'r>(row: &'r Row, shared: &[usize]) -> Vec<&'r Val> {
    shared
        .iter()
        .map(|&s| {
            row[s]
                .as_ref()
                .expect("invariant: shared join slots are bound on both sides")
        })
        .collect()
}

/// A bound value already checked against its attribute's value type.
fn value_of(v: &Val) -> Value {
    v.to_value()
        .expect("invariant: expand_row checked the value against the attribute's type")
}

/// `v` as an interval set, for an argument of `func`.
fn as_set(func: Func, v: &Val) -> Result<&IntervalSet, QueryError> {
    match v {
        Val::Intervals(_, s) => Ok(s),
        other => Err(QueryError::TypeMismatch(format!(
            "Apply {} needs interval sets, got {}",
            func.name(),
            other.kind().name()
        ))),
    }
}

fn not_an_entity(v: &Val) -> QueryError {
    QueryError::TypeMismatch(format!(
        "entity position needs an entity, got {}",
        v.kind().name()
    ))
}

/// A read of an entity undefined (at this canon) finds nothing, like a
/// `NodeSeek` of it, rather than failing.
fn absent_as_empty<T>(r: Result<T, CoreError>) -> Result<Option<T>, QueryError> {
    match r {
        Ok(x) => Ok(Some(x)),
        Err(CoreError::UnknownEntity(_)) => Ok(None),
        Err(e) => Err(QueryError::Core(e)),
    }
}
