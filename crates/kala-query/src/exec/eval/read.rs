//! Per-row index reads (`Expand`, `History`, `During`) and the per-query
//! scans they share.

use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;
use std::rc::Rc;

use kala_core::{Direction, EntityId, IntervalSet, Op as FactOp, Tick, Value};

use crate::ir::Access;

use super::{Exec, absent_as_empty, bind_all, not_an_entity, resolve, value_of};
use crate::exec::budget::MEMO_ENTRY_BYTES;
use crate::exec::program::{Attr, Pos};
use crate::exec::val::{
    PAIR_BYTES, POINT_BYTES, Pairs, Points, Row, SET_ENTRY_BYTES, Sets, Val, ValKind, set_bytes,
};
use crate::exec::{QueryError, type_name};

impl Exec<'_, '_> {
    /// The `(entity, value)` pairs of `attr` holding at `t` that `access`
    /// reads for `row`, already narrowed by the bound side the access needs
    /// (any other bound side is checked by [`bind`](super::bind)). For [`Access::Scan`],
    /// `scanned` is the scan of `attr` at `t` if the caller already holds it;
    /// otherwise the per-query cache ([`Exec::scan`]) is used.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn expand_row(
        &mut self,
        row: &Row,
        entity: &Pos,
        attr: &Attr,
        value: &Pos,
        t: Tick,
        access: Access,
        scanned: Option<&Pairs>,
    ) -> Result<Pairs, QueryError> {
        let e = match resolve(entity, row) {
            Some(Val::Ref(e)) => Some(*e),
            Some(other) => return Err(not_an_entity(other)),
            None => None,
        };
        let v = match resolve(value, row) {
            Some(v) if v.kind() != ValKind::Value(attr.value_type) => {
                return Err(QueryError::TypeMismatch(format!(
                    ":{} needs {} values, got {}",
                    attr.name,
                    type_name(attr.value_type),
                    v.kind().name()
                )));
            }
            // NaN is never stored, so it matches nothing, whichever access.
            Some(Val::F64(x)) if x.is_nan() => return Ok(Vec::new()),
            other => other,
        };
        let name = attr.name.as_str();
        let pairs = match access {
            Access::E => {
                let e = e.expect("invariant: compile checked E has the entity bound");
                self.budget.step(1)?;
                if let (Some(v), false) = (v, attr.inverse) {
                    let key = (e, v.clone(), t);
                    let memo = self.budget.memo(name);
                    let holds = match memo.and_then(|m| m.as_of_value.get(&key)) {
                        Some(&h) => h,
                        None => {
                            // kala-core takes an owned value: a string is
                            // copied here, once per memo key, and charged by
                            // its length (a per-row tick misses every time).
                            let per_key = self.charge_bound_value(v)?;
                            let r = self.read_view().as_of_value(e, name, &value_of(v), t);
                            self.charge_bound_read(per_key)?;
                            let h = absent_as_empty(r)? == Some(true);
                            let n = MEMO_ENTRY_BYTES + size_of_val(&key);
                            if let Some(m) = self.budget.memo_slot(name, n) {
                                m.as_of_value.insert(key, h);
                            }
                            h
                        }
                    };
                    if holds {
                        vec![(e, v.clone())]
                    } else {
                        Vec::new()
                    }
                } else if attr.inverse {
                    let ns = self.read_view().neighbors(e, name, Direction::Out, t);
                    self.charge_index_work()?;
                    absent_as_empty(ns)?
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(x, _)| (e, Val::Ref(x)))
                        .collect()
                } else {
                    let memo = self.budget.memo(name);
                    let vals = match memo.and_then(|m| m.as_of.get(&(e, t))) {
                        Some(vals) => Rc::clone(vals),
                        None => {
                            let vs = self.read_view().as_of(e, name, t);
                            self.charge_index_work()?;
                            let mut vals = Vec::new();
                            for x in absent_as_empty(vs)?.unwrap_or_default() {
                                vals.push(self.intern(x)?);
                            }
                            let vals: Rc<[Val]> = vals.into();
                            let n = MEMO_ENTRY_BYTES + vals.len() * size_of::<Val>();
                            if let Some(m) = self.budget.memo_slot(name, n) {
                                m.as_of.insert((e, t), Rc::clone(&vals));
                            }
                            vals
                        }
                    };
                    vals.iter().map(|x| (e, x.clone())).collect()
                }
            }
            Access::R | Access::V => {
                let v = v.expect("invariant: compile checked R/V have the value bound");
                self.budget.step(1)?;
                match v {
                    Val::Ref(y) => {
                        let ns = self.read_view().neighbors(*y, name, Direction::In, t);
                        self.charge_index_work()?;
                        absent_as_empty(ns)?
                            .unwrap_or_default()
                            .into_iter()
                            .map(|(x, _)| (x, v.clone()))
                            .collect()
                    }
                    _ => {
                        let key = (v.clone(), t);
                        let memo = self.budget.memo(name);
                        let found = match memo.and_then(|m| m.lookup.get(&key)) {
                            Some(found) => Rc::clone(found),
                            None => {
                                // Copies a string value, once per memo key,
                                // charged by its length.
                                let per_key = self.charge_bound_value(v)?;
                                let found = self.read_view().lookup(name, &value_of(v), t);
                                self.charge_bound_read(per_key)?;
                                let found: Rc<[EntityId]> = found?.into();
                                let n = MEMO_ENTRY_BYTES
                                    + size_of_val(&key)
                                    + found.len() * size_of::<EntityId>();
                                if let Some(m) = self.budget.memo_slot(name, n) {
                                    m.lookup.insert(key, Rc::clone(&found));
                                }
                                found
                            }
                        };
                        found.iter().map(|&x| (x, v.clone())).collect()
                    }
                }
            }
            Access::Scan => {
                let cached;
                let all: &Pairs = match scanned {
                    Some(p) => p,
                    None => {
                        cached = self.scan(name, t)?;
                        &cached
                    }
                };
                let range = match e {
                    // Sorted by (entity, value): the entity's pairs are one run.
                    Some(e) => {
                        let lo = all.partition_point(|(x, _)| *x < e);
                        let hi = all.partition_point(|(x, _)| *x <= e);
                        lo..hi
                    }
                    None => 0..all.len(),
                };
                self.budget.step(range.len() as u64)?;
                all[range]
                    .iter()
                    .filter(|(_, x)| v.is_none_or(|v| x == v))
                    .cloned()
                    .collect()
            }
            Access::Handle => {
                unreachable!("invariant: compile routes :kala_handle to Step::Handle")
            }
        };
        self.budget.step(pairs.len() as u64)?;
        Ok(pairs)
    }

    /// Emits into `out` `row` extended with each change point of `attr`
    /// matching the row's bound positions. With the entity bound (and a
    /// forward name) the points are one [`View::history`] read; otherwise
    /// they come from the query's cached [`View::history_scan`] (narrowed to
    /// the entity's run if it is bound, for an inverse name). One step per
    /// point examined, plus the index work.
    ///
    /// [`View::history`]: kala_core::View::history
    /// [`View::history_scan`]: kala_core::View::history_scan
    #[allow(clippy::too_many_arguments)]
    pub(super) fn history_row(
        &mut self,
        row: &Row,
        entity: &Pos,
        attr: &Attr,
        value: &Pos,
        tick: &Pos,
        op: &Pos,
        precision: &Pos,
        out: &mut BTreeSet<Row>,
    ) -> Result<(), QueryError> {
        let e = match resolve(entity, row) {
            Some(Val::Ref(e)) => Some(*e),
            Some(other) => return Err(not_an_entity(other)),
            None => None,
        };
        match resolve(value, row) {
            Some(v) if v.kind() != ValKind::Value(attr.value_type) => {
                return Err(QueryError::TypeMismatch(format!(
                    ":{} needs {} values, got {}",
                    attr.name,
                    type_name(attr.value_type),
                    v.kind().name()
                )));
            }
            // NaN is never stored, so it matches nothing.
            Some(Val::F64(x)) if x.is_nan() => return Ok(()),
            _ => {}
        }
        if let Some(t) = resolve(tick, row)
            && !matches!(t, Val::I64(_))
        {
            return Err(QueryError::TypeMismatch(format!(
                "a tick must be i64, got {}",
                t.kind().name()
            )));
        }
        let (points, range) = match e {
            Some(e) if !attr.inverse => {
                self.budget.step(1)?;
                let points = self.history_of(e, &attr.name)?;
                let n = points.len();
                (points, 0..n)
            }
            _ => {
                let all = self.history_scan(&attr.name)?;
                let range = match e {
                    // Sorted by entity: the entity's points are one run.
                    Some(e) => {
                        let lo = all.partition_point(|(x, ..)| *x < e);
                        let hi = all.partition_point(|(x, ..)| *x <= e);
                        lo..hi
                    }
                    None => 0..all.len(),
                };
                (all, range)
            }
        };
        self.budget.step(range.len() as u64)?;
        for (x, v, t, o, p) in &points[range] {
            let binds = [
                (entity, Val::Ref(*x)),
                (value, v.clone()),
                (tick, Val::I64(t.0)),
                (op, Val::Op(*o)),
                (precision, Val::Precision(*p)),
            ];
            if let Some(r) = bind_all(row, binds) {
                self.budget.emit(out, r)?;
            }
        }
        Ok(())
    }

    /// Emits into `out` `row` extended with the interval set where each
    /// matching `(entity, attr, value)` holds, over every tick, at the
    /// view's canon (only pairs that ever held: never an empty set). With
    /// the entity and value bound, one [`View::holds_during`] read; with
    /// only a forward name's entity bound, one [`View::history`] read
    /// grouped by value; otherwise the query's grouped history scan
    /// ([`Exec::during_scan`]), narrowed to the entity's run if it is
    /// bound. One step per read, set or change point examined, plus the
    /// index work; each set read is charged by size.
    ///
    /// [`View::holds_during`]: kala_core::View::holds_during
    /// [`View::history`]: kala_core::View::history
    pub(super) fn during_row(
        &mut self,
        row: &Row,
        entity: &Pos,
        attr: &Attr,
        value: &Pos,
        set: &Pos,
        out: &mut BTreeSet<Row>,
    ) -> Result<(), QueryError> {
        let e = match resolve(entity, row) {
            Some(Val::Ref(e)) => Some(*e),
            Some(other) => return Err(not_an_entity(other)),
            None => None,
        };
        let v = match resolve(value, row) {
            Some(v) if v.kind() != ValKind::Value(attr.value_type) => {
                return Err(QueryError::TypeMismatch(format!(
                    ":{} needs {} values, got {}",
                    attr.name,
                    type_name(attr.value_type),
                    v.kind().name()
                )));
            }
            // NaN is never stored, so it matches nothing.
            Some(Val::F64(x)) if x.is_nan() => return Ok(()),
            other => other,
        };
        match (e, v) {
            (Some(e), Some(v)) => {
                self.budget.step(1)?;
                let name = attr.name.as_str();
                let key = (e, v.clone());
                let memo = self.budget.memo(name);
                let found = match memo.and_then(|m| m.holds_during.get(&key)) {
                    Some(found) => found.clone(),
                    None => {
                        // kala-core takes an owned value: a string is copied
                        // here, once per memo key, and charged by its length
                        // (entities sharing one value miss every time).
                        let per_key = self.charge_bound_value(v)?;
                        let read = match (attr.inverse, v) {
                            // `[e child_of v]` is `[v parent_of e]`.
                            (true, Val::Ref(source)) => {
                                self.read_view()
                                    .holds_during(*source, &attr.base, &Value::Ref(e))
                            }
                            (true, _) => {
                                unreachable!("invariant: an inverse name's values are refs")
                            }
                            (false, v) => self.read_view().holds_during(e, name, &value_of(v)),
                        };
                        self.charge_bound_read(per_key)?;
                        let found = absent_as_empty(read)?.unwrap_or_default();
                        self.budget.step(found.len() as u64)?;
                        let found = if found.is_empty() {
                            None
                        } else {
                            Some(self.hold_set(found)?)
                        };
                        let n = MEMO_ENTRY_BYTES + size_of_val(&key) + size_of::<Option<Val>>();
                        if let Some(m) = self.budget.memo_slot(name, n) {
                            m.holds_during.insert(key, found.clone());
                        }
                        found
                    }
                };
                if let Some(found) = found
                    && let Some(r) = bind_all(row, [(set, found)])
                {
                    self.budget.emit(out, r)?;
                }
            }
            (Some(e), None) if !attr.inverse => {
                self.budget.step(1)?;
                let name = attr.name.as_str();
                let memo = self.budget.memo(name);
                let sets = match memo.and_then(|m| m.during.get(&e)) {
                    Some(sets) => Rc::clone(sets),
                    None => {
                        let points = self.history_of(e, name)?;
                        self.budget.step(points.len() as u64)?;
                        let mut by_value: BTreeMap<&Val, Vec<(Tick, FactOp)>> = BTreeMap::new();
                        for (_, x, t, o, _) in points.iter() {
                            by_value.entry(x).or_default().push((*t, *o));
                        }
                        let mut sets = Vec::new();
                        for (x, changes) in by_value {
                            let found = IntervalSet::from_changes(&changes);
                            if found.is_empty() {
                                continue;
                            }
                            sets.push((x.clone(), self.hold_set(found)?));
                        }
                        let sets: Rc<[(Val, Val)]> = sets.into();
                        let n = MEMO_ENTRY_BYTES + sets.len() * size_of::<(Val, Val)>();
                        if let Some(m) = self.budget.memo_slot(name, n) {
                            m.during.insert(e, Rc::clone(&sets));
                        }
                        sets
                    }
                };
                for (x, found) in sets.iter() {
                    if let Some(r) = bind_all(row, [(value, x.clone()), (set, found.clone())]) {
                        self.budget.emit(out, r)?;
                    }
                }
            }
            _ => {
                let all = self.during_scan(&attr.name)?;
                let range = match e {
                    // Sorted by entity: the entity's sets are one run.
                    Some(e) => {
                        let lo = all.partition_point(|(x, ..)| *x < e);
                        let hi = all.partition_point(|(x, ..)| *x <= e);
                        lo..hi
                    }
                    None => 0..all.len(),
                };
                self.budget.step(range.len() as u64)?;
                for (x, v, found) in &all[range] {
                    let binds = [
                        (entity, Val::Ref(*x)),
                        (value, v.clone()),
                        (set, found.clone()),
                    ];
                    if let Some(r) = bind_all(row, binds) {
                        self.budget.emit(out, r)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// `View::history(e, attr)` as `e`'s change points (values interned),
    /// memoized per query by `(e, attr)`: a repeat costs nothing here (the
    /// caller charges the row's read step).
    fn history_of(&mut self, e: EntityId, attr: &str) -> Result<Rc<Points>, QueryError> {
        if let Some(points) = self.budget.memo(attr).and_then(|m| m.history.get(&e)) {
            return Ok(Rc::clone(points));
        }
        let read = self.read_view().history(e, attr);
        self.charge_index_work()?;
        let mut points = Vec::new();
        for c in absent_as_empty(read)?.unwrap_or_default() {
            points.push((e, self.intern(c.value)?, c.tick, c.op, c.precision));
        }
        let points = Rc::new(points);
        let n = MEMO_ENTRY_BYTES + points.len() * POINT_BYTES;
        if let Some(m) = self.budget.memo_slot(attr, n) {
            m.history.insert(e, Rc::clone(&points));
        }
        Ok(points)
    }

    /// An interval set the query now holds, as a value: charged by size
    /// until the query ends (rows may share it, so it is never released
    /// early), and interned (hashed once here), so an equal set already
    /// held is shared. Every set created is charged, shared or not.
    pub(super) fn hold_set(&mut self, s: IntervalSet) -> Result<Val, QueryError> {
        self.budget.charge_bytes(set_bytes(&s))?;
        self.interner.set(s)
    }

    /// The query's history scan of `attr` ([`Exec::history_scan`]) grouped
    /// into one interval set per `(entity, value)` that ever held, built
    /// once per query and cached: one step per point grouped, and each set
    /// and entry charged while held.
    fn during_scan(&mut self, attr: &str) -> Result<Rc<Sets>, QueryError> {
        if let Some(s) = self.during_scans.get(attr) {
            return Ok(Rc::clone(s));
        }
        let points = self.history_scan(attr)?;
        self.budget.step(points.len() as u64)?;
        let mut sets = Vec::new();
        let mut i = 0;
        while i < points.len() {
            let e = points[i].0;
            // Sorted by entity: the entity's points are one run.
            let j = i + points[i..].partition_point(|(x, ..)| *x == e);
            let mut by_value: BTreeMap<&Val, Vec<(Tick, FactOp)>> = BTreeMap::new();
            for (_, v, t, o, _) in &points[i..j] {
                by_value.entry(v).or_default().push((*t, *o));
            }
            for (v, changes) in by_value {
                let found = IntervalSet::from_changes(&changes);
                if found.is_empty() {
                    continue;
                }
                self.budget.charge_bytes(SET_ENTRY_BYTES)?;
                let found = self.hold_set(found)?;
                sets.push((e, v.clone(), found));
            }
            i = j;
        }
        let sets = Rc::new(sets);
        self.during_scans.insert(attr.to_owned(), Rc::clone(&sets));
        Ok(sets)
    }

    /// `View::history_scan(attr)`, read once per query and cached for the
    /// rest of it: one step plus one per point plus the index work, and the
    /// points' bytes (strings interned) held until the query ends.
    fn history_scan(&mut self, attr: &str) -> Result<Rc<Points>, QueryError> {
        if let Some(p) = self.history_scans.get(attr) {
            return Ok(Rc::clone(p));
        }
        self.budget.step(1)?;
        let read = self.read_view().history_scan(attr);
        self.charge_index_work()?;
        let read = read?;
        self.budget.step(read.len() as u64)?;
        self.budget
            .charge_bytes(read.len().saturating_mul(POINT_BYTES))?;
        let mut points = Vec::with_capacity(read.len());
        for (e, c) in read {
            points.push((e, self.intern(c.value)?, c.tick, c.op, c.precision));
        }
        let points = Rc::new(points);
        self.history_scans
            .insert(attr.to_owned(), Rc::clone(&points));
        Ok(points)
    }

    /// `View::attr_scan(attr, t)`, read once per query and cached for the
    /// rest of it (for constant, parameter and default ticks: one scan each).
    /// A cached scan stays charged to the byte budget until the query ends.
    fn scan(&mut self, attr: &str, t: Tick) -> Result<Rc<Pairs>, QueryError> {
        if let Some(s) = self.scans.get(attr).and_then(|m| m.get(&t)) {
            return Ok(Rc::clone(s));
        }
        let pairs = self.read_scan(attr, t)?;
        self.scans
            .entry(attr.to_owned())
            .or_default()
            .insert(t, Rc::clone(&pairs));
        Ok(pairs)
    }

    /// `View::attr_scan(attr, t)` for a per-row tick: reuses a cached scan
    /// if one exists but doesn't cache a new one, so the caller decides how
    /// long it lives (one distinct tick at a time, see `Step::Expand`).
    /// Returns whether the scan was freshly read: the caller releases a
    /// fresh scan's bytes when it drops it.
    pub(super) fn scan_once(
        &mut self,
        attr: &str,
        t: Tick,
    ) -> Result<(Rc<Pairs>, bool), QueryError> {
        if let Some(s) = self.scans.get(attr).and_then(|m| m.get(&t)) {
            return Ok((Rc::clone(s), false));
        }
        Ok((self.read_scan(attr, t)?, true))
    }

    /// Reads `View::attr_scan(attr, t)`, charging one step plus one per
    /// pair plus the index work, and the pairs' bytes (strings interned).
    fn read_scan(&mut self, attr: &str, t: Tick) -> Result<Rc<Pairs>, QueryError> {
        self.budget.step(1)?;
        let read = self.read_view().attr_scan(attr, t);
        self.charge_index_work()?;
        let read = read?;
        self.budget.step(read.len() as u64)?;
        self.budget
            .charge_bytes(read.len().saturating_mul(PAIR_BYTES))?;
        let mut pairs = Vec::with_capacity(read.len());
        for (e, v) in read {
            pairs.push((e, self.intern(v)?));
        }
        Ok(Rc::new(pairs))
    }
}
