//! Slice 7, item 5 (CONFIRM): edge cases of history patterns `[e a v t op]`.
//!
//! - inverse names with the entity bound, bidirectional history from both
//!   sides, extreme ticks;
//! - the op-constant contract: in a predicate whose other operand is an op
//!   variable, `:assert` / `:retract` are op constants, never handle lookups;
//! - a byte budget, recursion over history patterns, a ref value bound by an
//!   earlier pattern;
//! - a property test: the pattern's rows equal `View::history_scan`.

use kala_core::*;
use kala_datalog::prepare;
use kala_query::{Datum, Limits, Params, QueryError, run};
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

type Rows = BTreeSet<Vec<Datum>>;

struct W {
    w: World,
    ids: BTreeMap<String, EntityId>,
}

impl W {
    fn r(&self, n: &str) -> Datum {
        Datum::Value(Value::Ref(self.ids[n]))
    }
}

fn i(n: i64) -> Datum {
    Datum::Value(Value::I64(n))
}
fn a() -> Datum {
    Datum::Op(Op::Assert)
}
fn x() -> Datum {
    Datum::Op(Op::Retract)
}
fn tk(n: i64) -> Option<Tick> {
    Some(Tick(n))
}
fn rows(rs: &[&[Datum]]) -> Rows {
    rs.iter().map(|r| r.to_vec()).collect()
}

