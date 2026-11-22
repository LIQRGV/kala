//! Slice 7, item 0(b): `IntervalSet` and `View::holds_during` (RED step;
//! written from the contract).
//!
//! API under test (new):
//!   `kala_core::IntervalSet` (Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Display)
//!   `IntervalSet::empty() -> IntervalSet`
//!   `IntervalSet::from_intervals<I: IntoIterator<Item = Interval>>(i: I) -> IntervalSet`
//!   `IntervalSet::from_changes(changes: &[(Tick, Op)]) -> IntervalSet`
//!   `intervals(&self) -> &[Interval]`, `is_empty(&self) -> bool`, `len(&self) -> usize`,
//!   `contains(&self, t: Tick) -> bool`,
//!   `intersect/union/difference(&self, other: &IntervalSet) -> IntervalSet`
//!   `View::holds_during(&self, entity: EntityId, attr: &str, value: &Value) -> Result<IntervalSet, Error>`
//!   `World::holds_during(..)` (latest canon), same signature.

use kala_core::*;
use proptest::prelude::*;
use std::collections::BTreeMap;

fn iv(from: i64, until: Option<i64>) -> Interval {
    Interval {
        from: Tick(from),
        until: until.map(Tick),
    }
}

fn set(v: &[(i64, Option<i64>)]) -> IntervalSet {
    IntervalSet::from_intervals(v.iter().map(|&(f, u)| iv(f, u)))
}

fn parts(s: &IntervalSet) -> Vec<(i64, Option<i64>)> {
    s.intervals()
        .iter()
        .map(|i| (i.from.0, i.until.map(|t| t.0)))
        .collect()
}

const A: Op = Op::Assert;
const R: Op = Op::Retract;

fn t(x: i64) -> Tick {
    Tick(x)
}

// ------------------------------------------------------------- examples

#[test]
fn empty_set() {
    let e = IntervalSet::empty();
    assert!(e.is_empty());
    assert_eq!(e.len(), 0);
    assert_eq!(e, IntervalSet::default());
    assert!(!e.contains(t(0)));
    assert_eq!(e.to_string(), "{}");
    assert_eq!(e.intervals(), &[]);
}

#[test]
fn from_intervals_normalizes() {
    // out of order, overlapping, touching, empty and inverted ones
    let s = set(&[
        (10, Some(20)),
        (0, Some(5)),
        (5, Some(8)),
        (15, Some(30)),
        (40, Some(40)),
        (50, Some(45)),
    ]);
    assert_eq!(parts(&s), vec![(0, Some(8)), (10, Some(30))]);
    assert_eq!(s.len(), 2);
    assert!(!s.is_empty());
    assert_eq!(set(&[(0, Some(0))]), IntervalSet::empty());
    // open-ended swallows everything after
    assert_eq!(
        parts(&set(&[(5, None), (10, Some(20)), (0, Some(6))])),
        vec![(0, None)]
    );
    // equal sets compare equal whatever the input shape
    assert_eq!(set(&[(0, Some(5)), (5, Some(9))]), set(&[(0, Some(9))]));
}

#[test]
fn contains_is_half_open() {
    let s = set(&[(-300, Some(-250)), (-200, None)]);
    assert!(s.contains(t(-300)));
    assert!(s.contains(t(-251)));
    assert!(!s.contains(t(-250)));
    assert!(!s.contains(t(-201)));
    assert!(s.contains(t(-200)));
    assert!(s.contains(t(i64::MAX)));
    assert!(!s.contains(t(i64::MIN)));
}

#[test]
fn claude_md_edwin_and_mara_example() {
    let edwin = set(&[(-300, Some(-250)), (-200, Some(-150))]);
    let mara = set(&[(-280, Some(-180))]);
    let both = edwin.intersect(&mara);
    assert_eq!(parts(&both), vec![(-280, Some(-250)), (-200, Some(-180))]);
    assert_eq!(both.to_string(), "{[-280, -250), [-200, -180)}");
    assert_eq!(both, mara.intersect(&edwin));
}

#[test]
fn touching_union_merges_and_intersect_is_empty() {
    let a = set(&[(200, Some(250))]);
    let b = set(&[(250, None)]);
    assert_eq!(parts(&a.union(&b)), vec![(200, None)]);
    assert_eq!(a.intersect(&b), IntervalSet::empty());
    assert_eq!(parts(&a.difference(&b)), vec![(200, Some(250))]);
    assert_eq!(parts(&b.difference(&a)), vec![(250, None)]);
}

