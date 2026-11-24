//! Slice 7, item 4 (RED): Datalog `(not clause+)` and the `allies(-210) -
//! allies(-200)` example of CLAUDE.md ("Comparing moments is a set operation").
//!
//! Contract: `(not ..)` in `:where` and rule bodies keeps a row iff NO way
//! exists to satisfy all its clauses given the row's bindings. Variables
//! bound outside are constraints; variables only inside are local
//! (existential). Each pattern inside reads at its own tick (`(at T ..)`
//! inside, else the default `:at`). A variable used by a predicate inside a
//! `not` must be bound outside by a positive clause, or by a positive
//! pattern/call inside, else `ErrorKind::UnboundVariable`. A `not` that
//! (transitively) calls a rule of its own recursive component is
//! `ErrorKind::Stratification` (new), naming the rule; a `not` over a
//! lower-stratum rule, recursive or not, is fine. `not-join` and `or-join` stay
//! `Unsupported`.
//!
//! World (built through kala-core), all facts at -500 unless noted:
//!   characters edwin mara tomas lisa greg hild (kind "character", names
//!   "Edwin".."Hild"); places varrow ashford carnath (kind "place", names).
//!   located_in: edwin, mara, tomas -> varrow; lisa -> carnath; greg ->
//!   ashford; hild nowhere.   age: edwin 40, mara 25, tomas 60, lisa 18.
//!   parent_of: lisa -> greg -> hild; edwin -> mara.
//!   ally_of (bidirectional): edwin~mara @-210; edwin~tomas @-210, retracted
//!   @-205; edwin~lisa @-202.

use kala_core::*;
use kala_datalog::{ErrorKind, prepare};
use kala_fixture::load_fixture;
use kala_query::{Limits, Params, run};
use std::collections::{BTreeMap, BTreeSet};

type Rows = BTreeSet<Vec<Value>>;

struct W {
    w: World,
    ids: BTreeMap<&'static str, EntityId>,
}

impl W {
    fn refs(&self, ns: &[&str]) -> Rows {
        ns.iter().map(|n| vec![Value::Ref(self.ids[n])]).collect()
    }
    fn pairs(&self, ps: &[(&str, &str)]) -> Rows {
        ps.iter()
            .map(|(a, b)| vec![Value::Ref(self.ids[a]), Value::Ref(self.ids[b])])
            .collect()
    }
}

