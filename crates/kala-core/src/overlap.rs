//! The interval overlap check behind cardinality warnings.
//!
//! Cardinality is a label, not a rule (CLAUDE.md "Attribute schema"): two
//! values of a cardinality-one attribute that hold at the same time are
//! reported, never refused or fixed. The check works on the whole timeline,
//! not one tick, because out-of-order writes (prequels) can create an
//! overlap anywhere in the story: each value's change points become
//! half-open intervals `[from, until)` and every pair of values whose
//! intervals intersect is an overlap. Touching intervals (`[200, 250)` and
//! `[250, ∞)`) don't intersect, and a gap (no value at all) is never
//! reported.
//!
//! This module is the one implementation of that check: [`View::overlaps`]
//! (and through it write warnings and doctor) builds on [`value_intervals`]
//! and [`find_overlaps`].
//!
//! [`View::overlaps`]: crate::View::overlaps

use crate::id::EntityId;
use crate::tick::Tick;
use crate::value::Value;
use crate::world::{Change, Op};

/// A half-open stretch of story time `[from, until)`; `until: None` is
/// open-ended (holds forever after `from`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Interval {
    /// First tick inside the interval.
    pub from: Tick,
    /// First tick past the interval; `None` = infinity.
    pub until: Option<Tick>,
}