#[test]
fn difference_splits_an_interval() {
    let a = set(&[(0, Some(100))]);
    let b = set(&[(10, Some(20)), (30, Some(40))]);
    assert_eq!(
        parts(&a.difference(&b)),
        vec![(0, Some(10)), (20, Some(30)), (40, Some(100))]
    );
    assert_eq!(b.difference(&a), IntervalSet::empty());
    assert_eq!(a.difference(&a), IntervalSet::empty());
    assert_eq!(a.difference(&IntervalSet::empty()), a);
}

#[test]
fn open_ended_on_each_side_of_each_op() {
    let open = set(&[(10, None)]);
    let closed = set(&[(0, Some(20))]);
    assert_eq!(parts(&open.intersect(&closed)), vec![(10, Some(20))]);
    assert_eq!(parts(&closed.intersect(&open)), vec![(10, Some(20))]);
    assert_eq!(parts(&open.union(&closed)), vec![(0, None)]);
    assert_eq!(parts(&closed.union(&open)), vec![(0, None)]);
    assert_eq!(parts(&open.difference(&closed)), vec![(20, None)]);
    assert_eq!(parts(&closed.difference(&open)), vec![(0, Some(10))]);
    let open2 = set(&[(-5, None)]);
    assert_eq!(parts(&open.intersect(&open2)), vec![(10, None)]);
    assert_eq!(parts(&open.union(&open2)), vec![(-5, None)]);
    assert_eq!(parts(&open2.difference(&open)), vec![(-5, Some(10))]);
    assert_eq!(open.difference(&open2), IntervalSet::empty());
    assert_eq!(parts(&open2.difference(&open2)), vec![]);
}

#[test]
fn extreme_ticks() {
    let all = set(&[(i64::MIN, None)]);
    assert!(all.contains(t(i64::MIN)) && all.contains(t(0)) && all.contains(t(i64::MAX)));
    let low = set(&[(i64::MIN, Some(i64::MIN + 1))]);
    assert!(low.contains(t(i64::MIN)));
    assert!(!low.contains(t(i64::MIN + 1)));
    assert_eq!(parts(&all.difference(&low)), vec![(i64::MIN + 1, None)]);
    let top = set(&[(i64::MAX, None)]);
    assert!(top.contains(t(i64::MAX)));
    assert!(!top.contains(t(i64::MAX - 1)));
    assert_eq!(parts(&all.intersect(&top)), vec![(i64::MAX, None)]);
    assert_eq!(
        parts(&low.union(&top)),
        vec![(i64::MIN, Some(i64::MIN + 1)), (i64::MAX, None)]
    );
    assert_eq!(all.to_string(), format!("{{[{}, ∞)}}", i64::MIN));
    // an interval ending at i64::MAX is not open-ended
    let upto = set(&[(0, Some(i64::MAX))]);
    assert!(upto.contains(t(i64::MAX - 1)) && !upto.contains(t(i64::MAX)));
}

#[test]
fn display_format() {
    assert_eq!(set(&[(601, None)]).to_string(), "{[601, ∞)}");
    assert_eq!(
        set(&[(-280, Some(-250)), (-200, Some(-180))]).to_string(),
        "{[-280, -250), [-200, -180)}"
    );
    assert_eq!(
        set(&[(-300, Some(-250)), (601, None)]).to_string(),
        "{[-300, -250), [601, ∞)}"
    );
}

#[test]
fn ord_is_lexicographic_with_none_as_infinity() {
    let a = set(&[(0, Some(5))]);
    let b = set(&[(0, None)]);
    let c = set(&[(1, Some(2))]);
    assert!(a < b);
    assert!(b < c);
    assert!(IntervalSet::empty() < a);
    let longer = set(&[(0, Some(5)), (10, Some(11))]);
    assert!(a < longer); // prefix is smaller
    assert!(longer < b);
    let mut v = vec![
        c.clone(),
        b.clone(),
        longer.clone(),
        a.clone(),
        IntervalSet::empty(),
    ];
    v.sort();
    assert_eq!(v, vec![IntervalSet::empty(), a.clone(), longer, b, c]);
    // Hash agrees with Eq
    let mut h = std::collections::HashSet::new();
    h.insert(set(&[(0, Some(3)), (3, Some(5))]));
    assert!(h.contains(&a.union(&IntervalSet::empty()).union(&set(&[(0, Some(5))]))));
}

