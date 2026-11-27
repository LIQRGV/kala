//! The query's step and byte budgets, and the per-row read memo they hold.

use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::Instant;

use kala_core::{EntityId, Tick};

use super::val::{CmpWork, Points, Row, Val};
use super::{Limits, QueryError};

pub(super) fn check_cancel(cancel: &AtomicBool) -> Result<(), QueryError> {
    if cancel.load(AtomicOrdering::Relaxed) {
        Err(QueryError::Cancelled)
    } else {
        Ok(())
    }
}

/// The deadline form of [`check_cancel`], compared with the clock now.
pub(super) fn check_deadline(deadline: Option<Instant>) -> Result<(), QueryError> {
    if deadline.is_some_and(|at| Instant::now() >= at) {
        Err(QueryError::Cancelled)
    } else {
        Ok(())
    }
}

/// How many budget checks share one look at the clock: a check is made for
/// every row and every charge, and reading the clock costs more than most
/// of them.
const CLOCK_EVERY: u32 = 64;

pub(super) struct Budget<'a> {
    pub(super) cancel: &'a AtomicBool,
    /// The run's deadline, if any ([`RunOptions::with_deadline`](super::RunOptions::with_deadline)):
    /// checked where the cancel flag is, the clock read for the first check
    /// and then every [`CLOCK_EVERY`]th.
    pub(super) deadline: Option<Instant>,
    polls: u32,
    pub(super) peak_bytes: usize,
    pub(super) steps: u64,
    /// Bytes held now: live row sets, held scans, interned strings.
    pub(super) bytes: usize,
    /// What one row of this plan's width costs ([`row_bytes`](super::val::row_bytes)).
    pub(super) row_bytes: usize,
    /// Real-order comparison work not yet charged as steps.
    pub(super) cmp: CmpWork,
    /// The per-row read memo, its bytes included in `bytes`. Held here so a
    /// charge that would exceed the budget can drop it first.
    pub(super) memo: Memo,
    pub(super) limits: Limits,
}

