//! A `Fixpoint` compiles inline into the flat program: `FixBegin`, each
//! rule's steps followed by `RuleEnd` (which folds the rule's rows into its
//! relation as positional tuples, a set), `FixLoop`, the body's steps, then
//! `FixEnd` (the relations are dropped; the body's rows are the
//! `Fixpoint`'s). A `Rel` resolves at compile time to the nearest enclosing
//! `Fixpoint` defining its name; inside that `Fixpoint`'s own rules it is a
//! recursive read, numbered.
//!
//! Recursion is evaluated semi-naively, with a program counter and no
//! native recursion. Round 0 runs every rule once with recursive reads
//! empty. Each later round runs, for every recursive read of every rule
//! (a *variant*: the rule's contiguous run of steps), the rule with that
//! read seeing only the previous round's new tuples (the delta) and every
//! other read the whole relation; `FixLoop` keeps what is new, and stops
//! once no delta a variant reads is non-empty. The result equals naive
//! iteration (set semantics), each derivation costing work about once.
//! Relations of enclosing `Fixpoint`s are fixed and read in full. All limits
//! hold across rounds: rows (each relation, with its new tuples, and each
//! operator's output), steps and bytes (relations and deltas held).

use std::collections::{BTreeSet, HashMap};
use std::mem::size_of;

use super::{Exec, resolve};
use crate::exec::QueryError;
use crate::exec::program::{Pos, Variant};
use crate::exec::val::{Row, Val, index_entry_bytes};

/// Which tuples of a relation a `Rel` reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Read {
    All,
    Delta,
    Nothing,
}

/// A `Fixpoint` being evaluated (see [`Exec::eval`]).
pub(super) struct Frame {
    /// The program counter of its `FixLoop` (known once the first round
    /// reaches it).
    pub(super) loop_pc: usize,
    /// The round: 0 runs every rule once; later rounds run variants.
    pub(super) round: u64,
    /// In a later round, the index of the variant running.
    pub(super) variant: usize,
    /// The recursive read that reads the delta in the running variant.
    pub(super) delta_occ: Option<usize>,
}

/// A relation's tuples while its `Fixpoint` runs: sets, keyed by position.
pub(in crate::exec) struct Relation {
    /// Every tuple derived so far (in the body: the whole relation).
    pub(in crate::exec) tuples: BTreeSet<Vec<Val>>,
    /// The tuples the previous round added (a subset of `tuples`).
    pub(in crate::exec) delta: BTreeSet<Vec<Val>>,
    /// The tuples this round derived that aren't in `tuples` yet.
    pub(in crate::exec) new: BTreeSet<Vec<Val>>,
    /// What one tuple costs ([`tuple_bytes`](crate::exec::val::tuple_bytes)).
    pub(in crate::exec) tuple_bytes: usize,
}

impl Exec<'_, '_> {
    /// Empties relation `id`, releasing its tuples' bytes.
    pub(super) fn clear_rel(&mut self, id: usize) {
        let r = &mut self.rels[id];
        let held = r.tuples.len() + r.delta.len() + r.new.len();
        r.tuples.clear();
        r.delta.clear();
        r.new.clear();
        self.budget
            .release_bytes(held.saturating_mul(r.tuple_bytes));
    }

    /// Drops relation `id`'s delta, releasing its bytes.
    pub(super) fn clear_delta(&mut self, id: usize) {
        let r = &mut self.rels[id];
        let bytes = r.delta.len().saturating_mul(r.tuple_bytes);
        r.delta.clear();
        self.budget.release_bytes(bytes);
    }

    /// The end of a round for relation `id`: its new tuples join it. If
    /// `keep_delta`, they also become its delta (a copy, charged while
    /// held), replacing the previous one.
    pub(super) fn merge_new(&mut self, id: usize, keep_delta: bool) -> Result<(), QueryError> {
        self.clear_delta(id);
        let r = &mut self.rels[id];
        let new = std::mem::take(&mut r.new);
        if keep_delta {
            let copies = new.len().saturating_mul(r.tuple_bytes);
            r.tuples.extend(new.iter().cloned());
            r.delta = new;
            self.budget.charge_bytes(copies)?;
        } else {
            r.tuples.extend(new);
        }
        Ok(())
    }