#[test]
fn from_changes_examples() {
    let s = IntervalSet::from_changes(&[(t(-300), A), (t(-250), R), (t(-200), A), (t(-150), R)]);
    assert_eq!(parts(&s), vec![(-300, Some(-250)), (-200, Some(-150))]);
    assert_eq!(
        parts(&IntervalSet::from_changes(&[(t(601), A)])),
        vec![(601, None)]
    );
    assert_eq!(
        IntervalSet::from_changes(&[(t(10), R)]),
        IntervalSet::empty()
    );
    assert_eq!(IntervalSet::from_changes(&[]), IntervalSet::empty());
}

#[test]
fn from_changes_any_order_and_repeats() {
    let s = IntervalSet::from_changes(&[(t(-150), R), (t(-300), A), (t(-200), A), (t(-250), R)]);
    assert_eq!(parts(&s), vec![(-300, Some(-250)), (-200, Some(-150))]);
    // duplicate assert while holding does not restart; duplicate retract is harmless
    let s = IntervalSet::from_changes(&[(t(0), A), (t(5), A), (t(10), R), (t(12), R)]);
    assert_eq!(parts(&s), vec![(0, Some(10))]);
    // the same op repeated at one tick
    let s = IntervalSet::from_changes(&[(t(0), A), (t(0), A), (t(4), R), (t(4), R)]);
    assert_eq!(parts(&s), vec![(0, Some(4))]);
    // a leading retract is ignored
    let s = IntervalSet::from_changes(&[(t(-5), R), (t(0), A)]);
    assert_eq!(parts(&s), vec![(0, None)]);
    let s = IntervalSet::from_changes(&[(t(i64::MIN), A), (t(i64::MAX), R)]);
    assert_eq!(parts(&s), vec![(i64::MIN, Some(i64::MAX))]);
}

// ------------------------------------------------------------- proptests

fn arb_set() -> impl Strategy<Value = Vec<(i64, Option<i64>)>> {
    proptest::collection::vec(
        (
            -16i64..=16,
            prop_oneof![1 => Just(None), 3 => (-16i64..=16).prop_map(Some)],
        ),
        0..6,
    )
}

fn model(v: &[(i64, Option<i64>)], x: i64) -> bool {
    v.iter().any(|&(f, u)| f <= x && u.is_none_or(|u| x < u))
}

fn in_normal_form(s: &IntervalSet) -> bool {
    let iv = s.intervals();
    for (i, w) in iv.iter().enumerate() {
        if w.until.is_some_and(|u| u <= w.from) {
            return false;
        }
        if let Some(n) = iv.get(i + 1) {
            match w.until {
                None => return false,
                Some(u) => {
                    if u >= n.from {
                        return false; // overlapping or touching
                    }
                }
            }
        }
    }
    s.len() == iv.len() && s.is_empty() == iv.is_empty()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn ops_match_tick_by_tick_model(a in arb_set(), b in arb_set()) {
        let (sa, sb) = (set(&a), set(&b));
        let (i, u, d) = (sa.intersect(&sb), sa.union(&sb), sa.difference(&sb));
        for s in [&sa, &sb, &i, &u, &d] {
            prop_assert!(in_normal_form(s), "{s}");
        }
        for x in -20..=20 {
            let (ma, mb) = (model(&a, x), model(&b, x));
            prop_assert_eq!(sa.contains(t(x)), ma, "contains {} {}", x, sa);
            prop_assert_eq!(i.contains(t(x)), ma && mb, "intersect {}", x);
            prop_assert_eq!(u.contains(t(x)), ma || mb, "union {}", x);
            prop_assert_eq!(d.contains(t(x)), ma && !mb, "difference {}", x);
        }
        prop_assert_eq!(sa.is_empty(), (-20..=20).all(|x| !model(&a, x)) && !a.iter().any(|&(_, u)| u.is_none()));
        // extremes agree for open-ended members
        prop_assert_eq!(sa.contains(t(i64::MAX)), a.iter().any(|&(_, u)| u.is_none()));
        prop_assert_eq!(sa.contains(t(i64::MIN)), false);
        // algebra
        prop_assert_eq!(sa.union(&sb), sb.union(&sa));
        prop_assert_eq!(sa.intersect(&sb), sb.intersect(&sa));
        prop_assert_eq!(sa.difference(&sb).union(&sa.intersect(&sb)), sa.clone());
    }

    #[test]
    fn from_intervals_is_order_independent(mut a in arb_set()) {
        let s = set(&a);
        a.reverse();
        prop_assert_eq!(&s, &set(&a));
        prop_assert_eq!(s.to_string() == "{}", s.is_empty());
        prop_assert_eq!(s.clone(), IntervalSet::from_intervals(s.intervals().iter().copied()));
    }

    #[test]
    fn from_changes_matches_latest_wins_replay(
        raw in proptest::collection::vec((-16i64..=16, any::<bool>()), 0..12),
        rot in 0usize..12,
    ) {
        // one op per tick (a same-tick conflict is a store error), plus
        // repeated identical entries
        let mut per_tick: BTreeMap<i64, bool> = BTreeMap::new();
        for &(tick, is_assert) in &raw {
            per_tick.entry(tick).or_insert(is_assert);
        }
        let mut changes: Vec<(Tick, Op)> = raw
            .iter()
            .map(|&(tick, _)| (t(tick), if per_tick[&tick] { A } else { R }))
            .collect();
        if !changes.is_empty() {
            let k = rot % changes.len();
            changes.rotate_left(k);
        }
        let s = IntervalSet::from_changes(&changes);
        prop_assert!(in_normal_form(&s), "{s}");
        for x in -20..=20 {
            let latest = per_tick.range(..=x).next_back().map(|(_, &a)| a).unwrap_or(false);
            prop_assert_eq!(s.contains(t(x)), latest, "tick {} set {}", x, s);
        }
        let last = per_tick.iter().next_back().map(|(_, &a)| a).unwrap_or(false);
        prop_assert_eq!(s.contains(t(i64::MAX)), last);
    }
}

