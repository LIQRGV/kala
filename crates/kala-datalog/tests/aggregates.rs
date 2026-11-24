//! Slice 7, item 7 (RED): aggregates in `:find`.
//!
//! Contract (tech-lead decisions):
//! - `:find` elements are variables or `(count ?x)`, `(count-distinct ?x)`,
//!   `(min ?x)`, `(max ?x)`, `(sum ?x)`. Any other list head, a wrong arity or
//!   a non-variable argument is `Syntax`. `?x` must be bound by a positive
//!   `:where` clause (a param, or a variable only a `not` mentions, is
//!   `UnboundVariable`). Aggregates are not allowed in rule heads (`Syntax`).
//! - Groups are the non-aggregate `:find` variables (none: one group).
//! - The input is the SET of distinct bindings of every variable the positive
//!   `:where` clauses bind (before projection): `count` counts those bindings
//!   in the group, `count-distinct` the distinct `?x`, `sum` adds `?x` over
//!   the bindings (a bag), `min`/`max` take the group's `?x` values.
//! - Types: count and count-distinct take any type and give i64; min/max take
//!   i64, f64 or string; sum takes i64 (checked; overflow is
//!   `QueryError::Overflow(String)`) or f64; the rest is `TypeMismatch`.
//! - Empty input gives no rows, with or without group variables.
//! - `ResultSet.columns` keep `:find` order; an aggregate column is
//!   `Var("(count ?c)")` (`(` + name + ` ?` + var + `)`, normalized).
//! - It compiles to `Op::Aggregate` below a `Project`; groups count toward
//!   `max_rows` and the work is metered.
//!
//! Base world (`world_a`), all located_in at tick 10 unless noted:
//!   edwin  varrow; alias "Ed","Eddy"; name "Edwin"; gold 10; weight 1.5; alive
//!   mara   varrow; alias "Ed";        name "Mara";  gold 10; weight 2.25; alive
//!   greg   varrow A10 R250 A300;      name "Greg";  gold 5;  weight 0.25
//!   lisa   ashford; alias "Ash","Queen","Ashen"; name "Lisa"; gold 7
//!   tomas  ashford A10 R20;           name "Tomas"
//!   eowyn  (no place)                 name "\u{c9}owyn"

use kala_core::*;
use kala_datalog::{ErrorKind, prepare};
use kala_fixture::load_fixture;
use kala_query::{Datum, Limits, Params, QueryError, ResultSet, Var, run};
use std::collections::{BTreeMap, BTreeSet};

type Rows = BTreeSet<Vec<Datum>>;

struct W {
    w: World,
    ids: BTreeMap<&'static str, EntityId>,
}