fn schema() -> Schema {
    let a = |n, t, k, c| AttrDef::new(n, t, k, c);
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(a("name", ValueType::Str, Kind::State, Cardinality::One))
        .attribute(a("kind", ValueType::Str, Kind::State, Cardinality::One))
        .attribute(a("age", ValueType::I64, Kind::State, Cardinality::One))
        .attribute(a("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(a("parent_of", ValueType::Ref, Kind::State, Cardinality::Many).directed())
        .attribute(a("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional())
}

fn world() -> W {
    let mut w = World::in_memory(schema()).unwrap();
    let mut ids = BTreeMap::new();
    {
        let mut t = w.begin();
        let chars = [
            ("edwin", "Edwin"),
            ("mara", "Mara"),
            ("tomas", "Tomas"),
            ("lisa", "Lisa"),
            ("greg", "Greg"),
            ("hild", "Hild"),
        ];
        for (h, n) in chars {
            let e = t.create_entity("CHARACTER", Some(h)).unwrap();
            ids.insert(h, e);
            t.assert(e, "name", Value::Str(n.into()), Some(Tick(-500)))
                .unwrap();
            t.assert(e, "kind", Value::Str("character".into()), Some(Tick(-500)))
                .unwrap();
        }
        for (h, n) in [
            ("varrow", "Varrow"),
            ("ashford", "Ashford"),
            ("carnath", "Carnath"),
        ] {
            let e = t.create_entity("PLACE", Some(h)).unwrap();
            ids.insert(h, e);
            t.assert(e, "name", Value::Str(n.into()), Some(Tick(-500)))
                .unwrap();
            t.assert(e, "kind", Value::Str("place".into()), Some(Tick(-500)))
                .unwrap();
        }
        let r = |n: &str| Value::Ref(ids[n]);
        for (c, p) in [
            ("edwin", "varrow"),
            ("mara", "varrow"),
            ("tomas", "varrow"),
            ("lisa", "carnath"),
            ("greg", "ashford"),
        ] {
            t.assert(ids[c], "located_in", r(p), Some(Tick(-500)))
                .unwrap();
        }
        for (c, a) in [("edwin", 40), ("mara", 25), ("tomas", 60), ("lisa", 18)] {
            t.assert(ids[c], "age", Value::I64(a), Some(Tick(-500)))
                .unwrap();
        }
        for (p, c) in [("lisa", "greg"), ("greg", "hild"), ("edwin", "mara")] {
            t.assert(ids[p], "parent_of", r(c), Some(Tick(-500)))
                .unwrap();
        }
        t.assert(ids["edwin"], "ally_of", r("mara"), Some(Tick(-210)))
            .unwrap();
        t.assert(ids["edwin"], "ally_of", r("tomas"), Some(Tick(-210)))
            .unwrap();
        t.retract(ids["edwin"], "ally_of", r("tomas"), Some(Tick(-205)))
            .unwrap();
        t.assert(ids["edwin"], "ally_of", r("lisa"), Some(Tick(-202)))
            .unwrap();
        t.commit().unwrap();
    }
    W { w, ids }
}

fn rows_of(w: &World, text: &str) -> Rows {
    let p = prepare(w, text).unwrap_or_else(|e| panic!("prepare failed for {text}\n  {e}"));
    run(w, &p.plan, &Params::new(), &Limits::default())
        .unwrap_or_else(|e| panic!("run failed for {text}\n{}\n  {e}", p.plan.explain(w)))
        .rows
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|d| d.as_value().unwrap().clone())
                .collect()
        })
        .collect()
}

fn err_kind(w: &World, text: &str) -> kala_datalog::DatalogError {
    match prepare(w, text) {
        Ok(_) => panic!("expected an error for {text}"),
        Err(e) => e,
    }
}

const ALL: [&str; 6] = ["edwin", "mara", "tomas", "lisa", "greg", "hild"];

// ------------------------------------------------- the CLAUDE.md example

#[test]
fn allies_at_minus_210_minus_allies_at_minus_200() {
    let f = world();
    // allies(-210) = {mara, tomas}; allies(-200) = {mara, lisa} (tomas retracted
    // @-205, lisa asserted @-202). The difference is {tomas}.
    let text = "[:find ?a :where (at -210 [:edwin :ally_of ?a]) \
                (not (at -200 [:edwin :ally_of ?a]))]";
    assert_eq!(rows_of(&f.w, text), f.refs(&["tomas"]));
}

#[test]
fn the_same_difference_with_the_default_tick_on_the_positive_side() {
    let f = world();
    let text = "[:find ?a :at -210 :where [:edwin :ally_of ?a] \
                (not (at -200 [:edwin :ally_of ?a]))]";
    assert_eq!(rows_of(&f.w, text), f.refs(&["tomas"]));
}

#[test]
fn the_difference_the_other_way_round_and_at_equal_ticks() {
    let f = world();
    // allies(-200) - allies(-210) = {lisa}
    let text = "[:find ?a :where (at -200 [:edwin :ally_of ?a]) \
                (not (at -210 [:edwin :ally_of ?a]))]";
    assert_eq!(rows_of(&f.w, text), f.refs(&["lisa"]));
    // a set minus itself is empty
    let text = "[:find ?a :at -200 :where [:edwin :ally_of ?a] (not [:edwin :ally_of ?a])]";
    assert!(rows_of(&f.w, text).is_empty());
}

#[test]
fn a_default_tick_applies_inside_the_not_unless_it_names_its_own() {
    let f = world();
    // no :at, so the default is the world's latest tick, -202 (the newest fact):
    // allies(-202) = {mara, lisa}, and allies(-210) = {mara, tomas}.
    let text = "[:find ?a :where (at -210 [:edwin :ally_of ?a]) (not [:edwin :ally_of ?a])]";
    assert_eq!(rows_of(&f.w, text), f.refs(&["tomas"]));
}

// ------------------------------------------------------------- semantics

#[test]
fn not_with_both_variables_bound_outside() {
    let f = world();
    // co-located pairs at -210 are all in Varrow: edwin, mara, tomas. The
    // edwin~mara and edwin~tomas pairs are allied, so only mara/tomas are left.
    let text = "[:find ?a ?b :at -210 :where [?a :located_in ?p] [?b :located_in ?p] \
                [(!= ?a ?b)] (not [?a :ally_of ?b])]";
    assert_eq!(
        rows_of(&f.w, text),
        f.pairs(&[("mara", "tomas"), ("tomas", "mara")])
    );
}

#[test]
fn a_local_existential_variable_inside_not() {
    let f = world();
    // characters located in no place named "Varrow": ?p exists only inside
    let text = "[:find ?c :at 0 :where [?c :kind \"character\"] \
                (not [?c :located_in ?p] [?p :name \"Varrow\"])]";
    assert_eq!(rows_of(&f.w, text), f.refs(&["lisa", "greg", "hild"]));
}

#[test]
fn an_underscore_inside_not() {
    let f = world();
    let text = "[:find ?c :at 0 :where [?c :kind \"character\"] (not [?c :located_in _])]";
    assert_eq!(rows_of(&f.w, text), f.refs(&["hild"]));
}

#[test]
fn a_predicate_inside_not() {
    let f = world();
    // no ally older than oneself, at -210: edwin (40) has tomas (60) and mara (25)
    // as allies, so he is out; mara (25) has edwin (40), out; tomas (60) has edwin
    // (40) only, in; lisa (18) has no allies yet, in. ?xa is local to the not.
    let text = "[:find ?c :at -210 :where [?c :age ?ca] \
                (not [?c :ally_of ?x] [?x :age ?xa] [(> ?xa ?ca)])]";
    assert_eq!(rows_of(&f.w, text), f.refs(&["tomas", "lisa"]));
}

#[test]
fn nested_not_is_double_negation() {
    let f = world();
    let text = "[:find ?c :at 0 :where [?c :kind \"character\"] \
                (not (not [?c :located_in :varrow]))]";
    assert_eq!(rows_of(&f.w, text), f.refs(&["edwin", "mara", "tomas"]));
}

#[test]
fn a_nested_not_sees_variables_of_the_enclosing_not() {
    let f = world();
    // at -200: characters with no ally who is outside Varrow. Edwin's allies are
    // mara (Varrow) and lisa (Carnath), so only edwin is out. Everyone else has
    // no allies or only edwin (in Varrow).
    let text = "[:find ?c :at -200 :where [?c :kind \"character\"] \
                (not [?c :ally_of ?x] (not [?x :located_in :varrow]))]";
    assert_eq!(
        rows_of(&f.w, text),
        f.refs(&["mara", "tomas", "lisa", "greg", "hild"])
    );
}

#[test]
fn a_not_sharing_no_variable_is_a_global_condition() {
    let f = world();
    // edwin~lisa does not hold at -210: the not is true for every row
    let text = "[:find ?c :at -210 :where [?c :kind \"character\"] (not [:edwin :ally_of :lisa])]";
    assert_eq!(rows_of(&f.w, text), f.refs(&ALL));
    // it holds at -200: the not is false for every row
    let text = "[:find ?c :at -200 :where [?c :kind \"character\"] (not [:edwin :ally_of :lisa])]";
    assert!(rows_of(&f.w, text).is_empty());
}

#[test]
fn not_over_a_rule_call() {
    let f = world();
    let rules = "[(colocated ?a ?b) [?a :located_in ?p] [?b :located_in ?p] [(!= ?a ?b)]]";
    // nobody shares a place with them: carnath/ashford alone, hild nowhere
    let text = format!(
        "[:find ?c :at 0 :rules [{rules}] :where [?c :kind \"character\"] (not (colocated ?c ?o))]"
    );
    assert_eq!(rows_of(&f.w, &text), f.refs(&["lisa", "greg", "hild"]));
    // with a constant argument: edwin shares with mara and tomas, never with
    // himself (the rule says !=), so edwin, lisa, greg, hild are not colocated with edwin
    let text = format!(
        "[:find ?c :at 0 :rules [{rules}] :where [?c :kind \"character\"] (not (colocated ?c :edwin))]"
    );
    assert_eq!(
        rows_of(&f.w, &text),
        f.refs(&["edwin", "lisa", "greg", "hild"])
    );
}

#[test]
fn not_inside_a_rule_body() {
    let f = world();
    let rules = "[(colocated ?a ?b) [?a :located_in ?p] [?b :located_in ?p] [(!= ?a ?b)]] \
                 [(lonely ?c) [?c :kind \"character\"] (not (colocated ?c ?o))]";
    let text = format!("[:find ?x :at 0 :rules [{rules}] :where (lonely ?x)]");
    assert_eq!(rows_of(&f.w, &text), f.refs(&["lisa", "greg", "hild"]));
    // the rule body reads at the query default tick: `kind` exists only from -500
    let text = format!("[:find ?x :at -600 :rules [{rules}] :where (lonely ?x)]");
    assert!(rows_of(&f.w, &text).is_empty());
}

const ANC: &str = "[(ancestor ?a ?d) [?a :parent_of ?d]] \
                   [(ancestor ?a ?d) [?a :parent_of ?x] (ancestor ?x ?d)]";

#[test]
fn not_over_a_recursive_rule_of_a_lower_stratum() {
    let f = world();
    // Lisa's descendants are greg and hild; everyone else has no ancestor named Lisa.
    let text = format!(
        "[:find ?c :at 0 :rules [{ANC}] :where [?c :kind \"character\"] \
         (not (ancestor ?a ?c) [?a :name \"Lisa\"])]"
    );
    assert_eq!(
        rows_of(&f.w, &text),
        f.refs(&["edwin", "mara", "tomas", "lisa"])
    );
    // the same through a rule that holds the not
    let text = format!(
        "[:find ?x :at 0 :rules [{ANC} [(free ?c) [?c :kind \"character\"] \
         (not (ancestor ?a ?c) [?a :name \"Lisa\"])]] :where (free ?x)]"
    );
    assert_eq!(
        rows_of(&f.w, &text),
        f.refs(&["edwin", "mara", "tomas", "lisa"])
    );
}

// ---------------------------------------------------------------- errors

#[test]
fn a_predicate_variable_bound_nowhere_inside_not_is_unbound() {
    let f = world();
    let e = err_kind(
        &f.w,
        "[:find ?c :where [?c :kind \"character\"] (not [?c :age ?a] [(> ?a ?z)])]",
    );
    assert_eq!(e.kind, ErrorKind::UnboundVariable, "{e}");
}

#[test]
fn negation_through_direct_recursion_is_a_stratification_error() {
    let f = world();
    let text = "[:find ?a ?b :rules [[(gloomy ?a ?b) [?a :parent_of ?b]] \
                [(gloomy ?a ?b) [?a :parent_of ?x] (not (gloomy ?x ?b))]] :where (gloomy ?a ?b)]";
    let e = err_kind(&f.w, text);
    assert_eq!(e.kind, ErrorKind::Stratification, "{e}");
    assert!(e.to_string().contains("gloomy"), "{e}");
}

#[test]
fn negation_through_mutual_recursion_is_a_stratification_error() {
    let f = world();
    let text = "[:find ?a :rules [[(pale ?a) [?a :kind \"character\"] (not (quiet ?a))] \
                [(quiet ?a) [?a :kind \"character\"] (pale ?a)]] :where (pale ?a)]";
    let e = err_kind(&f.w, text);
    assert_eq!(e.kind, ErrorKind::Stratification, "{e}");
    assert!(e.to_string().contains("pale"), "{e}");
}

#[test]
fn negation_through_a_nested_not_is_still_a_stratification_error() {
    let f = world();
    let text = "[:find ?a ?b :rules [[(gloomy ?a ?b) [?a :parent_of ?b]] \
                [(gloomy ?a ?b) [?a :parent_of ?x] [?x :kind \"character\"] \
                (not (not (gloomy ?x ?b)))]] :where (gloomy ?a ?b)]";
    let e = err_kind(&f.w, text);
    assert_eq!(e.kind, ErrorKind::Stratification, "{e}");
}

#[test]
fn not_join_and_or_join_stay_unsupported() {
    let f = world();
    for text in [
        "[:find ?c :where [?c :kind \"character\"] (not-join [?c] [?c :located_in _])]",
        "[:find ?c :where (or-join [?c] [?c :name \"Edwin\"] [?c :name \"Mara\"])]",
    ] {
        let e = err_kind(&f.w, text);
        assert_eq!(e.kind, ErrorKind::Unsupported, "{text}: {e}");
    }
}

#[test]
fn an_empty_not_is_an_error() {
    let f = world();
    assert!(prepare(&f.w, "[:find ?c :where [?c :kind \"character\"] (not)]").is_err());
}

// ------------------------------------------------------ the fixture world

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

#[test]
fn villagers_allied_at_minus_200_with_no_ally_at_minus_100() {
    let w = load_fixture(VARROW).expect("fixture loads");
    // Villager pairs (v(2k-1), v(2k)) ally from -300+k; for odd k the alliance is
    // retracted at -150+k (-149, -147, ... -139), i.e. before -100. Even-k pairs
    // are never retracted. So at -200 all 24 villagers have an ally, and at -100
    // exactly the odd-k pairs have none: k = 1,3,5,7,9,11 -> v01 v02, v05 v06,
    // v09 v10, v13 v14, v17 v18, v21 v22. (No villager has any other alliance.)
    let text = "[:find ?v :where [?v :label \"Villager\"] (at -200 [?v :ally_of ?w]) \
                (not (at -100 [?v :ally_of ?x]))]";
    let want: Rows = [
        "v01", "v02", "v05", "v06", "v09", "v10", "v13", "v14", "v17", "v18", "v21", "v22",
    ]
    .iter()
    .map(|h| vec![Value::Ref(w.entity_by_handle(h).expect(h))])
    .collect();
    assert_eq!(rows_of(&w, text), want);
}