// ------------------------------------------------------------- holds_during

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        )
}

#[test]
fn holds_during_matches_from_changes_of_history_across_a_retcon() {
    let mut w = World::in_memory(schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
    let (v, a) = (Value::Ref(varrow), Value::Ref(ashford));
    w.assert(greg, "located_in", v.clone(), Some(t(-300)))
        .unwrap();
    let ret = w
        .retract(greg, "located_in", v.clone(), Some(t(-250)))
        .unwrap()
        .entries[0];
    w.assert(greg, "located_in", v.clone(), Some(t(-200)))
        .unwrap();
    w.assert(greg, "located_in", a.clone(), Some(t(601)))
        .unwrap();
    let before = w.latest_tx();
    w.correct(
        ret,
        Correction::Replace {
            value: None,
            tick: Some(t(-280)),
            op: None,
            precision: None,
        },
    )
    .unwrap();
    let after = w.latest_tx();

    let want = |tx: TxId, val: &Value| {
        let ch: Vec<(Tick, Op)> = w
            .at_canon(tx)
            .history(greg, "located_in")
            .unwrap()
            .into_iter()
            .filter(|c| &c.value == val)
            .map(|c| (c.tick, c.op))
            .collect();
        IntervalSet::from_changes(&ch)
    };
    let got = |tx: TxId, val: &Value| {
        w.at_canon(tx)
            .holds_during(greg, "located_in", val)
            .unwrap()
    };

    assert_eq!(
        parts(&got(before, &v)),
        vec![(-300, Some(-250)), (-200, None)]
    );
    assert_eq!(
        parts(&got(after, &v)),
        vec![(-300, Some(-280)), (-200, None)]
    );
    assert_eq!(parts(&got(after, &a)), vec![(601, None)]);
    for tx in [before, after] {
        for val in [&v, &a] {
            assert_eq!(got(tx, val), want(tx, val));
        }
    }
    // world-level read is the latest canon
    assert_eq!(
        w.holds_during(greg, "located_in", &v).unwrap(),
        got(after, &v)
    );
    // a value never written: empty
    assert_eq!(
        w.holds_during(greg, "located_in", &Value::Ref(greg))
            .unwrap(),
        IntervalSet::empty()
    );
    assert!(matches!(
        w.holds_during(greg, "nope", &v),
        Err(Error::UnknownAttribute(_))
    ));
}

#[test]
fn holds_during_reads_bidirectional_from_either_side() {
    let mut w = World::in_memory(schema()).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    w.assert(mara, "ally_of", Value::Ref(edwin), Some(t(-208)))
        .unwrap();
    w.retract(edwin, "ally_of", Value::Ref(mara), Some(t(-100)))
        .unwrap();
    let want = vec![(-208, Some(-100))];
    assert_eq!(
        parts(&w.holds_during(edwin, "ally_of", &Value::Ref(mara)).unwrap()),
        want
    );
    assert_eq!(
        parts(&w.holds_during(mara, "ally_of", &Value::Ref(edwin)).unwrap()),
        want
    );
}
