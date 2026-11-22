//! Interval sets: "at the same time" questions over story time.
//!
//! CLAUDE.md "Time in queries": a tick variable only sees change points, so
//! "Edwin and Mara were both in Varrow" is an intersection of the stretches
//! each one holds, not a join on ticks. An [`IntervalSet`] is built from
//! change points during a query and never stored (it is derived data).
//!
//! Every operation is a linear sweep over the sorted endpoints of its
//! inputs, so long histories stay cheap. No operation does arithmetic on
//! ticks, so `i64::MIN` and `i64::MAX` are ordinary endpoints.

use std::cmp::Ordering;
use std::fmt;

use crate::overlap::Interval;
use crate::tick::Tick;
use crate::world::Op;

/// A set of story ticks as half-open intervals `[from, until)`, with
/// `until: None` meaning open-ended (holds forever after `from`).
///
/// Always in normal form: intervals sorted ascending, disjoint and not
/// touching (`[0, 5)` and `[5, 9)` are stored as `[0, 9)`), with no empty
/// interval. So two sets holding the same ticks are equal, hash alike and
/// print alike, whatever shape they were built from.
///
/// Ordered lexicographically over its intervals, each compared by
/// `(from, until)` with an open end (`None`) as +infinity; a prefix sorts
/// first, so the empty set is the smallest.
///
/// Displayed as `{[-280, -250), [-200, -180)}`, an open end as `[601, ∞)`,
/// the empty set as `{}`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct IntervalSet {
    /// Normal form (see the type docs).
    intervals: Vec<Interval>,
}

/// `until` as a sort key: an open end (`None`) is after every tick.
fn end_key(until: Option<Tick>) -> (bool, Tick) {
    match until {
        Some(t) => (false, t),
        None => (true, Tick(i64::MAX)),
    }
}

impl IntervalSet {
    /// The empty set (no tick).
    pub fn empty() -> IntervalSet {
        IntervalSet::default()
    }

    /// The set of ticks covered by any of `intervals`, in any order:
    /// overlapping and touching intervals are merged, empty and inverted
    /// ones (`until <= from`) dropped.
    pub fn from_intervals<I: IntoIterator<Item = Interval>>(intervals: I) -> IntervalSet {
        let mut v: Vec<Interval> = intervals
            .into_iter()
            .filter(|i| i.until.is_none_or(|u| u > i.from))
            .collect();
        v.sort_unstable_by_key(|i| i.from);
        let mut out: Vec<Interval> = Vec::with_capacity(v.len());
        for i in v {
            match out.last_mut() {
                // Overlapping or touching the previous one: extend it.
                Some(last) if last.until.is_none_or(|u| i.from <= u) => {
                    if end_key(i.until) > end_key(last.until) {
                        last.until = i.until;
                    }
                }
                _ => out.push(i),
            }
        }
        IntervalSet { intervals: out }
    }

    /// The ticks where a value holds, from its change points `(tick, op)`
    /// in any order, replayed latest-wins: an assert starts holding (a
    /// repeated assert doesn't restart it), a retract stops it, and a
    /// retract while not holding does nothing. The last op holding is
    /// open-ended.
    ///
    /// At most one op per tick is expected (the store rejects same-tick
    /// conflicts); the same op repeated at one tick is harmless.
    pub fn from_changes(changes: &[(Tick, Op)]) -> IntervalSet {
        let mut sorted = changes.to_vec();
        // Stable, so a (malformed) same-tick conflict keeps its input order.
        sorted.sort_by_key(|&(t, _)| t);
        let mut intervals = Vec::new();
        let mut open: Option<Tick> = None;
        for (t, op) in sorted {
            match (op, open) {
                (Op::Assert, None) => open = Some(t),
                (Op::Retract, Some(from)) => {
                    intervals.push(Interval {
                        from,
                        until: Some(t),
                    });
                    open = None;
                }
                _ => {}
            }
        }
        if let Some(from) = open {
            intervals.push(Interval { from, until: None });
        }
        // Already sorted; this only drops a same-tick `[t, t)`.
        IntervalSet::from_intervals(intervals)
    }