fn schema() -> Schema {
    let at = |n, t, k, c| AttrDef::new(n, t, k, c);
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(at("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(at("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional())
        .attribute(
            at("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .directed()
                .inverse("child_of"),
        )
}

fn world(chars: &[&str], places: &[&str]) -> W {
    let mut w = World::in_memory(schema()).unwrap();
    let mut ids = BTreeMap::new();
    for h in chars {
        ids.insert(
            h.to_string(),
            w.create_entity("CHARACTER", Some(h)).unwrap(),
        );
    }
    for h in places {
        ids.insert(h.to_string(), w.create_entity("PLACE", Some(h)).unwrap());
    }
    W { w, ids }
}

fn put(f: &mut W, e: &str, attr: &str, v: &str, tick: i64, op: Op) {
    let (e, v) = (f.ids[e], Value::Ref(f.ids[v]));
    match op {
        Op::Assert => f.w.assert(e, attr, v, tk(tick)).unwrap(),
        Op::Retract => f.w.retract(e, attr, v, tk(tick)).unwrap(),
    };
}

fn try_run(w: &World, text: &str, lim: &Limits) -> Result<Rows, QueryError> {
    let p = prepare(w, text).unwrap_or_else(|e| panic!("prepare failed for {text}\n  {e}"));
    run(w, &p.plan, &Params::new(), lim).map(|rs| rs.rows.into_iter().collect())
}

fn go(w: &World, text: &str) -> Rows {
    try_run(w, text, &Limits::default()).unwrap_or_else(|e| panic!("run failed for {text}\n  {e}"))
}

// ------------------------------------------------------------ inverse names

#[test]
fn an_inverse_name_with_the_entity_bound() {
    let mut f = world(&["edwin", "mara", "lisa"], &[]);
    put(&mut f, "edwin", "parent_of", "mara", -200, Op::Assert);
    put(&mut f, "edwin", "parent_of", "mara", -100, Op::Retract);
    put(&mut f, "lisa", "parent_of", "mara", -150, Op::Assert);
    put(&mut f, "lisa", "parent_of", "edwin", -90, Op::Assert);
    let text = "[:find ?p ?t ?op :where [:mara :child_of ?p ?t ?op]]";
    assert_eq!(
        go(&f.w, text),
        rows(&[
            &[f.r("edwin"), i(-200), a()],
            &[f.r("edwin"), i(-100), x()],
            &[f.r("lisa"), i(-150), a()],
        ])
    );
    // the value bound too
    let text = "[:find ?t ?op :where [:mara :child_of :lisa ?t ?op]]";
    assert_eq!(go(&f.w, text), rows(&[&[i(-150), a()]]));
    // nobody is Lisa's parent
    let text = "[:find ?p :where [:lisa :child_of ?p ?t]]";
    assert!(go(&f.w, text).is_empty());
}

// ------------------------------------------------------------ bidirectional

#[test]
fn bidirectional_history_reads_from_both_sides() {
    let mut f = world(&["edwin", "mara", "lisa"], &[]);
    // written from edwin's side, retracted from mara's side: one edge
    put(&mut f, "edwin", "ally_of", "mara", -208, Op::Assert);
    put(&mut f, "mara", "ally_of", "edwin", -100, Op::Retract);
    // written from mara's side
    put(&mut f, "mara", "ally_of", "lisa", -50, Op::Assert);

    let q = |who: &str| format!("[:find ?x ?t ?op :where [:{who} :ally_of ?x ?t ?op]]");
    assert_eq!(
        go(&f.w, &q("mara")),
        rows(&[
            &[f.r("edwin"), i(-208), a()],
            &[f.r("edwin"), i(-100), x()],
            &[f.r("lisa"), i(-50), a()],
        ])
    );
    assert_eq!(
        go(&f.w, &q("edwin")),
        rows(&[&[f.r("mara"), i(-208), a()], &[f.r("mara"), i(-100), x()]])
    );
    assert_eq!(go(&f.w, &q("lisa")), rows(&[&[f.r("mara"), i(-50), a()]]));
    // all pairs, both directions, equal to the core scan
    let text = "[:find ?a ?b ?t ?op :where [?a :ally_of ?b ?t ?op]]";
    let want: Rows =
        f.w.history_scan("ally_of")
            .unwrap()
            .into_iter()
            .map(|(e, c)| {
                vec![
                    Datum::Value(Value::Ref(e)),
                    Datum::Value(c.value),
                    i(c.tick.0),
                    Datum::Op(c.op),
                ]
            })
            .collect();
    assert_eq!(want.len(), 6, "sanity: 3 change points, each from 2 sides");
    assert_eq!(go(&f.w, text), want);
}

// ------------------------------------------------------------ extreme ticks

#[test]
fn extreme_and_zero_ticks_are_ordinary_change_points() {
    let mut f = world(&["greg"], &["varrow", "ashford", "carnath"]);
    put(&mut f, "greg", "located_in", "varrow", i64::MIN, Op::Assert);
    put(&mut f, "greg", "located_in", "varrow", 0, Op::Retract);
    put(&mut f, "greg", "located_in", "ashford", 0, Op::Assert);
    put(
        &mut f,
        "greg",
        "located_in",
        "ashford",
        i64::MAX,
        Op::Retract,
    );
    put(
        &mut f,
        "greg",
        "located_in",
        "carnath",
        i64::MAX,
        Op::Assert,
    );
    let text = "[:find ?p ?t ?op :where [:greg :located_in ?p ?t ?op]]";
    assert_eq!(
        go(&f.w, text),
        rows(&[
            &[f.r("varrow"), i(i64::MIN), a()],
            &[f.r("varrow"), i(0), x()],
            &[f.r("ashford"), i(0), a()],
            &[f.r("ashford"), i(i64::MAX), x()],
            &[f.r("carnath"), i(i64::MAX), a()],
        ])
    );
    // constant ticks at the extremes
    for (t, who) in [
        (i64::MIN.to_string(), "varrow"),
        ("0".to_string(), "ashford"),
        (i64::MAX.to_string(), "carnath"),
    ] {
        let text = format!("[:find ?p :where [:greg :located_in ?p {t} :assert]]");
        assert_eq!(go(&f.w, &text), rows(&[&[f.r(who)]]), "tick {t}");
    }
    // a bound tick is an ordinary i64 in a predicate
    let text = format!(
        "[:find ?p :where [:greg :located_in ?p ?t] [(= ?t {})]]",
        i64::MAX
    );
    assert_eq!(
        go(&f.w, &text),
        rows(&[&[f.r("ashford")], &[f.r("carnath")]])
    );
    let text = "[:find ?t :where [:greg :located_in :varrow ?t] [(< ?t 0)]]";
    assert_eq!(go(&f.w, text), rows(&[&[i(i64::MIN)]]));
}

// ------------------------------------------------- op constants in predicates

fn arm_world(extra_handles: &[&str]) -> W {
    let mut f = world(&["greg", "edwin"], &["varrow", "ashford"]);
    for h in extra_handles {
        f.ids.insert(
            h.to_string(),
            f.w.create_entity("CHARACTER", Some(h)).unwrap(),
        );
    }
    put(&mut f, "greg", "located_in", "varrow", -300, Op::Assert);
    put(&mut f, "greg", "located_in", "varrow", -250, Op::Retract);
    put(&mut f, "greg", "located_in", "ashford", -250, Op::Assert);
    put(&mut f, "edwin", "located_in", "varrow", -300, Op::Assert);
    put(&mut f, "edwin", "located_in", "varrow", -200, Op::Retract);
    f
}

#[test]
fn op_constants_in_a_predicate_are_not_handle_lookups() {
    // no entity has the handle `assert` or `retract`
    let f = arm_world(&[]);
    let retracts = rows(&[&[f.r("greg"), i(-250)], &[f.r("edwin"), i(-200)]]);
    let asserts = rows(&[
        &[f.r("greg"), i(-300)],
        &[f.r("greg"), i(-250)],
        &[f.r("edwin"), i(-300)],
    ]);
    for (pred, want) in [
        ("[(= ?op :retract)]", &retracts),
        ("[(= :retract ?op)]", &retracts),
        ("[(!= ?op :assert)]", &retracts),
        ("[(= ?op :assert)]", &asserts),
        ("[(!= ?op :retract)]", &asserts),
    ] {
        let text = format!("[:find ?g ?t :where [?g :located_in ?p ?t ?op] {pred}]");
        assert_eq!(&go(&f.w, &text), want, "{pred}");
    }
}

#[test]
fn op_constants_win_even_when_an_entity_has_that_handle() {
    let f = arm_world(&["assert", "retract"]);
    let text = "[:find ?g ?t :where [?g :located_in ?p ?t ?op] [(= ?op :retract)]]";
    assert_eq!(
        go(&f.w, text),
        rows(&[&[f.r("greg"), i(-250)], &[f.r("edwin"), i(-200)]])
    );
}

// ------------------------------------------------------------- byte budget

#[test]
fn a_history_scan_under_a_small_byte_budget_is_a_memory_budget_error() {
    let mut f = world(&[], &["varrow"]);
    {
        let mut t = f.w.begin();
        for n in 0..300 {
            let e = t.create_entity("CHARACTER", None).unwrap();
            let p = f.ids["varrow"];
            t.assert(e, "located_in", Value::Ref(p), tk(n)).unwrap();
            t.retract(e, "located_in", Value::Ref(p), tk(n + 1000))
                .unwrap();
        }
        t.commit().unwrap();
    }
    let text = "[:find ?g ?t ?op :where [?g :located_in ?p ?t ?op]]";
    assert_eq!(go(&f.w, text).len(), 600); // OK under the default budget
    let lim = Limits {
        max_bytes: 4 * 1024,
        ..Limits::default()
    };
    match try_run(&f.w, text, &lim) {
        Err(QueryError::MemoryBudget { limit }) => assert_eq!(limit, 4 * 1024),
        other => panic!("expected MemoryBudget, got {other:?}"),
    }
}

// --------------------------------------------- recursion over history patterns

#[test]
fn a_recursive_rule_over_history_patterns_follows_chains_of_moves() {
    let mut f = world(
        &["greg", "lisa", "edwin"],
        &["varrow", "ashford", "carnath"],
    );
    // moves (retract of one place and assert of another at the SAME tick):
    //   greg: varrow -> ashford @-250      => (varrow, ashford)
    //   lisa: ashford -> carnath @-100     => (ashford, carnath)
    // decoy: edwin leaves carnath @-50 and arrives in varrow @-40: not a move.
    // Arrivals alone (greg's varrow @-300, lisa's ashford @-260, edwin's
    // carnath @-60) have no retract at their tick: not moves.
    put(&mut f, "greg", "located_in", "varrow", -300, Op::Assert);
    put(&mut f, "greg", "located_in", "varrow", -250, Op::Retract);
    put(&mut f, "greg", "located_in", "ashford", -250, Op::Assert);
    put(&mut f, "lisa", "located_in", "ashford", -260, Op::Assert);
    put(&mut f, "lisa", "located_in", "ashford", -100, Op::Retract);
    put(&mut f, "lisa", "located_in", "carnath", -100, Op::Assert);
    put(&mut f, "edwin", "located_in", "carnath", -60, Op::Assert);
    put(&mut f, "edwin", "located_in", "carnath", -50, Op::Retract);
    put(&mut f, "edwin", "located_in", "varrow", -40, Op::Assert);

    let step = "[?a :located_in ?x ?t :retract] [?a :located_in ?y ?t :assert]";
    let rules = format!(
        "[(chain ?x ?y) {step}] \
         [(chain ?x ?z) (chain ?x ?y) \
          [?b :located_in ?y ?u :retract] [?b :located_in ?z ?u :assert]]"
    );
    let text = format!("[:find ?x ?y :rules [{rules}] :where (chain ?x ?y)]");
    // hand-computed: (varrow,ashford), (ashford,carnath), and their composition
    assert_eq!(
        go(&f.w, &text),
        rows(&[
            &[f.r("varrow"), f.r("ashford")],
            &[f.r("ashford"), f.r("carnath")],
            &[f.r("varrow"), f.r("carnath")],
        ])
    );
}

// ------------------------------------------- a ref value bound by an earlier pattern

#[test]
fn a_ref_value_bound_by_an_earlier_pattern() {
    let mut f = world(&["greg", "edwin", "mara"], &["varrow", "ashford"]);
    put(&mut f, "greg", "located_in", "varrow", -300, Op::Assert);
    put(&mut f, "greg", "located_in", "varrow", -250, Op::Retract);
    put(&mut f, "edwin", "located_in", "varrow", -300, Op::Assert);
    put(&mut f, "mara", "located_in", "varrow", -280, Op::Assert);
    put(&mut f, "mara", "located_in", "ashford", -100, Op::Assert);
    // where Edwin is at -270 (varrow), then every change point there
    let text = "[:find ?g ?t ?op :at -270 :where \
                [:edwin :located_in ?p] [?g :located_in ?p ?t ?op]]";
    assert_eq!(
        go(&f.w, text),
        rows(&[
            &[f.r("greg"), i(-300), a()],
            &[f.r("greg"), i(-250), x()],
            &[f.r("edwin"), i(-300), a()],
            &[f.r("mara"), i(-280), a()],
        ])
    );
    // the entity bound by an earlier pattern, the place left free
    let text = "[:find ?p ?t :where [?c :located_in :ashford] [?c :located_in ?p ?t :assert]]";
    assert_eq!(
        go(&f.w, text),
        rows(&[&[f.r("varrow"), i(-280)], &[f.r("ashford"), i(-100)]])
    );
}

// --------------------------------------------------- property: == history_scan

type WriteSpec = (usize, usize, i64, bool);
type CorrSpec = (usize, Option<i64>);

fn spec() -> impl Strategy<Value = (Vec<WriteSpec>, Vec<CorrSpec>, usize)> {
    (
        prop::collection::vec((0usize..3, 0usize..3, -4i64..5, any::<bool>()), 0..20),
        prop::collection::vec((0usize..64, prop::option::of(-4i64..5)), 0..5),
        0usize..64,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn history_pattern_rows_equal_history_scan_at_any_canon(
        (writes, corrections, canon_pick) in spec()
    ) {
        let mut f = world(&["c0", "c1", "c2"], &["p0", "p1", "p2"]);
        let mut entries = Vec::new();
        for (c, p, t, assert) in writes {
            let (e, v) = (f.ids[&format!("c{c}")], Value::Ref(f.ids[&format!("p{p}")]));
            // same-tick conflicts are errors: ignore them
            let res = if assert {
                f.w.assert(e, "located_in", v, tk(t))
            } else {
                f.w.retract(e, "located_in", v, tk(t))
            };
            if let Ok(r) = res {
                entries.extend(r.entries);
            }
        }
        for (pick, new_tick) in corrections {
            if entries.is_empty() {
                break;
            }
            let entry = entries[pick % entries.len()];
            let c = match new_tick {
                None => Correction::Delete,
                Some(t) => Correction::Replace {
                    value: None,
                    tick: tk(t),
                    op: None,
                    precision: None,
                },
            };
            if let Ok(r) = f.w.correct(entry, c) {
                entries.extend(r.entries);
            }
        }
        let latest = f.w.latest_tx().0;
        let canon = (canon_pick as u64) % (latest + 1);
        let want: Rows = f
            .w
            .at_canon(TxId(canon))
            .history_scan("located_in")
            .unwrap()
            .into_iter()
            .map(|(e, c)| vec![
                Datum::Value(Value::Ref(e)),
                Datum::Value(c.value),
                i(c.tick.0),
                Datum::Op(c.op),
            ])
            .collect();
        let text = format!(
            "[:find ?e ?v ?t ?op :canon {canon} :where [?e :located_in ?v ?t ?op]]"
        );
        prop_assert_eq!(go(&f.w, &text), want);
    }
}