impl<'a> Budget<'a> {
    /// A fresh budget: nothing charged yet, and the row size unknown until
    /// the plan is compiled.
    pub(super) fn new(cancel: &'a AtomicBool, limits: Limits) -> Budget<'a> {
        Budget {
            cancel,
            deadline: None,
            polls: 0,
            peak_bytes: 0,
            steps: 0,
            bytes: 0,
            row_bytes: 0,
            cmp: CmpWork::default(),
            memo: Memo::default(),
            limits,
        }
    }

    /// Charges `n` more bytes held. If that exceeds the budget, the memo
    /// (a cache) is dropped first; only then is it an error.
    /// Stops the run if its flag is set or its deadline has passed.
    fn check_stop(&mut self) -> Result<(), QueryError> {
        check_cancel(self.cancel)?;
        if self.deadline.is_some() {
            let poll = self.polls;
            self.polls = poll.wrapping_add(1);
            if poll.is_multiple_of(CLOCK_EVERY) {
                check_deadline(self.deadline)?;
            }
        }
        Ok(())
    }

    pub(super) fn charge_bytes(&mut self, n: usize) -> Result<(), QueryError> {
        self.check_stop()?;
        self.bytes = self.bytes.saturating_add(n);
        if self.bytes > self.limits.max_bytes && self.memo.bytes > 0 {
            self.evict_memo();
        }
        self.peak_bytes = self.peak_bytes.max(self.bytes);
        if self.bytes > self.limits.max_bytes {
            return Err(QueryError::MemoryBudget {
                limit: self.limits.max_bytes,
            });
        }
        Ok(())
    }

    /// Drops the whole memo and releases its bytes. Deterministic: whether
    /// it happens depends only on byte counts.
    fn evict_memo(&mut self) {
        self.release_bytes(self.memo.bytes);
        self.memo = Memo::default();
    }

    /// Charges `n` bytes to the memo, dropping the memo first if they don't
    /// fit; false (nothing charged) if they don't fit even then, and the
    /// caller doesn't keep the entry.
    fn charge_memo(&mut self, n: usize) -> bool {
        if self.bytes.saturating_add(n) > self.limits.max_bytes {
            self.evict_memo();
            if self.bytes.saturating_add(n) > self.limits.max_bytes {
                return false;
            }
        }
        self.bytes += n;
        self.peak_bytes = self.peak_bytes.max(self.bytes);
        self.memo.bytes += n;
        true
    }

    /// The memo of `attr`, with `n` more bytes charged for the entry the
    /// caller is about to insert, or `None` (nothing charged) if it doesn't
    /// fit in the byte budget.
    pub(super) fn memo_slot(&mut self, attr: &str, n: usize) -> Option<&mut AttrMemo> {
        if !self.memo.attrs.contains_key(attr) {
            if !self.charge_memo(MEMO_ENTRY_BYTES.saturating_add(attr.len())) {
                return None;
            }
            self.memo.attrs.insert(attr.to_owned(), AttrMemo::default());
        }
        if !self.charge_memo(n) {
            return None;
        }
        if !self.memo.attrs.contains_key(attr) {
            // Charging `n` dropped the memo, the new slot included.
            self.release_bytes(n);
            self.memo.bytes -= n;
            return None;
        }
        self.memo.attrs.get_mut(attr)
    }

    /// The memo of `attr`, if any of its reads are memoized.
    pub(super) fn memo(&self, attr: &str) -> Option<&AttrMemo> {
        self.memo.attrs.get(attr)
    }

    /// `n` bytes are no longer held.
    pub(super) fn release_bytes(&mut self, n: usize) {
        self.bytes = self.bytes.saturating_sub(n);
    }

    /// A row set of `rows` rows is dropped.
    pub(super) fn release_rows(&mut self, rows: usize) {
        self.release_bytes(rows.saturating_mul(self.row_bytes));
    }

    /// Charges the real-order comparison work done since the last charge
    /// (about one step per 64 bytes, per 4 intervals compared).
    pub(super) fn charge_cmp(&mut self) -> Result<(), QueryError> {
        self.check_stop()?;
        let n = self.cmp.take_steps();
        if n == 0 {
            return Ok(());
        }
        self.step(n)
    }

    pub(super) fn step(&mut self, n: u64) -> Result<(), QueryError> {
        self.check_stop()?;
        self.steps = self.steps.saturating_add(n);
        if self.steps > self.limits.max_steps {
            return Err(QueryError::StepBudget {
                limit: self.limits.max_steps,
            });
        }
        Ok(())
    }

    /// Adds `row` to an operator's output set, charging one step, and its
    /// bytes if it is new to the set.
    pub(super) fn emit(&mut self, out: &mut BTreeSet<Row>, row: Row) -> Result<(), QueryError> {
        self.step(1)?;
        if out.insert(row) {
            self.charge_bytes(self.row_bytes)?;
        }
        if out.len() > self.limits.max_rows {
            return Err(QueryError::RowLimit {
                limit: self.limits.max_rows,
            });
        }
        Ok(())
    }
}

/// Per-row kala-core reads already made by this query, so a read repeated
/// for many rows (the same entity's long string, once per input row) is made,
/// copied and interned once. Every read here depends only on its key and the
/// query's canon, which is fixed, so entries stay valid for the whole query
/// (every `Fixpoint` round included). Charged to the byte budget while held
/// and dropped whole when the budget needs the room ([`Budget::evict_memo`]),
/// so it never makes a query fail.
#[derive(Default)]
pub(super) struct Memo {
    /// Keyed by attribute name as written, so a lookup needn't allocate.
    attrs: HashMap<String, AttrMemo>,
    /// Bytes charged for everything in `attrs`.
    bytes: usize,
}

/// One attribute name's memoized reads (see [`Memo`]).
#[derive(Default)]
pub(super) struct AttrMemo {
    /// Optional property values, including NULL and interned sorted lists.
    pub(super) property: HashMap<(EntityId, Tick), Val>,
    /// `View::as_of(entity, attr, tick)`, values interned.
    pub(super) as_of: HashMap<(EntityId, Tick), Rc<[Val]>>,
    /// `View::as_of_value(entity, attr, value, tick)`.
    pub(super) as_of_value: HashMap<(EntityId, Val, Tick), bool>,
    /// `View::lookup(attr, value, tick)`.
    pub(super) lookup: HashMap<(Val, Tick), Rc<[EntityId]>>,
    /// `View::history(entity, attr)` as the entity's points, interned.
    pub(super) history: HashMap<EntityId, Rc<Points>>,
    /// `View::history(entity, attr)` grouped into `(value, set)`, empty
    /// sets left out.
    pub(super) during: HashMap<EntityId, Rc<[(Val, Val)]>>,
    /// `View::holds_during(entity, attr, value)` as a held set, if not empty.
    pub(super) holds_during: HashMap<(EntityId, Val), Option<Val>>,
}

/// Charged per memo entry (and per attribute's memo) beyond its contents:
/// the hash table's slot and the `Rc` header.
pub(super) const MEMO_ENTRY_BYTES: usize = 64;
