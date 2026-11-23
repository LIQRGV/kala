//! Slice 7, item 6 (RED): interval sets in queries.
//!
//! Contract (tech-lead decisions):
//! - `(during [e a v] ?ie)` binds `?ie` to the `IntervalSet` where `(e, a, v)`
//!   holds at the query's canon across ALL ticks (`:at` does not apply; inside
//!   `(at ..)` it is `Syntax`). `e` and `v` may be constants, params, bound or
//!   free variables, or `_`; free ones are bound, one row per (entity, value)
//!   that EVER held (non-empty sets only). `a` is an attribute or inverse name;
//!   `:kala_handle` is `Syntax`. An already-bound `?ie` is an equality constraint.
//!   The form is exactly `(during <3-element pattern> ?var)`, else `Syntax`.
//!   Compiles to `Op::During`.
//! - Functions `[(intersect ?a ?b) ?c]`, `[(union ?a ?b) ?c]`,
//!   `[(difference ?a ?b) ?c]` (a minus b); predicates `[(not-empty ?s)]` and
//!   `[(contains ?s T)]` (`T`: i64 variable, integer or `:in` param). Compile to
//!   `Op::Apply`. Wrong arity or unknown name is `Syntax`; a non-interval
//!   argument is `TypeMismatch`; an unbound one `UnboundVariable`. An output
//!   variable that is already bound is an equality constraint.
//! - Interval sets are their own type: in a pattern position or under
//!   `< <= > >=` they are `TypeMismatch`; `=` / `!=` between two are fine.
//! - They are charged to the byte budget by size and the work is metered.
//! - They work in rule bodies and inside `not`.
//!
//! Base world (`edwin_world`, all written through kala-core), `located_in`
//! varrow unless noted:
//!   edwin  A-300 R-250 A-200 R-150      => {[-300,-250), [-200,-150)}
//!   mara   A-280 R-180                  => {[-280,-180)}
//!   greg   A-400 R-310; ashford A-310; carnath A0 R10
//!   tomas  A-250 R-200                  (touches Edwin's intervals, no overlap)
//!   lisa   A-250 (never retracted)      => {[-250, inf)}
//!   wren   ashford A-100 only
//!   edwin bought sword @-208 (event); edwin parent_of mara A-200 R-100;
//!   edwin ally_of mara A-208 R-100 (bidirectional)

use kala_core::*;
use kala_datalog::{ErrorKind, prepare};
use kala_fixture::load_fixture;
use kala_query::{Datum, Limits, Params, QueryError, run};
use std::collections::{BTreeMap, BTreeSet};

type Rows = BTreeSet<Vec<Datum>>;

struct W {
    w: World,
    ids: BTreeMap<&'static str, EntityId>,
}

impl W {
    fn r(&self, n: &str) -> Datum {
        Datum::Value(Value::Ref(self.ids[n]))
    }
}

fn tk(n: i64) -> Option<Tick> {
    Some(Tick(n))
}
fn iv(from: i64, until: Option<i64>) -> Interval {
    Interval {
        from: Tick(from),
        until: until.map(Tick),
    }
}
/// A datum holding the set of the given `(from, until)` intervals.
fn set(ivs: &[(i64, Option<i64>)]) -> Datum {
    Datum::Intervals(IntervalSet::from_intervals(
        ivs.iter().map(|&(a, b)| iv(a, b)),
    ))
}
fn rows(rs: &[&[Datum]]) -> Rows {
    rs.iter().map(|r| r.to_vec()).collect()
}