    /// The intervals, in normal form (sorted, disjoint, non-touching,
    /// non-empty).
    pub fn intervals(&self) -> &[Interval] {
        &self.intervals
    }

    /// Whether the set holds no tick.
    pub fn is_empty(&self) -> bool {
        self.intervals.is_empty()
    }

    /// The number of intervals (not ticks) in normal form.
    pub fn len(&self) -> usize {
        self.intervals.len()
    }

    /// Whether tick `t` is in the set (half-open: `from` is in, `until`
    /// isn't). One binary search.
    pub fn contains(&self, t: Tick) -> bool {
        let i = self.intervals.partition_point(|iv| iv.from <= t);
        i > 0 && self.intervals[i - 1].until.is_none_or(|u| t < u)
    }

    /// The ticks in both `self` and `other` (`∩`).
    pub fn intersect(&self, other: &IntervalSet) -> IntervalSet {
        self.sweep(other, |a, b| a && b)
    }

    /// The ticks in `self` or `other` (`∪`).
    pub fn union(&self, other: &IntervalSet) -> IntervalSet {
        self.sweep(other, |a, b| a || b)
    }

    /// The ticks in `self` but not in `other` (`−`).
    pub fn difference(&self, other: &IntervalSet) -> IntervalSet {
        self.sweep(other, |a, b| a && !b)
    }

    /// The set of ticks where `keep(in self, in other)` holds: one sweep
    /// over both sets' endpoints in tick order.
    ///
    /// Each set's endpoints `from0, until0, from1, …` strictly ascend
    /// (normal form), and passing the `k`-th one means "inside" iff `k` is
    /// even. The output changes state at most once per distinct tick, so
    /// its intervals come out sorted, non-empty and non-touching.
    fn sweep(&self, other: &IntervalSet, keep: impl Fn(bool, bool) -> bool) -> IntervalSet {
        let (a, b) = (&self.intervals, &other.intervals);
        let (mut ia, mut ib) = (0usize, 0usize);
        let mut out = Vec::new();
        let mut start: Option<Tick> = None;
        loop {
            let t = match (endpoint(a, ia), endpoint(b, ib)) {
                (None, None) => break,
                (Some(x), None) => x,
                (None, Some(y)) => y,
                (Some(x), Some(y)) => x.min(y),
            };
            if endpoint(a, ia) == Some(t) {
                ia += 1;
            }
            if endpoint(b, ib) == Some(t) {
                ib += 1;
            }
            let inside = keep(ia % 2 == 1, ib % 2 == 1);
            match (inside, start) {
                (true, None) => start = Some(t),
                (false, Some(from)) => {
                    out.push(Interval {
                        from,
                        until: Some(t),
                    });
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(from) = start {
            out.push(Interval { from, until: None });
        }
        IntervalSet { intervals: out }
    }
}

/// The `k`-th endpoint of normal-form `intervals` (`from` for even `k`,
/// `until` for odd), `None` past the last one (an open end has none).
fn endpoint(intervals: &[Interval], k: usize) -> Option<Tick> {
    let iv = intervals.get(k / 2)?;
    if k.is_multiple_of(2) {
        Some(iv.from)
    } else {
        iv.until
    }
}

impl Ord for IntervalSet {
    fn cmp(&self, other: &IntervalSet) -> Ordering {
        let key = |i: &Interval| (i.from, end_key(i.until));
        self.intervals
            .iter()
            .map(key)
            .cmp(other.intervals.iter().map(key))
    }
}

impl PartialOrd for IntervalSet {
    fn partial_cmp(&self, other: &IntervalSet) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for IntervalSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{")?;
        for (n, i) in self.intervals.iter().enumerate() {
            if n > 0 {
                f.write_str(", ")?;
            }
            match i.until {
                Some(u) => write!(f, "[{}, {})", i.from.0, u.0)?,
                None => write!(f, "[{}, ∞)", i.from.0)?,
            }
        }
        f.write_str("}")
    }
}