    /// The first variant from `from` on whose recursive read has a
    /// non-empty delta to read.
    pub(super) fn next_variant(&self, variants: &[Variant], from: usize) -> Option<usize> {
        (from..variants.len()).find(|&i| !self.rels[variants[i].rel].delta.is_empty())
    }

    /// Folds one rule's rows into relation `id`'s tuples new this round as
    /// tuples of the slots `cols` (a tuple the relation already has is not
    /// new): one step per row, new tuples charged while held, and the
    /// relation (with this round's new tuples) bounded by `max_rows` like
    /// any union.
    pub(super) fn add_tuples(
        &mut self,
        id: usize,
        cols: &[usize],
        rows: &BTreeSet<Row>,
    ) -> Result<(), QueryError> {
        let rel = &mut self.rels[id];
        for row in rows {
            self.budget.step(1)?;
            let t: Vec<Val> = cols
                .iter()
                .map(|&c| {
                    row[c]
                        .clone()
                        .expect("invariant: compile checked every rule binds its columns")
                })
                .collect();
            if !rel.tuples.contains(&t) && rel.new.insert(t) {
                self.budget.charge_bytes(rel.tuple_bytes)?;
            }
            if rel.tuples.len() + rel.new.len() > self.budget.limits.max_rows {
                return Err(QueryError::RowLimit {
                    limit: self.budget.limits.max_rows,
                });
            }
        }
        Ok(())
    }

    /// Joins `rows` with relation `id`'s tuples, matched to `args` by
    /// position: constants and bound variables must equal the tuple's
    /// value (looked up in a hash index on those positions, built once for
    /// all rows), free variables bind it (a variable repeated must take
    /// one value), `_` matches anything.
    pub(super) fn rel(
        &mut self,
        id: usize,
        args: &[Pos],
        read: Read,
        rows: BTreeSet<Row>,
        out: &mut BTreeSet<Row>,
    ) -> Result<(), QueryError> {
        let nothing = BTreeSet::new();
        let tuples = match read {
            Read::All => &self.rels[id].tuples,
            Read::Delta => &self.rels[id].delta,
            Read::Nothing => &nothing,
        };
        let keyed: Vec<usize> = (0..args.len()).filter(|&i| args[i].is_bound()).collect();
        // Reading the relation (or indexing it) examines every tuple once.
        self.budget.step(tuples.len() as u64)?;
        let mut index: HashMap<Vec<&Val>, Vec<&Vec<Val>>> = HashMap::new();
        let entry_bytes = if keyed.is_empty() {
            size_of::<&Vec<Val>>()
        } else {
            index_entry_bytes(keyed.len())
        };
        let mut index_bytes = 0usize;
        let mut all = Vec::new();
        for t in tuples {
            self.budget.charge_bytes(entry_bytes)?;
            index_bytes = index_bytes.saturating_add(entry_bytes);
            if keyed.is_empty() {
                all.push(t);
            } else {
                let k: Vec<&Val> = keyed.iter().map(|&i| &t[i]).collect();
                index.entry(k).or_default().push(t);
            }
        }
        for row in rows {
            self.budget.step(1)?;
            let found: &[&Vec<Val>] = if keyed.is_empty() {
                &all
            } else {
                let k: Vec<&Val> = keyed
                    .iter()
                    .map(|&i| {
                        resolve(&args[i], &row)
                            .expect("invariant: compile checked Rel's bound arguments")
                    })
                    .collect();
                index.get(&k).map_or(&[], Vec::as_slice)
            };
            'tuples: for t in found {
                self.budget.step(1)?;
                let mut r = row.clone();
                for (p, x) in args.iter().zip(t.iter()) {
                    if let Pos::Free(s) = p {
                        match &r[*s] {
                            Some(prev) if prev != x => continue 'tuples,
                            Some(_) => {}
                            None => r[*s] = Some(x.clone()),
                        }
                    }
                }
                self.budget.emit(out, r)?;
            }
        }
        drop(index);
        drop(all);
        self.budget.release_bytes(index_bytes);
        Ok(())
    }
}