fn schema() -> Schema {
    let at = |n, t, k, c| AttrDef::new(n, t, k, c);
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .node_type("ITEM")
        .attribute(at("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(at("bought", ValueType::Ref, Kind::Event, Cardinality::Many).directed())
        .attribute(at("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional())
        .attribute(
            at("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .directed()
                .inverse("child_of"),
        )
}

fn fresh() -> W {
    let mut w = World::in_memory(schema()).unwrap();
    let mut ids = BTreeMap::new();
    for h in ["edwin", "mara", "greg", "tomas", "lisa", "wren"] {
        ids.insert(h, w.create_entity("CHARACTER", Some(h)).unwrap());
    }
    for h in ["varrow", "ashford", "carnath"] {
        ids.insert(h, w.create_entity("PLACE", Some(h)).unwrap());
    }
    ids.insert("sword", w.create_entity("ITEM", Some("sword")).unwrap());
    W { w, ids }
}

fn put(f: &mut W, e: &str, attr: &str, v: &str, tick: i64, op: Op) -> EntryId {
    let (e, v) = (f.ids[e], Value::Ref(f.ids[v]));
    let r = match op {
        Op::Assert => f.w.assert(e, attr, v, tk(tick)).unwrap(),
        Op::Retract => f.w.retract(e, attr, v, tk(tick)).unwrap(),
    };
    r.entries[0]
}

fn loc(f: &mut W, who: &str, place: &str, tick: i64, op: Op) -> EntryId {
    put(f, who, "located_in", place, tick, op)
}

fn edwin_world() -> W {
    let mut f = fresh();
    use Op::{Assert as A, Retract as R};
    for (t, op) in [(-300, A), (-250, R), (-200, A), (-150, R)] {
        loc(&mut f, "edwin", "varrow", t, op);
    }
    loc(&mut f, "mara", "varrow", -280, A);
    loc(&mut f, "mara", "varrow", -180, R);
    loc(&mut f, "greg", "varrow", -400, A);
    loc(&mut f, "greg", "varrow", -310, R);
    loc(&mut f, "greg", "ashford", -310, A);
    loc(&mut f, "greg", "carnath", 0, A);
    loc(&mut f, "greg", "carnath", 10, R);
    loc(&mut f, "tomas", "varrow", -250, A);
    loc(&mut f, "tomas", "varrow", -200, R);
    loc(&mut f, "lisa", "varrow", -250, A);
    loc(&mut f, "wren", "ashford", -100, A);
    put(&mut f, "edwin", "bought", "sword", -208, A);
    put(&mut f, "edwin", "parent_of", "mara", -200, A);
    put(&mut f, "edwin", "parent_of", "mara", -100, R);
    put(&mut f, "edwin", "ally_of", "mara", -208, A);
    put(&mut f, "edwin", "ally_of", "mara", -100, R);
    f
}

fn try_run(
    w: &World,
    text: &str,
    params: &[(&str, Value)],
    lim: &Limits,
) -> Result<Rows, QueryError> {
    let p = prepare(w, text).unwrap_or_else(|e| panic!("prepare failed for {text}\n  {e}"));
    let ps: Params = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    run(w, &p.plan, &ps, lim).map(|rs| rs.rows.into_iter().collect())
}

fn go(w: &World, text: &str) -> Rows {
    try_run(w, text, &[], &Limits::default())
        .unwrap_or_else(|e| panic!("run failed for {text}\n  {e}"))
}

fn go_p(w: &World, text: &str, params: &[(&str, Value)]) -> Rows {
    try_run(w, text, params, &Limits::default())
        .unwrap_or_else(|e| panic!("run failed for {text}\n  {e}"))
}

fn kind(w: &World, text: &str) -> ErrorKind {
    match prepare(w, text) {
        Ok(_) => panic!("expected an error for {text}"),
        Err(e) => e.kind,
    }
}

const EDWIN: &str = "(during [:edwin :located_in :varrow] ?ie)";
const MARA: &str = "(during [:mara :located_in :varrow] ?im)";

// ----------------------------------------------------- the CLAUDE.md example

#[test]
fn edwin_and_mara_in_varrow_at_the_same_time() {
    let f = edwin_world();
    let text = format!(
        "[:find ?both :where {EDWIN} {MARA} [(intersect ?ie ?im) ?both] [(not-empty ?both)]]"
    );
    let want = IntervalSet::from_intervals([iv(-280, Some(-250)), iv(-200, Some(-180))]);
    assert_eq!(want.to_string(), "{[-280, -250), [-200, -180)}");
    assert_eq!(go(&f.w, &text), rows(&[&[Datum::Intervals(want)]]));
}

#[test]
fn during_alone_returns_the_interval_set() {
    let f = edwin_world();
    assert_eq!(
        go(&f.w, &format!("[:find ?ie :where {EDWIN}]")),
        rows(&[&[set(&[(-300, Some(-250)), (-200, Some(-150))])]])
    );
    // the core API agrees (only the query is under test)
    let core =
        f.w.holds_during(f.ids["edwin"], "located_in", &Value::Ref(f.ids["varrow"]))
            .unwrap();
    assert_eq!(core.to_string(), "{[-300, -250), [-200, -150)}");
}

#[test]
fn the_query_default_tick_does_not_apply_to_during() {
    let f = edwin_world();
    let want = rows(&[&[set(&[(-300, Some(-250)), (-200, Some(-150))])]]);
    for at in ["-400", "-260", "0", "99999"] {
        let text = format!("[:find ?ie :at {at} :where {EDWIN}]");
        assert_eq!(go(&f.w, &text), want, ":at {at}");
    }
}

// --------------------------------------------------------- union, difference

#[test]
fn union_and_difference_of_two_sets() {
    let f = edwin_world();
    let q = |f: &str| format!("[:find ?r :where {EDWIN} {MARA} [({f} ?ie ?im) ?r]]");
    // edwin [-300,-250) [-200,-150); mara [-280,-180)
    assert_eq!(
        go(&f.w, &q("union")),
        rows(&[&[set(&[(-300, Some(-150))])]])
    );
    assert_eq!(
        go(&f.w, &q("difference")),
        rows(&[&[set(&[(-300, Some(-280)), (-180, Some(-150))])]])
    );
    assert_eq!(
        go(&f.w, &q("intersect")),
        rows(&[&[set(&[(-280, Some(-250)), (-200, Some(-180))])]])
    );
    // a minus b is not b minus a
    let text = format!("[:find ?r :where {EDWIN} {MARA} [(difference ?im ?ie) ?r]]");
    assert_eq!(go(&f.w, &text), rows(&[&[set(&[(-250, Some(-200))])]]));
}

#[test]
fn an_empty_result_is_an_empty_interval_set_not_a_missing_row() {
    let f = edwin_world();
    let text = "[:find ?r :where (during [:edwin :located_in :varrow] ?ie) \
                (during [:tomas :located_in :varrow] ?it) [(intersect ?ie ?it) ?r]]";
    // touching intervals only: [-300,-250) | [-250,-200) | [-200,-150)
    assert_eq!(go(&f.w, text), rows(&[&[set(&[])]]));
}

// ------------------------------------------------------ contains / not-empty

#[test]
fn contains_is_half_open_at_every_boundary() {
    let f = edwin_world();
    let cases: &[(i64, bool)] = &[
        (-301, false),
        (-300, true),
        (-251, true),
        (-250, false),
        (-201, false),
        (-200, true),
        (-151, true),
        (-150, false),
        (i64::MIN, false),
        (i64::MAX, false),
    ];
    for &(t, want) in cases {
        let text = format!("[:find ?ie :where {EDWIN} [(contains ?ie {t})]]");
        let got = go(&f.w, &text);
        assert_eq!(got.len(), usize::from(want), "contains {t}: {got:?}");
    }
    // an unbounded set contains the largest tick
    let text = "[:find ?s :where (during [:lisa :located_in :varrow] ?s) [(contains ?s 9223372036854775807)]]";
    assert_eq!(go(&f.w, text).len(), 1);
}

#[test]
fn contains_takes_a_param_and_a_bound_tick_variable() {
    let f = edwin_world();
    let text = format!("[:find ?ie :in $ ?t :where {EDWIN} [(contains ?ie ?t)]]");
    assert_eq!(go_p(&f.w, &text, &[("t", Value::I64(-300))]).len(), 1);
    assert_eq!(go_p(&f.w, &text, &[("t", Value::I64(-250))]).len(), 0);
    // a tick bound by a history pattern: every tick where edwin's varrow state
    // changed and holds afterwards (the assert points)
    let text = format!(
        "[:find ?t :where {EDWIN} [:edwin :located_in :varrow ?t :assert] [(contains ?ie ?t)]]"
    );
    assert_eq!(
        go(&f.w, &text),
        rows(&[
            &[Datum::Value(Value::I64(-300))],
            &[Datum::Value(Value::I64(-200))]
        ])
    );
    // and the retract points are exactly the ones NOT contained
    let text = format!(
        "[:find ?t :where {EDWIN} [:edwin :located_in :varrow ?t :retract] [(contains ?ie ?t)]]"
    );
    assert!(go(&f.w, &text).is_empty());
}

#[test]
fn not_empty_filters_rows() {
    let f = edwin_world();
    let both = "(during [?c :located_in :varrow] ?ic)";
    let text = format!(
        "[:find ?c :where {EDWIN} {both} [(!= ?c :edwin)] [(intersect ?ie ?ic) ?x] [(not-empty ?x)]]"
    );
    assert_eq!(go(&f.w, &text), rows(&[&[f.r("mara")], &[f.r("lisa")]]));
    // not-empty on a plain during: only entities that EVER held it
    let text = "[:find ?c :where (during [?c :located_in :varrow] ?s) [(not-empty ?s)]]";
    assert_eq!(
        go(&f.w, text),
        rows(&[
            &[f.r("edwin")],
            &[f.r("mara")],
            &[f.r("greg")],
            &[f.r("tomas")],
            &[f.r("lisa")]
        ])
    );
}

// ------------------------------------------ who shared Varrow with Edwin (free ?c)

#[test]
fn who_was_ever_in_varrow_at_the_same_time_as_edwin() {
    let f = edwin_world();
    let text = "[:find ?c ?both :where (during [:edwin :located_in :varrow] ?ie) \
                (during [?c :located_in :varrow] ?ic) [(!= ?c :edwin)] \
                [(intersect ?ie ?ic) ?both] [(not-empty ?both)]]";
    // mara {[-280,-250),[-200,-180)}; lisa [-250,inf) meets only [-200,-150);
    // tomas only touches, greg left before Edwin arrived, wren was never there
    assert_eq!(
        go(&f.w, text),
        rows(&[
            &[f.r("mara"), set(&[(-280, Some(-250)), (-200, Some(-180))])],
            &[f.r("lisa"), set(&[(-200, Some(-150))])],
        ])
    );
}

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

#[test]
fn who_was_ever_in_varrow_with_edwin_on_varrow_yaml() {
    let w = load_fixture(VARROW).expect("fixture loads");
    let h = |n: &str| Datum::Value(Value::Ref(w.entity_by_handle(n).unwrap()));
    // Hand-derived from the YAML. Edwin's located_in varrow: asserted @-300 and
    // retracted @-250 (tx 4); the prequel (tx 8) is oakhollow @-340..-300, not
    // varrow; the retcon (tx 11) touches his alliances and a typo only. So
    // edwin = {[-300, -250)}. Everyone else with a varrow state:
    //   osric  A-400, never retracted        -> [-400, inf)  meets: [-300,-250)
    //   tomas  A-350, never retracted        -> [-350, inf)  meets: [-300,-250)
    //   hild   A-270 R-230                   -> [-270, -230) meets: [-270,-250)
    //   mara   A-220 R-190                   -> starts after -250: no
    //   greg   A200 R250 (prequel is carnath) -> no
    //   v01    A-395 R-95                    -> [-395, -95)  meets: [-300,-250)
    //   v15    A-325 R-25                    -> [-325, -25)  meets: [-300,-250)
    //   v10 (A-50) and v24 (A20) arrive long after: no
    let text = "[:find ?c ?both :where (during [:edwin :located_in :varrow] ?ie) \
                (during [?c :located_in :varrow] ?ic) [(!= ?c :edwin)] \
                [(intersect ?ie ?ic) ?both] [(not-empty ?both)]]";
    let full = set(&[(-300, Some(-250))]);
    assert_eq!(
        go(&w, text),
        rows(&[
            &[h("osric"), full.clone()],
            &[h("tomas"), full.clone()],
            &[h("hild"), set(&[(-270, Some(-250))])],
            &[h("v01"), full.clone()],
            &[h("v15"), full],
        ])
    );
}

// --------------------------------------------------------------- free values

#[test]
fn a_free_value_gives_one_row_per_place_ever_visited() {
    let f = edwin_world();
    let text = "[:find ?p ?s :where (during [:greg :located_in ?p] ?s)]";
    assert_eq!(
        go(&f.w, text),
        rows(&[
            &[f.r("varrow"), set(&[(-400, Some(-310))])],
            &[f.r("ashford"), set(&[(-310, None)])],
            &[f.r("carnath"), set(&[(0, Some(10))])],
        ])
    );
}

#[test]
fn a_free_entity_and_a_free_value_one_row_per_pair() {
    let f = edwin_world();
    let text = "[:find ?c ?p :where (during [?c :located_in ?p] ?s)]";
    assert_eq!(
        go(&f.w, text),
        rows(&[
            &[f.r("edwin"), f.r("varrow")],
            &[f.r("mara"), f.r("varrow")],
            &[f.r("greg"), f.r("varrow")],
            &[f.r("greg"), f.r("ashford")],
            &[f.r("greg"), f.r("carnath")],
            &[f.r("tomas"), f.r("varrow")],
            &[f.r("lisa"), f.r("varrow")],
            &[f.r("wren"), f.r("ashford")],
        ])
    );
    // `_` for the entity: the distinct sets of everyone who was ever in varrow
    let text = "[:find ?s :where (during [_ :located_in :varrow] ?s)]";
    assert_eq!(
        go(&f.w, text),
        rows(&[
            &[set(&[(-300, Some(-250)), (-200, Some(-150))])],
            &[set(&[(-280, Some(-180))])],
            &[set(&[(-400, Some(-310))])],
            &[set(&[(-250, Some(-200))])],
            &[set(&[(-250, None)])],
        ])
    );
}

#[test]
fn during_with_params() {
    let f = edwin_world();
    let text = "[:find ?s :in $ ?c ?p :where (during [?c :located_in ?p] ?s)]";
    let got = go_p(
        &f.w,
        text,
        &[
            ("c", Value::Ref(f.ids["mara"])),
            ("p", Value::Ref(f.ids["varrow"])),
        ],
    );
    assert_eq!(got, rows(&[&[set(&[(-280, Some(-180))])]]));
    let got = go_p(
        &f.w,
        text,
        &[
            ("c", Value::Ref(f.ids["wren"])),
            ("p", Value::Ref(f.ids["varrow"])),
        ],
    );
    assert!(got.is_empty(), "wren was never in varrow: {got:?}");
}

#[test]
fn an_entity_bound_by_an_earlier_pattern() {
    let f = edwin_world();
    // wren is in ashford now: her set there
    let text = "[:find ?s :where [:wren :located_in ?p] (during [:wren :located_in ?p] ?s)]";
    assert_eq!(go(&f.w, text), rows(&[&[set(&[(-100, None)])]]));
}

// ------------------------------------------------------- kinds of attribute

#[test]
fn an_event_that_is_never_retracted_holds_from_its_tick_on() {
    let f = edwin_world();
    let text = "[:find ?s :where (during [:edwin :bought :sword] ?s)]";
    assert_eq!(go(&f.w, text), rows(&[&[set(&[(-208, None)])]]));
    let text = "[:find ?s :where (during [:edwin :bought :sword] ?s) [(contains ?s -209)]]";
    assert!(go(&f.w, text).is_empty());
    let text = "[:find ?s :where (during [:edwin :bought :sword] ?s) [(contains ?s -208)]]";
    assert_eq!(go(&f.w, text).len(), 1);
}

#[test]
fn an_inverse_name_and_a_bidirectional_attribute() {
    let f = edwin_world();
    let want = rows(&[&[set(&[(-200, Some(-100))])]]);
    assert_eq!(
        go(
            &f.w,
            "[:find ?s :where (during [:mara :child_of :edwin] ?s)]"
        ),
        want
    );
    assert_eq!(
        go(
            &f.w,
            "[:find ?s :where (during [:edwin :parent_of :mara] ?s)]"
        ),
        want
    );
    let want = rows(&[&[set(&[(-208, Some(-100))])]]);
    for t in ["[:edwin :ally_of :mara]", "[:mara :ally_of :edwin]"] {
        let text = format!("[:find ?s :where (during {t} ?s)]");
        assert_eq!(go(&f.w, &text), want, "{t}");
    }
}

// --------------------------------------------------- canon: late and deleted

#[test]
fn an_entity_defined_after_the_canon_contributes_nothing() {
    let mut f = edwin_world();
    let before = f.w.latest_tx().0;
    let late = f.w.create_entity("CHARACTER", Some("late")).unwrap();
    f.ids.insert("late", late);
    f.w.assert(late, "located_in", Value::Ref(f.ids["varrow"]), tk(-300))
        .unwrap();
    let q = |canon: u64| {
        format!(
            "[:find ?c :canon {canon} :where (during [?c :located_in :varrow] ?s) [(not-empty ?s)]]"
        )
    };
    let old = go(&f.w, &q(before));
    assert!(!old.contains(&vec![f.r("late")]), "{old:?}");
    assert_eq!(old.len(), 5);
    let new = go(&f.w, &q(f.w.latest_tx().0));
    assert!(new.contains(&vec![f.r("late")]));
    assert_eq!(new.len(), 6);
}

#[test]
fn a_correction_is_honoured_by_canon() {
    let mut f = fresh();
    let a = loc(&mut f, "lisa", "varrow", -250, Op::Assert);
    let old = f.w.latest_tx().0;
    f.w.correct(
        a,
        Correction::Replace {
            value: None,
            tick: tk(-260),
            op: None,
            precision: None,
        },
    )
    .unwrap();
    let new = f.w.latest_tx().0;
    let q =
        |c: u64| format!("[:find ?s :canon {c} :where (during [:lisa :located_in :varrow] ?s)]");
    assert_eq!(go(&f.w, &q(old)), rows(&[&[set(&[(-250, None)])]]));
    assert_eq!(go(&f.w, &q(new)), rows(&[&[set(&[(-260, None)])]]));
    // a delete: the pair has no row at the latest canon, but does at the old one
    f.w.correct(a, Correction::Delete).unwrap();
    let text = "[:find ?c :where (during [?c :located_in :varrow] ?s)]";
    assert!(go(&f.w, text).is_empty());
    let text = format!("[:find ?c :canon {new} :where (during [?c :located_in :varrow] ?s)]");
    assert_eq!(go(&f.w, &text), rows(&[&[f.r("lisa")]]));
}

// ----------------------------------------------------- equality constraints

#[test]
fn a_bound_interval_variable_is_an_equality_constraint() {
    let f = edwin_world();
    // the same during twice: one row
    let text = format!("[:find ?ie :where {EDWIN} {EDWIN}]");
    assert_eq!(go(&f.w, &text).len(), 1);
    // two different sets: no row
    let text = format!("[:find ?ie :where {EDWIN} (during [:mara :located_in :varrow] ?ie)]");
    assert!(go(&f.w, &text).is_empty());
    // a free entity joined by the shared set: who has exactly Edwin's set (just him)
    let text = format!("[:find ?c :where {EDWIN} (during [?c :located_in :varrow] ?ie)]");
    assert_eq!(go(&f.w, &text), rows(&[&[f.r("edwin")]]));
    // a function output that is already bound: a ∩ a = a holds, a − a = a does not
    let text = format!("[:find ?ie :where {EDWIN} [(intersect ?ie ?ie) ?ie]]");
    assert_eq!(go(&f.w, &text).len(), 1);
    let text = format!("[:find ?ie :where {EDWIN} [(difference ?ie ?ie) ?ie]]");
    assert!(go(&f.w, &text).is_empty());
    // = and != between interval sets
    let text =
        format!("[:find ?c :where {EDWIN} (during [?c :located_in :varrow] ?ic) [(= ?ie ?ic)]]");
    assert_eq!(go(&f.w, &text), rows(&[&[f.r("edwin")]]));
    let text =
        format!("[:find ?c :where {EDWIN} (during [?c :located_in :varrow] ?ic) [(!= ?ie ?ic)]]");
    assert_eq!(go(&f.w, &text).len(), 4);
}

// ------------------------------------------------------ rules and negation

#[test]
fn during_and_functions_inside_a_rule_body() {
    let f = edwin_world();
    let text = "[:find ?c ?both :rules [[(shared ?c ?both) \
                (during [:edwin :located_in :varrow] ?ie) \
                (during [?c :located_in :varrow] ?ic) [(!= ?c :edwin)] \
                [(intersect ?ie ?ic) ?both] [(not-empty ?both)]]] \
                :where (shared ?c ?both)]";
    assert_eq!(
        go(&f.w, text),
        rows(&[
            &[f.r("mara"), set(&[(-280, Some(-250)), (-200, Some(-180))])],
            &[f.r("lisa"), set(&[(-200, Some(-150))])],
        ])
    );
}

#[test]
fn a_not_with_functions_inside_keeps_rows_that_share_no_time() {
    let f = edwin_world();
    // everyone ever in varrow who never overlapped Edwin there (Edwin himself did)
    let text = format!(
        "[:find ?c :where {EDWIN} (during [?c :located_in :varrow] ?ic) \
         (not [(intersect ?ie ?ic) ?b] [(not-empty ?b)])]"
    );
    assert_eq!(go(&f.w, &text), rows(&[&[f.r("greg")], &[f.r("tomas")]]));
}

#[test]
fn a_not_with_a_during_inside() {
    let f = edwin_world();
    // characters with any location history who were never in varrow: only wren
    let text = "[:find ?c :where [?c :located_in _ _] \
                (not (during [?c :located_in :varrow] ?s))]";
    assert_eq!(go(&f.w, text), rows(&[&[f.r("wren")]]));
}

// ------------------------------------------------------------------- errors

#[test]
fn malformed_during_is_a_syntax_error() {
    let f = edwin_world();
    for bad in [
        "(during [:edwin :located_in :varrow])",
        "(during [:edwin :located_in :varrow] ?a ?b)",
        "(during [:edwin :located_in] ?s)",
        "(during [:edwin :located_in :varrow ?t] ?s)",
        "(during [:edwin :located_in :varrow ?t :assert] ?s)",
        "(during ?x ?s)",
        "(during [:edwin :located_in :varrow] 5)",
        "(during [:edwin :located_in :varrow] :x)",
        "(during)",
        "(during [:edwin :kala_handle :varrow] ?s)",
        "(during [?e :kala_handle \"edwin\"] ?s)",
    ] {
        let text = format!("[:find ?e :where {bad}]");
        assert_eq!(kind(&f.w, &text), ErrorKind::Syntax, "{bad}");
    }
}

#[test]
fn during_inside_at_is_a_syntax_error() {
    let f = edwin_world();
    let text = "[:find ?s :where (at -200 (during [:edwin :located_in :varrow] ?s))]";
    assert_eq!(kind(&f.w, text), ErrorKind::Syntax);
}

#[test]
fn during_name_resolution_errors() {
    let f = edwin_world();
    assert_eq!(
        kind(&f.w, "[:find ?s :where (during [:edwin :nope :varrow] ?s)]"),
        ErrorKind::UnknownAttribute
    );
    assert_eq!(
        kind(
            &f.w,
            "[:find ?s :where (during [:nobody :located_in :varrow] ?s)]"
        ),
        ErrorKind::UnknownEntity
    );
    // a value of the wrong type for a ref attribute
    assert_eq!(
        kind(&f.w, "[:find ?s :where (during [:edwin :located_in 5] ?s)]"),
        ErrorKind::TypeMismatch
    );
    // the output variable already bound as something else
    assert_eq!(
        kind(
            &f.w,
            "[:find ?p :where [:edwin :bought ?p] (during [:edwin :located_in :varrow] ?p)]"
        ),
        ErrorKind::TypeMismatch
    );
}

#[test]
fn wrong_arity_and_unknown_function_are_syntax_errors() {
    let f = edwin_world();
    for bad in [
        "[(intersect ?ie) ?c]",
        "[(intersect ?ie ?im ?ie) ?c]",
        "[(union ?ie) ?c]",
        "[(difference ?ie ?im ?ie) ?c]",
        "[(intersect ?ie ?im)]",
        "[(not-empty ?ie ?im)]",
        "[(not-empty)]",
        "[(contains ?ie)]",
        "[(contains ?ie 1 2)]",
        "[(frobnicate ?ie ?im) ?c]",
        "[(frobnicate ?ie)]",
    ] {
        let text = format!("[:find ?ie :where {EDWIN} {MARA} {bad}]");
        assert_eq!(kind(&f.w, &text), ErrorKind::Syntax, "{bad}");
    }
}

#[test]
fn an_argument_that_is_not_an_interval_set_is_a_type_mismatch() {
    let f = edwin_world();
    for bad in [
        "[:edwin :located_in ?p] [(intersect ?p ?ie) ?c]",
        "[:edwin :located_in ?p] [(intersect ?ie ?p) ?c]",
        "[:edwin :located_in ?p] [(union ?p ?p) ?c]",
        "[:edwin :located_in ?p] [(difference ?ie ?p) ?c]",
        "[:edwin :located_in ?p] [(not-empty ?p)]",
        "[:edwin :located_in ?p] [(contains ?p 1)]",
        "[(contains ?ie :varrow)]",
        "[(contains ?ie \"x\")]",
        "[(contains ?ie 1.5)]",
        "[:edwin :located_in ?p] [(contains ?ie ?p)]",
    ] {
        let text = format!("[:find ?ie :where {EDWIN} {bad}]");
        assert_eq!(kind(&f.w, &text), ErrorKind::TypeMismatch, "{bad}");
    }
}

#[test]
fn an_unbound_argument_is_an_unbound_variable_error() {
    let f = edwin_world();
    for bad in [
        "[(intersect ?ie ?nope) ?c]",
        "[(intersect ?nope ?ie) ?c]",
        "[(not-empty ?nope)]",
        "[(contains ?nope 1)]",
        "[(contains ?ie ?nope)]",
    ] {
        let text = format!("[:find ?ie :where {EDWIN} {bad}]");
        assert_eq!(kind(&f.w, &text), ErrorKind::UnboundVariable, "{bad}");
    }
}

#[test]
fn interval_sets_are_their_own_type() {
    let f = edwin_world();
    // in a pattern position
    for bad in [
        "[?ie :located_in :varrow]",
        "[:edwin :located_in ?ie]",
        "[:edwin :located_in :varrow ?ie]",
        "[:edwin :located_in :varrow 0 ?ie]",
        "[:edwin :located_in :varrow ?ie :assert]",
        "(at ?ie [:edwin :located_in :varrow])",
    ] {
        let text = format!("[:find ?ie :where {EDWIN} {bad}]");
        assert_eq!(kind(&f.w, &text), ErrorKind::TypeMismatch, "{bad}");
    }
    // ordering comparisons
    for op in ["<", "<=", ">", ">="] {
        let text = format!("[:find ?ie :where {EDWIN} {MARA} [({op} ?ie ?im)]]");
        assert_eq!(kind(&f.w, &text), ErrorKind::TypeMismatch, "{op}");
    }
    // against a non-interval value
    for bad in ["[(= ?ie 1)]", "[(= ?ie :mara)]", "[(!= ?ie \"x\")]"] {
        let text = format!("[:find ?ie :where {EDWIN} {bad}]");
        assert_eq!(kind(&f.w, &text), ErrorKind::TypeMismatch, "{bad}");
    }
    // = and != between two interval sets are fine
    for op in ["=", "!="] {
        let text = format!("[:find ?ie :where {EDWIN} {MARA} [({op} ?ie ?im)]]");
        prepare(&f.w, &text).unwrap_or_else(|e| panic!("{op}: {e}"));
    }
}

// ------------------------------------------------------------------- budgets

/// One character with `n` separate intervals in varrow.
fn striped(n: i64) -> W {
    let mut f = fresh();
    let (g, p) = (f.ids["greg"], Value::Ref(f.ids["varrow"]));
    let mut t = f.w.begin();
    for k in 0..n {
        t.assert(g, "located_in", p.clone(), tk(k * 10)).unwrap();
        t.retract(g, "located_in", p.clone(), tk(k * 10 + 5))
            .unwrap();
    }
    t.commit().unwrap();
    f
}

#[test]
fn a_large_interval_set_is_charged_to_the_byte_budget_by_size() {
    let big = striped(2000);
    let text = "[:find ?s :where (during [:greg :located_in :varrow] ?s)]";
    assert_eq!(go(&big.w, text).len(), 1); // fine under the default budget
    let lim = Limits {
        max_bytes: 8 * 1024,
        ..Limits::default()
    };
    match try_run(&big.w, text, &[], &lim) {
        Err(QueryError::MemoryBudget { limit }) => assert_eq!(limit, 8 * 1024),
        other => panic!("expected MemoryBudget, got {other:?}"),
    }
    // a small set passes under the same limit
    let small = striped(2);
    assert_eq!(try_run(&small.w, text, &[], &lim).unwrap().len(), 1);
}

#[test]
fn a_function_result_is_charged_too() {
    // the during reads fit in 64 KiB, but the union of a set with itself plus
    // the result sets in one row hold several copies
    let big = striped(1500);
    let text = "[:find ?u ?i ?d :where (during [:greg :located_in :varrow] ?s) \
                [(union ?s ?s) ?u] [(intersect ?s ?s) ?i] [(difference ?s ?s) ?d]]";
    assert_eq!(go(&big.w, text).len(), 1);
    let lim = Limits {
        max_bytes: 16 * 1024,
        ..Limits::default()
    };
    match try_run(&big.w, text, &[], &lim) {
        Err(QueryError::MemoryBudget { .. }) => {}
        other => panic!("expected MemoryBudget, got {other:?}"),
    }
}

#[test]
fn during_work_is_metered_against_the_step_budget() {
    let mut f = fresh();
    {
        let mut t = f.w.begin();
        let p = Value::Ref(f.ids["varrow"]);
        for n in 0..200 {
            let e = t.create_entity("CHARACTER", None).unwrap();
            t.assert(e, "located_in", p.clone(), tk(n)).unwrap();
            t.retract(e, "located_in", p.clone(), tk(n + 1000)).unwrap();
        }
        t.commit().unwrap();
    }
    let text = "[:find ?c ?s :where (during [?c :located_in :varrow] ?s)]";
    assert!(go(&f.w, text).len() >= 200);
    let lim = Limits {
        max_steps: 50,
        ..Limits::default()
    };
    match try_run(&f.w, text, &[], &lim) {
        Err(QueryError::StepBudget { limit: 50 }) => {}
        other => panic!("expected StepBudget, got {other:?}"),
    }
}