impl Interval {
    /// Whether `self` ends before `other` does (`None` = infinity is last).
    fn ends_before(&self, other: &Interval) -> bool {
        match (self.until, other.until) {
            (Some(a), Some(b)) => a < b,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// The intersection of two intervals, `None` if it is empty (disjoint
    /// or touching).
    fn intersect(&self, other: &Interval) -> Option<Interval> {
        let from = self.from.max(other.from);
        let until = if self.ends_before(other) {
            self.until
        } else {
            other.until
        };
        match until {
            Some(u) if u <= from => None,
            _ => Some(Interval { from, until }),
        }
    }
}

/// Which side of an attribute an overlap is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    /// The entity's own values (`E|entity|attr|`): Greg is in Varrow and
    /// Ashford. For a bidirectional attribute, the entity's partners from
    /// either side.
    Forward,
    /// The sources pointing at the entity through a directed `ref`
    /// attribute with a reverse cardinality of one (`R|entity|attr|`):
    /// Carnath is ruled by Edwin and Mara.
    Reverse,
}

/// Two values of one cardinality-one side that hold at the same time.
#[derive(Clone, Debug, PartialEq)]
pub struct Overlap {
    /// The entity whose side has two values at once (for
    /// [`Side::Reverse`], the target).
    pub entity: EntityId,
    /// The attribute's name.
    pub attr: String,
    /// Forward (the entity's values) or reverse (its sources).
    pub side: Side,
    /// The value with the smaller encoding (for [`Side::Reverse`],
    /// `Ref(source)`).
    pub a: Value,
    /// The value with the larger encoding.
    pub b: Value,
    /// One maximal stretch during which both hold.
    pub during: Interval,
}

/// Builds each value's holding intervals from change points, as
/// [`View::history`](crate::View::history) returns them.
///
/// The changes are applied in the given order, latest wins: an assert
/// starts an interval unless the value already holds (a repeated assert
/// doesn't restart it), a retract ends the open one, and a retract of a
/// value that doesn't hold adds nothing. At most one change per
/// `(value, tick)` is expected (history guarantees it). Adjacent intervals
/// (a retract and a re-assert at the same tick) are merged. The result is
/// sorted by value encoding with each value's intervals ascending and
/// disjoint; a value that never held (only retracts) is left out.
pub fn value_intervals(changes: &[Change]) -> Vec<(Value, Vec<Interval>)> {
    struct Acc {
        /// Sort key: the value's encoding. `None` only for an unencodable
        /// value (NaN), which no stored fact has; those sort first.
        key: Option<Vec<u8>>,
        value: Value,
        intervals: Vec<Interval>,
        open: Option<Tick>,
    }
    let mut accs: Vec<Acc> = Vec::new();
    for c in changes {
        let key = c.value.encode().ok();
        let i = match accs.binary_search_by(|a| a.key.cmp(&key)) {
            Ok(i) => i,
            Err(i) => {
                accs.insert(
                    i,
                    Acc {
                        key,
                        value: c.value.clone(),
                        intervals: Vec::new(),
                        open: None,
                    },
                );
                i
            }
        };
        let acc = &mut accs[i];
        match (c.op, acc.open) {
            (Op::Assert, None) => {
                // Re-asserted where the last interval ended: one interval.
                acc.open = match acc.intervals.last() {
                    Some(last) if last.until == Some(c.tick) => {
                        let from = last.from;
                        acc.intervals.pop();
                        Some(from)
                    }
                    _ => Some(c.tick),
                };
            }
            (Op::Retract, Some(from)) => {
                acc.open = None;
                // Empty only for a retract at the assert's own tick, which
                // history never yields; keep the result well-formed anyway.
                if from < c.tick {
                    acc.intervals.push(Interval {
                        from,
                        until: Some(c.tick),
                    });
                }
            }
            (Op::Assert, Some(_)) | (Op::Retract, None) => {}
        }
    }
    accs.into_iter()
        .filter_map(|mut acc| {
            if let Some(from) = acc.open {
                acc.intervals.push(Interval { from, until: None });
            }
            (!acc.intervals.is_empty()).then_some((acc.value, acc.intervals))
        })
        .collect()
}

/// Every pair of values whose intervals overlap, as `(a, b, during)` with
/// `a` before `b` by value encoding, one row per maximal overlapping
/// stretch. Touching intervals don't overlap.
///
/// Each value's intervals must be ascending and disjoint, with adjacent
/// ones merged, as [`value_intervals`] returns them. Rows come sorted by
/// `(a, b, during.from)`.
pub fn find_overlaps(per_value: &[(Value, Vec<Interval>)]) -> Vec<(Value, Value, Interval)> {
    let sorted = by_encoding(per_value);
    let mut out = Vec::new();
    for (i, (_, a)) in sorted.iter().enumerate() {
        for (_, b) in &sorted[i + 1..] {
            sweep(a, b, &mut out);
        }
    }
    out
}

/// The rows of [`find_overlaps`] in which `a` or `b` is one of the
/// `touched` values (their encodings, ascending and deduplicated), in the
/// same order. Write warnings use it: a commit only changes the intervals
/// of the values it touched, so every other row is the same before and
/// after it. Cost: one sweep per pair with a touched value, i.e.
/// proportional to `values × touched`, not `values²`.
pub(crate) fn find_overlaps_touching(
    per_value: &[(Value, Vec<Interval>)],
    touched: &[Vec<u8>],
) -> Vec<(Value, Value, Interval)> {
    let sorted = by_encoding(per_value);
    let is_touched: Vec<bool> = sorted
        .iter()
        .map(|(key, _)| {
            key.as_ref()
                .is_some_and(|k| touched.binary_search(k).is_ok())
        })
        .collect();
    // Positions (ascending) of the touched values among `sorted`.
    let positions: Vec<usize> = (0..sorted.len()).filter(|&i| is_touched[i]).collect();
    let mut out = Vec::new();
    for (i, (_, a)) in sorted.iter().enumerate() {
        if is_touched[i] {
            // A touched `a`: against every later value.
            for (_, b) in &sorted[i + 1..] {
                sweep(a, b, &mut out);
            }
        } else {
            // An untouched `a`: only against the touched values after it.
            let from = positions.partition_point(|&j| j <= i);
            for &j in &positions[from..] {
                sweep(a, sorted[j].1, &mut out);
            }
        }
    }
    out
}

/// One value's intervals, as [`value_intervals`] lists them.
type ValueIntervals = (Value, Vec<Interval>);

/// `per_value` sorted by value encoding as in [`value_intervals`] (NaN, which
/// doesn't encode, first), each with its encoding.
fn by_encoding(per_value: &[ValueIntervals]) -> Vec<(Option<Vec<u8>>, &ValueIntervals)> {
    let mut sorted: Vec<_> = per_value
        .iter()
        .map(|pv| (pv.0.encode().ok(), pv))
        .collect();
    sorted.sort_by(|x, y| x.0.cmp(&y.0));
    sorted
}

/// Appends the overlaps of two values' interval lists as `(a, b, during)`,
/// ascending by `during.from`: a sweep over both ascending lists that
/// advances whichever interval ends first.
fn sweep(
    (a, a_iv): &(Value, Vec<Interval>),
    (b, b_iv): &(Value, Vec<Interval>),
    out: &mut Vec<(Value, Value, Interval)>,
) {
    let (mut x, mut y) = (0, 0);
    while x < a_iv.len() && y < b_iv.len() {
        if let Some(during) = a_iv[x].intersect(&b_iv[y]) {
            out.push((a.clone(), b.clone(), during));
        }
        if a_iv[x].ends_before(&b_iv[y]) {
            x += 1;
        } else {
            y += 1;
        }
    }
}

/// The overlap warnings a commit introduced and resolved, reported next to
/// its "N changed" count (like compiler warnings). Warnings never block a
/// write: cardinality is a label, not a rule.
///
/// Computed over the entities, attributes and sides the commit touched, at
/// the latest canon, before vs after the commit, by exact [`Overlap`]
/// equality: an overlap whose interval changed is reported as the old one
/// resolved and the new one introduced. Both lists are sorted by
/// `(entity, attr, side, a, b, during.from)`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Warnings {
    /// Overlaps that hold after the commit and didn't before.
    pub introduced: Vec<Overlap>,
    /// Overlaps that held before the commit and don't after.
    pub resolved: Vec<Overlap>,
}