fn schema() -> Schema {
    let at = |n, t, k, c| AttrDef::new(n, t, k, c);
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(at("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(at("alias", ValueType::Str, Kind::State, Cardinality::Many))
        .attribute(at("name", ValueType::Str, Kind::State, Cardinality::One))
        .attribute(at("gold", ValueType::I64, Kind::State, Cardinality::One))
        .attribute(at("weight", ValueType::F64, Kind::State, Cardinality::One))
        .attribute(at("alive", ValueType::Bool, Kind::State, Cardinality::One))
        .attribute(
            at("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .directed()
                .inverse("child_of"),
        )
}

fn tk(n: i64) -> Option<Tick> {
    Some(Tick(n))
}
fn i(n: i64) -> Datum {
    Datum::Value(Value::I64(n))
}
fn fl(x: f64) -> Datum {
    Datum::Value(Value::F64(x))
}
fn s(x: &str) -> Datum {
    Datum::Value(Value::Str(x.into()))
}
fn rows(rs: &[&[Datum]]) -> Rows {
    rs.iter().map(|r| r.to_vec()).collect()
}

impl W {
    fn r(&self, n: &str) -> Datum {
        Datum::Value(Value::Ref(self.ids[n]))
    }
}

fn world_a() -> W {
    let mut w = World::in_memory(schema()).unwrap();
    let mut ids = BTreeMap::new();
    for h in ["edwin", "mara", "greg", "lisa", "tomas", "eowyn"] {
        ids.insert(h, w.create_entity("CHARACTER", Some(h)).unwrap());
    }
    for h in ["varrow", "ashford"] {
        ids.insert(h, w.create_entity("PLACE", Some(h)).unwrap());
    }
    let id = |h: &str| ids[h];
    let rf = |h: &str| Value::Ref(ids[h]);
    let st = |x: &str| Value::Str(x.into());
    {
        let mut t = w.begin();
        for (c, p) in [
            ("edwin", "varrow"),
            ("mara", "varrow"),
            ("greg", "varrow"),
            ("lisa", "ashford"),
            ("tomas", "ashford"),
        ] {
            t.assert(id(c), "located_in", rf(p), tk(10)).unwrap();
        }
        t.retract(id("tomas"), "located_in", rf("ashford"), tk(20))
            .unwrap();
        t.retract(id("greg"), "located_in", rf("varrow"), tk(250))
            .unwrap();
        t.assert(id("greg"), "located_in", rf("varrow"), tk(300))
            .unwrap();
        for (c, a) in [
            ("edwin", "Ed"),
            ("edwin", "Eddy"),
            ("mara", "Ed"),
            ("lisa", "Ash"),
            ("lisa", "Queen"),
            ("lisa", "Ashen"),
        ] {
            t.assert(id(c), "alias", st(a), tk(10)).unwrap();
        }
        for (c, n) in [
            ("edwin", "Edwin"),
            ("mara", "Mara"),
            ("greg", "Greg"),
            ("lisa", "Lisa"),
            ("tomas", "Tomas"),
            ("eowyn", "\u{c9}owyn"),
        ] {
            t.assert(id(c), "name", st(n), tk(10)).unwrap();
        }
        for (c, g) in [("edwin", 10), ("mara", 10), ("greg", 5), ("lisa", 7)] {
            t.assert(id(c), "gold", Value::I64(g), tk(10)).unwrap();
        }
        for (c, x) in [("edwin", 1.5), ("mara", 2.25), ("greg", 0.25)] {
            t.assert(id(c), "weight", Value::F64(x), tk(10)).unwrap();
        }
        t.assert(id("edwin"), "alive", Value::Bool(true), tk(10))
            .unwrap();
        t.assert(id("mara"), "alive", Value::Bool(true), tk(10))
            .unwrap();
        t.commit().unwrap();
    }
    W { w, ids }
}

fn rs_of(
    w: &World,
    text: &str,
    params: &[(&str, Value)],
    lim: &Limits,
) -> Result<ResultSet, QueryError> {
    let p = prepare(w, text).unwrap_or_else(|e| panic!("prepare failed for {text}\n  {e}"));
    let ps: Params = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    run(w, &p.plan, &ps, lim)
}
fn go(w: &World, text: &str) -> Rows {
    rs_of(w, text, &[], &Limits::default())
        .unwrap_or_else(|e| panic!("run failed for {text}\n  {e}"))
        .rows
        .into_iter()
        .collect()
}
fn kind(w: &World, text: &str) -> ErrorKind {
    match prepare(w, text) {
        Ok(_) => panic!("expected an error for {text}"),
        Err(e) => e.kind,
    }
}

// ------------------------------------------------------------- count by group

#[test]
fn count_per_place_at_a_tick() {
    let f = world_a();
    // :at 15: varrow = edwin, mara, greg; ashford = lisa, tomas
    assert_eq!(
        go(
            &f.w,
            "[:find ?p (count ?c) :at 15 :where [?c :located_in ?p]]"
        ),
        rows(&[&[f.r("varrow"), i(3)], &[f.r("ashford"), i(2)]])
    );
    // :at 25: tomas left ashford at 20
    assert_eq!(
        go(
            &f.w,
            "[:find ?p (count ?c) :at 25 :where [?c :located_in ?p]]"
        ),
        rows(&[&[f.r("varrow"), i(3)], &[f.r("ashford"), i(1)]])
    );
    // :at 260: greg left varrow at 250; :at 300: he is back
    assert_eq!(
        go(
            &f.w,
            "[:find ?p (count ?c) :at 260 :where [?c :located_in ?p]]"
        ),
        rows(&[&[f.r("varrow"), i(2)], &[f.r("ashford"), i(1)]])
    );
    assert_eq!(
        go(
            &f.w,
            "[:find ?p (count ?c) :at 300 :where [?c :located_in ?p]]"
        ),
        rows(&[&[f.r("varrow"), i(3)], &[f.r("ashford"), i(1)]])
    );
}

#[test]
fn count_per_place_on_varrow_yaml_at_a_tick() {
    let w = load_fixture(include_str!("../../kala-core/tests/fixtures/varrow.yaml")).unwrap();
    // Hand-derived from the `located_in` lines of varrow.yaml, who is in each
    // place at -200 (latest op with tick <= -200 is an assert):
    //   varrow 5: osric(-400) tomas(-350) v01(-395, left -95) v15(-325, left -25)
    //             mara(-220..-190)   [edwin is in carnath from -250; hild left -230]
    //   carnath 3: edwin(-250), v03(-385), v17(-315)
    //   redcliff 3: hild(-230), v09(-355), v23(-285)
    //   ashford 2: v02, v16        dunmere 2: v05, v19    eldoria 2: v04, v18
    //   highkeep 2: v06, v20       oakhollow 2: v08, v22  saltmarsh 2: v07, v21
    //   stonebridge 2: v10, v24
    //   graymoor 1: v14   mistral 1: v13   thornwick 1: v11   whitefall 1: v12
    // (wren is in saltmarsh only from -170; v-villagers and the rest checked by
    // listing every located_in line; the total is 29 placed characters)
    let want = [
        ("varrow", 5),
        ("carnath", 3),
        ("redcliff", 3),
        ("ashford", 2),
        ("dunmere", 2),
        ("eldoria", 2),
        ("highkeep", 2),
        ("oakhollow", 2),
        ("saltmarsh", 2),
        ("stonebridge", 2),
        ("graymoor", 1),
        ("mistral", 1),
        ("thornwick", 1),
        ("whitefall", 1),
    ];
    let want: Rows = want
        .iter()
        .map(|(p, n)| {
            vec![
                Datum::Value(Value::Ref(w.entity_by_handle(p).unwrap())),
                i(*n),
            ]
        })
        .collect();
    assert_eq!(
        go(
            &w,
            "[:find ?p (count ?c) :at -200 :where [?c :located_in ?p]]"
        ),
        want
    );
    // no group variable: all 29
    assert_eq!(
        go(&w, "[:find (count ?c) :at -200 :where [?c :located_in ?p]]"),
        rows(&[&[i(29)]])
    );
    // Osric's two aliases ("Old Osric" at -400, "the Innkeeper" at -390)
    assert_eq!(
        go(&w, "[:find (count ?a) :at -200 :where [:osric :alias ?a]]"),
        rows(&[&[i(2)]])
    );
    // Greg's change points: -100 150 200 250 601; first time in Varrow: 200
    assert_eq!(
        go(
            &w,
            "[:find (min ?t) :where [:greg :located_in :varrow ?t :assert]]"
        ),
        rows(&[&[i(200)]])
    );
    assert_eq!(
        go(
            &w,
            "[:find (min ?t) (max ?t) :where [:greg :located_in ?p ?t]]"
        ),
        rows(&[&[i(-100), i(601)]])
    );
}

// ------------------------------------------- count vs count-distinct (bindings)

#[test]
fn count_counts_bindings_and_count_distinct_counts_values() {
    let f = world_a();
    let q =
        |aggs: &str| format!("[:find ?p {aggs} :at 15 :where [?c :located_in ?p] [?c :alias ?a]]");
    // varrow bindings (c, a): (edwin,Ed) (edwin,Eddy) (mara,Ed) -> 3; distinct c = 2, distinct a = 2
    // ashford (lisa has 3 aliases; tomas none): 3 bindings; distinct c = 1, distinct a = 3
    assert_eq!(
        go(&f.w, &q("(count ?c)")),
        rows(&[&[f.r("varrow"), i(3)], &[f.r("ashford"), i(3)]])
    );
    assert_eq!(
        go(&f.w, &q("(count-distinct ?c)")),
        rows(&[&[f.r("varrow"), i(2)], &[f.r("ashford"), i(1)]])
    );
    assert_eq!(
        go(&f.w, &q("(count-distinct ?a)")),
        rows(&[&[f.r("varrow"), i(2)], &[f.r("ashford"), i(3)]])
    );
    // several aggregates in one :find, in the order written
    assert_eq!(
        go(
            &f.w,
            &q("(count ?c) (count-distinct ?c) (count-distinct ?a)")
        ),
        rows(&[
            &[f.r("varrow"), i(3), i(2), i(2)],
            &[f.r("ashford"), i(3), i(1), i(3)],
        ])
    );
    // count over a projected-away variable still sees the bindings
    assert_eq!(
        go(
            &f.w,
            "[:find (count ?a) :at 15 :where [?c :located_in :varrow] [?c :alias ?a]]"
        ),
        rows(&[&[i(3)]])
    );
    // count of any type: ref, string, bool
    assert_eq!(
        go(
            &f.w,
            "[:find (count ?c) (count ?n) :at 15 :where [?c :name ?n]]"
        ),
        rows(&[&[i(6), i(6)]])
    );
    assert_eq!(
        go(&f.w, "[:find (count ?b) :at 15 :where [?c :alive ?b]]"),
        rows(&[&[i(2)]])
    );
}

#[test]
fn a_group_variable_may_also_be_aggregated() {
    let f = world_a();
    // one binding per (c, p) and the group is c: every count is 1
    assert_eq!(
        go(
            &f.w,
            "[:find ?c (count ?c) :at 15 :where [?c :located_in ?p]]"
        ),
        rows(&[
            &[f.r("edwin"), i(1)],
            &[f.r("mara"), i(1)],
            &[f.r("greg"), i(1)],
            &[f.r("lisa"), i(1)],
            &[f.r("tomas"), i(1)],
        ])
    );
}

// ------------------------------------------------------------------- sum

#[test]
fn sum_i64_adds_over_bindings_not_distinct_values() {
    let f = world_a();
    // varrow: edwin 10 + mara 10 + greg 5 = 25 (the two 10s are two bindings);
    // ashford: lisa 7 (tomas has no gold)
    assert_eq!(
        go(
            &f.w,
            "[:find ?p (sum ?g) :at 15 :where [?c :located_in ?p] [?c :gold ?g]]"
        ),
        rows(&[&[f.r("varrow"), i(25)], &[f.r("ashford"), i(7)]])
    );
    assert_eq!(
        go(&f.w, "[:find (sum ?g) :at 15 :where [?c :gold ?g]]"),
        rows(&[&[i(32)]])
    );
    // the plain projection would collapse the equal 10s: that is not what sum sees
    assert_eq!(go(&f.w, "[:find ?g :at 15 :where [?c :gold ?g]]").len(), 3);
}

#[test]
fn sum_f64() {
    let f = world_a();
    // 1.5 + 2.25 + 0.25 = 4.0 exactly in binary
    assert_eq!(
        go(&f.w, "[:find (sum ?x) :at 15 :where [?c :weight ?x]]"),
        rows(&[&[fl(4.0)]])
    );
    assert_eq!(
        go(
            &f.w,
            "[:find ?p (sum ?x) :at 15 :where [?c :located_in ?p] [?c :weight ?x]]"
        ),
        rows(&[&[f.r("varrow"), fl(4.0)]])
    );
}

#[test]
fn sum_overflow_is_an_overflow_error() {
    let mut f = world_a();
    let mut t = f.w.begin();
    let a = t.create_entity("CHARACTER", None).unwrap();
    let b = t.create_entity("CHARACTER", None).unwrap();
    t.assert(a, "gold", Value::I64(i64::MAX), tk(1000)).unwrap();
    t.assert(b, "gold", Value::I64(1), tk(1000)).unwrap();
    t.commit().unwrap();
    // every gold is positive, so MAX + 1 + 10 + 10 + 5 + 7 overflows in any order
    let r = rs_of(
        &f.w,
        "[:find (sum ?g) :at 1000 :where [?c :gold ?g]]",
        &[],
        &Limits::default(),
    );
    assert!(matches!(r, Err(QueryError::Overflow(_))), "{r:?}");
    // a group that doesn't overflow still answers: only `a` holds MAX
    assert_eq!(
        go(
            &f.w,
            "[:find ?c (sum ?g) :at 1000 :where [?c :gold ?g] [(> ?g 100)]]"
        ),
        rows(&[&[Datum::Value(Value::Ref(a)), i(i64::MAX)]])
    );
}

#[test]
fn sum_of_min_and_max_i64_is_minus_one_and_min_max_see_the_extremes() {
    let mut f = world_a();
    let mut t = f.w.begin();
    let a = t.create_entity("CHARACTER", None).unwrap();
    let b = t.create_entity("CHARACTER", None).unwrap();
    // tick 5 is before every gold of the base world (tick 10)
    t.assert(a, "gold", Value::I64(i64::MIN), tk(5)).unwrap();
    t.assert(b, "gold", Value::I64(i64::MAX), tk(5)).unwrap();
    t.commit().unwrap();
    assert_eq!(
        go(&f.w, "[:find (sum ?g) :at 5 :where [?c :gold ?g]]"),
        rows(&[&[i(-1)]])
    );
    assert_eq!(
        go(&f.w, "[:find (min ?g) (max ?g) :at 5 :where [?c :gold ?g]]"),
        rows(&[&[i(i64::MIN), i(i64::MAX)]])
    );
}

// ------------------------------------------------------------ min and max

#[test]
fn min_max_of_ticks_from_a_history_pattern() {
    let f = world_a();
    // greg in varrow: assert 10, retract 250, assert 300
    assert_eq!(
        go(
            &f.w,
            "[:find (min ?t) :where [:greg :located_in :varrow ?t :assert]]"
        ),
        rows(&[&[i(10)]])
    );
    assert_eq!(
        go(
            &f.w,
            "[:find (min ?t) (max ?t) :where [:greg :located_in :varrow ?t :assert]]"
        ),
        rows(&[&[i(10), i(300)]])
    );
    assert_eq!(
        go(
            &f.w,
            "[:find (min ?t) (max ?t) :where [:greg :located_in :varrow ?t :retract]]"
        ),
        rows(&[&[i(250), i(250)]])
    );
    // per character: first and last time anyone asserted being in varrow
    assert_eq!(
        go(
            &f.w,
            "[:find ?c (min ?t) (max ?t) :where [?c :located_in :varrow ?t :assert]]"
        ),
        rows(&[
            &[f.r("edwin"), i(10), i(10)],
            &[f.r("mara"), i(10), i(10)],
            &[f.r("greg"), i(10), i(300)],
        ])
    );
}

#[test]
fn min_max_of_strings_use_utf8_byte_order() {
    let f = world_a();
    // Edwin < Greg < Lisa < Mara < Tomas < "\u{c9}owyn" (0xC3 0x89 sorts after ASCII)
    assert_eq!(
        go(&f.w, "[:find (min ?n) (max ?n) :where [?c :name ?n]]"),
        rows(&[&[s("Edwin"), s("\u{c9}owyn")]])
    );
    assert_eq!(
        go(
            &f.w,
            "[:find ?p (min ?n) (max ?n) :at 15 :where [?c :located_in ?p] [?c :name ?n]]"
        ),
        rows(&[
            &[f.r("varrow"), s("Edwin"), s("Mara")],
            &[f.r("ashford"), s("Lisa"), s("Tomas")],
        ])
    );
}

#[test]
fn min_max_of_f64_and_i64_values() {
    let f = world_a();
    assert_eq!(
        go(
            &f.w,
            "[:find (min ?x) (max ?x) :at 15 :where [?c :weight ?x]]"
        ),
        rows(&[&[fl(0.25), fl(2.25)]])
    );
    assert_eq!(
        go(
            &f.w,
            "[:find (min ?g) (max ?g) :at 15 :where [?c :gold ?g]]"
        ),
        rows(&[&[i(5), i(10)]])
    );
}

// ------------------------------------------------------- columns and order

#[test]
fn columns_keep_find_order_and_aggregates_are_named_by_their_text() {
    let f = world_a();
    let rs = rs_of(
        &f.w,
        "[:find ?p (count ?c) (count-distinct ?c) (min ?n) (max ?n) (sum ?g) \
         :at 15 :where [?c :located_in ?p] [?c :name ?n] [?c :gold ?g]]",
        &[],
        &Limits::default(),
    )
    .unwrap();
    let names: Vec<&str> = rs.columns.iter().map(|v| v.0.as_str()).collect();
    assert_eq!(
        names,
        [
            "p",
            "(count ?c)",
            "(count-distinct ?c)",
            "(min ?n)",
            "(max ?n)",
            "(sum ?g)"
        ]
    );
    // spacing is normalized
    let rs = rs_of(
        &f.w,
        "[:find ( count   ?c ) :at 15 :where [?c :located_in ?p]]",
        &[],
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(rs.columns, vec![Var("(count ?c)".into())]);
}

#[test]
fn an_aggregate_column_may_come_first_and_rows_are_sorted_in_find_order() {
    let f = world_a();
    let rs = rs_of(
        &f.w,
        "[:find (count ?c) ?p :at 15 :where [?c :located_in ?p]]",
        &[],
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(rs.columns, vec![Var("(count ?c)".into()), Var("p".into())]);
    // sorted by the first column (the count), then the place
    assert_eq!(
        rs.rows,
        vec![vec![i(2), f.r("ashford")], vec![i(3), f.r("varrow")]]
    );
    let mut sorted = rs.rows.clone();
    sorted.sort();
    assert_eq!(rs.rows, sorted);
}

// ------------------------------------------------------------ empty and none

#[test]
fn empty_input_gives_no_rows_even_without_group_variables() {
    let f = world_a();
    // nobody is placed at tick 5
    for q in [
        "[:find ?p (count ?c) :at 5 :where [?c :located_in ?p]]",
        "[:find (count ?c) :at 5 :where [?c :located_in ?p]]",
        "[:find (sum ?g) :at 5 :where [?c :gold ?g]]",
        "[:find (min ?g) (max ?g) :at 5 :where [?c :gold ?g]]",
        "[:find (count-distinct ?c) :where [?c :located_in :varrow ?t :assert] [(> ?t 1000)]]",
    ] {
        assert!(go(&f.w, q).is_empty(), "{q}");
    }
}

#[test]
fn a_group_with_no_input_has_no_row() {
    let f = world_a();
    // at 15 ashford has lisa and tomas; at 25 tomas is gone, varrow has nobody
    // who left: the difference keeps only tomas at ashford and nobody at varrow
    assert_eq!(
        go(
            &f.w,
            "[:find ?p (count ?c) :at 15 :where [?c :located_in ?p] \
             (not (at 25 [?c :located_in ?p]))]"
        ),
        rows(&[&[f.r("ashford"), i(1)]])
    );
}

// ---------------------------------------------------------------- not, rules

#[test]
fn aggregates_over_a_query_with_not() {
    let f = world_a();
    // edwin and mara have the alias "Ed": only greg remains in varrow
    assert_eq!(
        go(
            &f.w,
            "[:find ?p (count ?c) :at 15 :where [?c :located_in ?p] (not [?c :alias \"Ed\"])]"
        ),
        rows(&[&[f.r("varrow"), i(1)], &[f.r("ashford"), i(2)]])
    );
    // a variable only a `not` mentions is not bound by a positive clause
    assert_eq!(
        kind(
            &f.w,
            "[:find (count ?a) :where [?c :located_in ?p] (not [?c :alias ?a])]"
        ),
        ErrorKind::UnboundVariable
    );
}

fn family() -> W {
    let mut w = World::in_memory(schema()).unwrap();
    let mut ids = BTreeMap::new();
    for h in ["a", "b", "y", "c", "d", "e"] {
        ids.insert(h, w.create_entity("CHARACTER", Some(h)).unwrap());
    }
    {
        let mut t = w.begin();
        // a -> b -> c -> d, a -> y -> c (a diamond), a -> e
        for (p, ch) in [
            ("a", "b"),
            ("a", "y"),
            ("b", "c"),
            ("y", "c"),
            ("c", "d"),
            ("a", "e"),
        ] {
            t.assert(ids[p], "parent_of", Value::Ref(ids[ch]), tk(0))
                .unwrap();
        }
        t.commit().unwrap();
    }
    W { w, ids }
}

const ANCESTOR: &str = ":rules [[(ancestor ?a ?d) [?a :parent_of ?d]] \
     [(ancestor ?a ?d) [?a :parent_of ?x] (ancestor ?x ?d)]]";

#[test]
fn aggregates_over_a_recursive_rule() {
    let f = family();
    // ancestors of each person (a set: a reaches c by two paths, counted once):
    // b {a}=1  y {a}=1  c {b,y,a}=3  d {c,b,y,a}=4  e {a}=1
    assert_eq!(
        go(
            &f.w,
            &format!("[:find ?d (count ?a) {ANCESTOR} :where (ancestor ?a ?d)]")
        ),
        rows(&[
            &[f.r("b"), i(1)],
            &[f.r("y"), i(1)],
            &[f.r("c"), i(3)],
            &[f.r("d"), i(4)],
            &[f.r("e"), i(1)],
        ])
    );
    // descendants: a {b,y,c,d,e}=5  b {c,d}=2  y {c,d}=2  c {d}=1
    assert_eq!(
        go(
            &f.w,
            &format!("[:find ?a (count-distinct ?d) {ANCESTOR} :where (ancestor ?a ?d)]")
        ),
        rows(&[
            &[f.r("a"), i(5)],
            &[f.r("b"), i(2)],
            &[f.r("y"), i(2)],
            &[f.r("c"), i(1)],
        ])
    );
    // total (ancestor, descendant) pairs: 1+1+3+4+1
    assert_eq!(
        go(
            &f.w,
            &format!("[:find (count ?a) {ANCESTOR} :where (ancestor ?a ?d)]")
        ),
        rows(&[&[i(10)]])
    );
}

// -------------------------------------------------------------- errors

#[test]
fn bad_aggregate_forms_are_syntax_errors() {
    let f = world_a();
    for q in [
        "[:find (foo ?c) :where [?c :located_in ?p]]",
        "[:find (avg ?g) :where [?c :gold ?g]]",
        "[:find (count) :where [?c :located_in ?p]]",
        "[:find (count ?c ?p) :where [?c :located_in ?p]]",
        "[:find (count 5) :where [?c :located_in ?p]]",
        "[:find (count \"c\") :where [?c :located_in ?p]]",
        "[:find (count :c) :where [?c :located_in ?p]]",
        "[:find (count _) :where [?c :located_in ?p]]",
        "[:find (count (count ?c)) :where [?c :located_in ?p]]",
        "[:find () :where [?c :located_in ?p]]",
        "[:find ((count ?c)) :where [?c :located_in ?p]]",
        "[:find (?c) :where [?c :located_in ?p]]",
        "[:find (5 ?c) :where [?c :located_in ?p]]",
        "[:find ?p (sum) :where [?c :located_in ?p]]",
    ] {
        assert_eq!(kind(&f.w, q), ErrorKind::Syntax, "{q}");
    }
}

#[test]
fn aggregates_are_not_allowed_in_rule_heads() {
    let f = world_a();
    assert_eq!(
        kind(
            &f.w,
            "[:find ?a :rules [[(r ?a (count ?b)) [?a :alias ?b]]] :where (r ?a ?n)]"
        ),
        ErrorKind::Syntax
    );
}

#[test]
fn the_aggregated_variable_must_be_bound_by_a_positive_clause() {
    let f = world_a();
    for q in [
        "[:find (count ?x) :where [?c :located_in ?p]]",
        "[:find ?p (sum ?x) :where [?c :located_in ?p]]",
        // a parameter is not bound by a clause
        "[:find (count ?x) :in ?x :where [?c :located_in ?p]]",
    ] {
        assert_eq!(kind(&f.w, q), ErrorKind::UnboundVariable, "{q}");
    }
}

#[test]
fn aggregate_type_errors() {
    let f = world_a();
    for q in [
        // min / max: i64, f64 or string only
        "[:find (min ?c) :where [?c :located_in ?p]]",
        "[:find (max ?p) :where [?c :located_in ?p]]",
        "[:find (min ?b) :where [?c :alive ?b]]",
        "[:find (max ?b) :where [?c :alive ?b]]",
        // sum: i64 or f64 only
        "[:find (sum ?n) :where [?c :name ?n]]",
        "[:find (sum ?p) :where [?c :located_in ?p]]",
        "[:find (sum ?b) :where [?c :alive ?b]]",
    ] {
        assert_eq!(kind(&f.w, q), ErrorKind::TypeMismatch, "{q}");
    }
    // the same variable aggregated and compared still type-checks as before
    assert!(prepare(&f.w, "[:find (count ?p) :where [?c :located_in ?p]]").is_ok());
}

// ------------------------------------------------------------- plan and limits

#[test]
fn explain_shows_an_aggregate_line_below_the_project() {
    let f = world_a();
    let p = prepare(
        &f.w,
        "[:find ?p (count ?c) :at 15 :where [?c :located_in ?p]]",
    )
    .unwrap();
    let text = p.plan.explain(&f.w);
    let lines: Vec<&str> = text.lines().collect();
    let agg = lines
        .iter()
        .position(|l| l.trim_start().starts_with("Aggregate ?p: count ?c -> "))
        .unwrap_or_else(|| panic!("no Aggregate line in:\n{text}"));
    let proj = lines
        .iter()
        .position(|l| l.trim_start().starts_with("Project"))
        .unwrap();
    let exp = lines
        .iter()
        .position(|l| l.trim_start().starts_with("Expand"))
        .unwrap();
    assert!(proj < agg && agg < exp, "{text}");
    // the aggregate is a child of the project, the expand a child of the aggregate
    let indent = |l: &str| l.len() - l.trim_start().len();
    assert!(indent(lines[agg]) > indent(lines[proj]));
    assert!(indent(lines[exp]) > indent(lines[agg]));
}

#[test]
fn groups_count_toward_max_rows() {
    let mut w = World::in_memory(schema()).unwrap();
    {
        let mut t = w.begin();
        for _ in 0..30 {
            let p = t.create_entity("PLACE", None).unwrap();
            let c = t.create_entity("CHARACTER", None).unwrap();
            t.assert(c, "located_in", Value::Ref(p), tk(0)).unwrap();
        }
        t.commit().unwrap();
    }
    let q = "[:find ?p (count ?c) :at 0 :where [?c :located_in ?p]]";
    let ok = rs_of(&w, q, &[], &Limits::default()).unwrap();
    assert_eq!(ok.rows.len(), 30);
    let tight = Limits {
        max_rows: 10,
        ..Limits::default()
    };
    let r = rs_of(&w, q, &[], &tight);
    assert!(
        matches!(r, Err(QueryError::RowLimit { limit: 10 })),
        "{r:?}"
    );
}

#[test]
fn aggregation_work_is_metered() {
    let mut w = World::in_memory(schema()).unwrap();
    {
        let mut t = w.begin();
        let p = t.create_entity("PLACE", None).unwrap();
        for _ in 0..200 {
            let c = t.create_entity("CHARACTER", None).unwrap();
            t.assert(c, "located_in", Value::Ref(p), tk(0)).unwrap();
        }
        t.commit().unwrap();
    }
    let q = "[:find (count ?c) :at 0 :where [?c :located_in ?p]]";
    let ok = rs_of(&w, q, &[], &Limits::default()).unwrap();
    assert_eq!(ok.rows, vec![vec![i(200)]]);
    let tight = Limits {
        max_steps: 50,
        ..Limits::default()
    };
    let r = rs_of(&w, q, &[], &tight);
    assert!(matches!(r, Err(QueryError::StepBudget { .. })), "{r:?}");
}

#[test]
fn a_param_may_appear_in_the_where_of_an_aggregate_query() {
    let f = world_a();
    let r = rs_of(
        &f.w,
        "[:find (count ?c) :in ?p :at 15 :where [?c :located_in ?p]]",
        &[("p", Value::Ref(f.ids["varrow"]))],
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(r.rows, vec![vec![i(3)]]);
}
