//! Slice 7, item 5 (RED): history patterns. A 4- or 5-element pattern
//! `[e a v t]` / `[e a v t op]` binds the change points of `(e, a, v)` at the
//! query's canon across ALL ticks (the query `:at` and `(at ..)` do not apply).
//!
//! Contract (tech-lead decisions):
//! - `t`: a variable, an integer, an `:in` param or `_` (an i64 tick). `op`: a
//!   variable (binds `Datum::Op`), `:assert`, `:retract` or `_`; any other
//!   keyword there is `Syntax`. `e` and `v` as in 3-element patterns.
//! - `a` is a schema attribute or an inverse name (read through `R`, like
//!   `View::history_scan`); `:kala_handle` is `Syntax`.
//! - A history pattern inside `(at ..)` is `Syntax`; so are 7 elements.
//! - Corrections are honoured by canon exactly as `View::history`.
//! - A bound `?t` is an ordinary i64: usable in predicates, `(at ?t ..)`, rule
//!   heads and `not`.
//! - Compiles to `Op::History` (explain `History [<e> :<attr> <v> <tick> <op>]`);
//!   index work is metered against the step budget.
//!
//! Base world (all written through kala-core). Characters greg edwin mara
//! lisa; places varrow ashford carnath.
//!   greg  missing_arm true @-230
//!   greg  located_in varrow @-300, retract @-250; ashford @-250
//!   edwin located_in varrow @-300, retract @-250
//!   mara  located_in varrow @-280 (never retracted)
//!   lisa  located_in carnath @-250
//!   edwin parent_of mara @-200, retracted @-100

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
    fn rows(&self, rs: &[&[Datum]]) -> Rows {
        rs.iter().map(|r| r.to_vec()).collect()
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

fn schema() -> Schema {
    let at = |n, t, k, c| AttrDef::new(n, t, k, c);
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(at("name", ValueType::Str, Kind::State, Cardinality::One))
        .attribute(at(
            "missing_arm",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(at("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(
            at("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .directed()
                .inverse("child_of"),
        )
        .attribute(at("present", ValueType::Ref, Kind::State, Cardinality::Many).directed())
        .attribute(at(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
}

/// Entities only, no facts.
fn fresh() -> W {
    let mut w = World::in_memory(schema()).unwrap();
    let mut ids = BTreeMap::new();
    for h in ["greg", "edwin", "mara", "lisa"] {
        ids.insert(h, w.create_entity("CHARACTER", Some(h)).unwrap());
    }
    for h in ["varrow", "ashford", "carnath"] {
        ids.insert(h, w.create_entity("PLACE", Some(h)).unwrap());
    }
    W { w, ids }
}

fn put(f: &mut W, e: &str, attr: &str, v: Value, tick: i64, op: Op) {
    let e = f.ids[e];
    match op {
        Op::Assert => f.w.assert(e, attr, v, tk(tick)).unwrap(),
        Op::Retract => f.w.retract(e, attr, v, tk(tick)).unwrap(),
    };
}

fn loc(f: &mut W, who: &str, place: &str, tick: i64, op: Op) {
    let v = Value::Ref(f.ids[place]);
    put(f, who, "located_in", v, tick, op);
}

fn base() -> W {
    let mut f = fresh();
    put(
        &mut f,
        "greg",
        "missing_arm",
        Value::Bool(true),
        -230,
        Op::Assert,
    );
    loc(&mut f, "greg", "varrow", -300, Op::Assert);
    loc(&mut f, "greg", "varrow", -250, Op::Retract);
    loc(&mut f, "greg", "ashford", -250, Op::Assert);
    loc(&mut f, "edwin", "varrow", -300, Op::Assert);
    loc(&mut f, "edwin", "varrow", -250, Op::Retract);
    loc(&mut f, "mara", "varrow", -280, Op::Assert);
    loc(&mut f, "lisa", "carnath", -250, Op::Assert);
    let m = Value::Ref(f.ids["mara"]);
    put(&mut f, "edwin", "parent_of", m.clone(), -200, Op::Assert);
    put(&mut f, "edwin", "parent_of", m, -100, Op::Retract);
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

// ------------------------------------------------------- the headline query

#[test]
fn when_did_greg_lose_his_arm() {
    let f = base();
    let text = "[:find ?t :where [:greg :missing_arm true ?t :assert]]";
    assert_eq!(go(&f.w, text), f.rows(&[&[i(-230)]]));
    // with the op as a variable it binds a Datum::Op
    let text = "[:find ?t ?op :where [:greg :missing_arm true ?t ?op]]";
    assert_eq!(go(&f.w, text), f.rows(&[&[i(-230), a()]]));
    // the 4-element form is the same as `?op` left free
    let text = "[:find ?t :where [:greg :missing_arm true ?t]]";
    assert_eq!(go(&f.w, text), f.rows(&[&[i(-230)]]));
}

#[test]
fn the_query_default_tick_does_not_apply_to_a_history_pattern() {
    let f = base();
    for at in ["-400", "-230", "0", "99999"] {
        let text = format!("[:find ?t :at {at} :where [:greg :missing_arm true ?t :assert]]");
        assert_eq!(go(&f.w, &text), f.rows(&[&[i(-230)]]), ":at {at}");
    }
}

// ------------------------------------------------- all change points vs core

#[test]
fn every_change_point_of_one_mover_equals_world_history() {
    let f = base();
    let want: Rows =
        f.w.history(f.ids["greg"], "located_in")
            .unwrap()
            .into_iter()
            .map(|c| {
                let v = match c.value {
                    Value::Ref(e) => Datum::Value(Value::Ref(e)),
                    other => panic!("{other:?}"),
                };
                vec![v, i(c.tick.0), Datum::Op(c.op)]
            })
            .collect();
    assert_eq!(want.len(), 3, "sanity: varrow A/R and ashford A");
    let text = "[:find ?p ?t ?op :where [:greg :located_in ?p ?t ?op]]";
    assert_eq!(go(&f.w, text), want);
}

#[test]
fn every_change_point_of_every_entity_equals_history_scan() {
    let f = base();
    let want: Rows =
        f.w.history_scan("located_in")
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
    assert_eq!(want.len(), 7);
    let text = "[:find ?g ?p ?t ?op :where [?g :located_in ?p ?t ?op]]";
    assert_eq!(go(&f.w, text), want);
}

// --------------------------------------------- constant, `_` and param ticks

#[test]
fn a_constant_tick_finds_who_moved_then() {
    let f = base();
    // at -250: greg leaves varrow and enters ashford, edwin leaves varrow, lisa
    // enters carnath. mara (-280) did not move at -250.
    let text = "[:find ?g :where [?g :located_in ?p -250]]";
    assert_eq!(
        go(&f.w, text),
        f.rows(&[&[f.r("greg")], &[f.r("edwin")], &[f.r("lisa")]])
    );
    let text = "[:find ?g :where [?g :located_in ?p -250 :retract]]";
    assert_eq!(go(&f.w, text), f.rows(&[&[f.r("greg")], &[f.r("edwin")]]));
    let text = "[:find ?g :where [?g :located_in ?p -251]]";
    assert!(go(&f.w, text).is_empty());
}

#[test]
fn an_underscore_tick_matches_every_change_point_and_binds_nothing() {
    let f = base();
    let text = "[:find ?g :where [?g :missing_arm true _]]";
    assert_eq!(go(&f.w, text), f.rows(&[&[f.r("greg")]]));
    // only retractions, any tick: greg and edwin each left varrow
    let text = "[:find ?g :where [?g :located_in _ _ :retract]]";
    assert_eq!(go(&f.w, text), f.rows(&[&[f.r("greg")], &[f.r("edwin")]]));
    // `_` for the op too
    let text = "[:find ?g :where [?g :located_in :carnath _ _]]";
    assert_eq!(go(&f.w, text), f.rows(&[&[f.r("lisa")]]));
}

#[test]
fn a_param_tick() {
    let f = base();
    let text = "[:find ?g :in $ ?t :where [?g :located_in ?p ?t :assert]]";
    assert_eq!(
        go_p(&f.w, text, &[("t", Value::I64(-250))]),
        f.rows(&[&[f.r("greg")], &[f.r("lisa")]])
    );
    assert_eq!(
        go_p(&f.w, text, &[("t", Value::I64(-280))]),
        f.rows(&[&[f.r("mara")]])
    );
    assert!(go_p(&f.w, text, &[("t", Value::I64(0))]).is_empty());
}

// ------------------------------------------------------------ canon / retcon

#[test]
fn a_retcon_that_moves_a_tick_shows_the_old_tick_at_the_older_canon() {
    let mut f = fresh();
    let res =
        f.w.assert(
            f.ids["greg"],
            "located_in",
            Value::Ref(f.ids["ashford"]),
            tk(-250),
        )
        .unwrap();
    let old = f.w.latest_tx().0;
    f.w.correct(
        res.entries[0],
        Correction::Replace {
            value: None,
            tick: tk(-280),
            op: None,
            precision: None,
        },
    )
    .unwrap();
    let new = f.w.latest_tx().0;
    assert!(new > old);
    let q =
        |canon: &str| format!("[:find ?t ?op {canon} :where [:greg :located_in :ashford ?t ?op]]");
    assert_eq!(go(&f.w, &q("")), f.rows(&[&[i(-280), a()]]));
    assert_eq!(
        go(&f.w, &q(&format!(":canon {new}"))),
        f.rows(&[&[i(-280), a()]])
    );
    assert_eq!(
        go(&f.w, &q(&format!(":canon {old}"))),
        f.rows(&[&[i(-250), a()]])
    );
}

#[test]
fn a_retcon_that_deletes_is_absent_after_and_present_before() {
    let mut f = fresh();
    let res =
        f.w.assert(
            f.ids["lisa"],
            "located_in",
            Value::Ref(f.ids["carnath"]),
            tk(-250),
        )
        .unwrap();
    let old = f.w.latest_tx().0;
    f.w.correct(res.entries[0], Correction::Delete).unwrap();
    let text = "[:find ?t :where [:lisa :located_in :carnath ?t]]";
    assert!(go(&f.w, text).is_empty());
    let text = format!("[:find ?t :canon {old} :where [:lisa :located_in :carnath ?t]]");
    assert_eq!(go(&f.w, &text), f.rows(&[&[i(-250)]]));
}

// ------------------------------------------------------ inverse and edge props

#[test]
fn an_inverse_name_gives_the_reversed_pairs() {
    let f = base();
    let text = "[:find ?c ?p ?t ?op :where [?c :child_of ?p ?t ?op]]";
    assert_eq!(
        go(&f.w, text),
        f.rows(&[
            &[f.r("mara"), f.r("edwin"), i(-200), a()],
            &[f.r("mara"), f.r("edwin"), i(-100), x()],
        ])
    );
    let fwd = go(
        &f.w,
        "[:find ?c ?p ?t ?op :where [?p :parent_of ?c ?t ?op]]",
    );
    assert_eq!(go(&f.w, text), fwd);
}

#[test]
fn an_edge_property_has_its_own_history() {
    let mut f = fresh();
    let (e, c) = (f.ids["edwin"], f.ids["carnath"]);
    f.w.assert(e, "present", Value::Ref(c), tk(-300)).unwrap();
    let edge = EntityId::from(f.w.edge_id(e, "present", c).expect("edge interned"));
    f.w.assert(edge, "disguised", Value::Bool(true), tk(-300))
        .unwrap();
    f.w.retract(edge, "disguised", Value::Bool(true), tk(-280))
        .unwrap();
    f.w.assert(edge, "disguised", Value::Bool(false), tk(-280))
        .unwrap();
    let b = |v| Datum::Value(Value::Bool(v));
    let edge_text = f.w.format_id(edge);
    let text = format!("[:find ?v ?t ?op :where [{edge_text} :disguised ?v ?t ?op]]");
    assert_eq!(
        go(&f.w, &text),
        f.rows(&[
            &[b(true), i(-300), a()],
            &[b(true), i(-280), x()],
            &[b(false), i(-280), a()],
        ])
    );
}

// ------------------------------------------------------------------ the trap

#[test]
fn joining_history_patterns_on_t_matches_only_change_points_not_overlaps() {
    // CLAUDE.md, "A tick variable only sees change points": Edwin was in Varrow
    // during [-300, -250) and Mara from -280 on, so they were together during
    // [-280, -250). But their change points are at different ticks (-300 / -250
    // vs -280), so joining the two patterns on ?t finds nothing. "At the same
    // time" questions need interval sets (slice 7, item 6: `during`/`intersect`).
    let mut f = fresh();
    loc(&mut f, "edwin", "varrow", -300, Op::Assert);
    loc(&mut f, "edwin", "varrow", -250, Op::Retract);
    loc(&mut f, "mara", "varrow", -280, Op::Assert);
    let joined = "[:find ?t :where [:edwin :located_in :varrow ?t :assert] \
                  [:mara :located_in :varrow ?t :assert]]";
    assert!(go(&f.w, joined).is_empty());
    // control: the overlap is real, a point read at -260 sees both
    let both = "[:find ?p :where (at -260 [:edwin :located_in ?p] [:mara :located_in ?p])]";
    assert_eq!(go(&f.w, both), f.rows(&[&[f.r("varrow")]]));
    // control: when change points DO coincide the join finds the tick
    loc(&mut f, "lisa", "varrow", -300, Op::Assert);
    let same = "[:find ?t :where [:edwin :located_in :varrow ?t :assert] \
                [:lisa :located_in :varrow ?t :assert]]";
    assert_eq!(go(&f.w, same), f.rows(&[&[i(-300)]]));
}

// ------------------------------------------------- ?t used by other clauses

#[test]
fn a_bound_tick_drives_an_at_clause() {
    let f = base();
    // "where was Mara when Greg moved to Ashford?" Greg enters ashford at -250;
    // Mara was in varrow then.
    let text = "[:find ?t ?p :where [:greg :located_in :ashford ?t :assert] \
                (at ?t [:mara :located_in ?p])]";
    assert_eq!(go(&f.w, text), f.rows(&[&[i(-250), f.r("varrow")]]));
}

#[test]
fn a_bound_tick_in_a_predicate() {
    let f = base();
    let text = "[:find ?t :where [:greg :located_in ?p ?t] [(< ?t -260)]]";
    assert_eq!(go(&f.w, text), f.rows(&[&[i(-300)]]));
}

#[test]
fn a_history_pattern_in_a_rule_body_and_a_bound_tick_in_the_head() {
    let f = base();
    let text = "[:find ?g ?t :rules [[(left ?g ?t) [?g :located_in ?p ?t :retract]]] \
                :where (left ?g ?t)]";
    assert_eq!(
        go(&f.w, text),
        f.rows(&[&[f.r("greg"), i(-250)], &[f.r("edwin"), i(-250)]])
    );
}

#[test]
fn a_history_pattern_inside_a_not() {
    let f = base();
    // assert change points with no retract of anything at that same tick by the
    // same character: greg -300, edwin -300, mara -280, lisa -250 (greg's -250
    // assert coincides with his retract of varrow, so it is out).
    let text = "[:find ?g ?t :where [?g :located_in _ ?t :assert] \
                (not [?g :located_in _ ?t :retract])]";
    assert_eq!(
        go(&f.w, text),
        f.rows(&[
            &[f.r("greg"), i(-300)],
            &[f.r("edwin"), i(-300)],
            &[f.r("mara"), i(-280)],
            &[f.r("lisa"), i(-250)],
        ])
    );
}

// ------------------------------------------------------- plan, budget, errors

#[test]
fn explain_shows_a_history_operator() {
    let f = base();
    let p = prepare(&f.w, "[:find ?t :where [:greg :located_in ?p ?t :assert]]").unwrap();
    let text = p.plan.explain(&f.w);
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("History ["))
        .unwrap_or_else(|| panic!("no History line in\n{text}"));
    assert!(line.contains(":located_in"), "{line}");
    assert!(line.contains("?t"), "{line}");
    assert!(line.trim_end().ends_with(":assert]"), "{line}");
}

#[test]
fn history_index_work_is_charged_to_the_step_budget() {
    let mut f = fresh();
    {
        let mut t = f.w.begin();
        for n in 0..200 {
            let e = t.create_entity("CHARACTER", None).unwrap();
            let p = f.ids["varrow"];
            t.assert(e, "located_in", Value::Ref(p), tk(n)).unwrap();
            t.retract(e, "located_in", Value::Ref(p), tk(n + 1000))
                .unwrap();
        }
        t.commit().unwrap();
    }
    let text = "[:find ?g ?t :where [?g :located_in ?p ?t ?op]]";
    assert_eq!(go(&f.w, text).len(), 400);
    let lim = Limits {
        max_steps: 50,
        ..Limits::default()
    };
    match try_run(&f.w, text, &[], &lim) {
        Err(QueryError::StepBudget { limit: 50 }) => {}
        other => panic!("expected StepBudget, got {other:?}"),
    }
}

#[test]
fn history_pattern_errors_are_syntax_errors() {
    let f = base();
    for (what, text) in [
        (
            ":kala_handle in a history pattern",
            "[:find ?t :where [:greg :kala_handle ?h ?t]]",
        ),
        (
            "an unknown op keyword",
            "[:find ?t :where [:greg :located_in ?p ?t :foo]]",
        ),
        (
            "a history pattern inside (at ..)",
            "[:find ?t :where (at -250 [:greg :located_in ?p ?t])]",
        ),
        (
            "a 5-element history pattern inside (at ..)",
            "[:find ?t :where (at -250 [:greg :located_in ?p ?t ?op])]",
        ),
        (
            "seven elements",
            "[:find ?t :where [?g :located_in ?p ?t ?op ?x ?extra]]",
        ),
    ] {
        assert_eq!(kind(&f.w, text), ErrorKind::Syntax, "{what}");
    }
}

// ------------------------------------------------------------- varrow.yaml

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

#[test]
fn every_tick_at_which_gregs_location_changed_on_varrow_yaml() {
    let w = load_fixture(VARROW).expect("fixture loads");
    let greg = w.entity_by_handle("greg").unwrap();
    let h = |n: &str| w.entity_by_handle(n).unwrap();
    // Hand-derived from the YAML (the retcon in tx 11 touches Edwin's alliances
    // and a typo, not Greg's location):
    //   tx 5: greg located_in varrow  @200  assert
    //   tx 7: greg located_in ashford @601  assert
    //   tx 8: greg located_in carnath @-100 assert, @150 retract
    //   tx 9: greg located_in varrow  @250  retract
    let want: Rows = [
        (-100, "carnath", a()),
        (150, "carnath", x()),
        (200, "varrow", a()),
        (250, "varrow", x()),
        (601, "ashford", a()),
    ]
    .into_iter()
    .map(|(t, p, op)| vec![Datum::Value(Value::Ref(h(p))), i(t), op])
    .collect();
    // the core API agrees with the derivation, so only the query is under test
    let core: Rows = w
        .history(greg, "located_in")
        .unwrap()
        .into_iter()
        .map(|c| vec![Datum::Value(c.value), i(c.tick.0), Datum::Op(c.op)])
        .collect();
    assert_eq!(core, want);
    let text = "[:find ?p ?t ?op :where [:greg :located_in ?p ?t ?op]]";
    assert_eq!(go(&w, text), want);
    let ticks: Rows = [-100, 150, 200, 250, 601]
        .iter()
        .map(|t| vec![i(*t)])
        .collect();
    assert_eq!(go(&w, "[:find ?t :where [:greg :located_in _ ?t]]"), ticks);
}

#[test]
fn the_varrow_retcon_is_honoured_by_canon() {
    let w = load_fixture(VARROW).expect("fixture loads");
    let latest = w.latest_tx().0;
    let before = latest - 1; // the retcon is the last transaction
    // Edwin ~ Greg: asserted @-280, retracted @-190 (retconned to @-200 in the
    // last tx); the prequel in tx 8 adds assert @-320 and retract @-300.
    // Wren: asserted @-170, deleted by the retcon.
    let q = |who: &str, canon: u64| {
        format!("[:find ?t ?op :canon {canon} :where [:edwin :ally_of :{who} ?t ?op]]")
    };
    let rows =
        |rs: &[(i64, Datum)]| -> Rows { rs.iter().map(|(t, o)| vec![i(*t), o.clone()]).collect() };
    let new = rows(&[(-320, a()), (-300, x()), (-280, a()), (-200, x())]);
    let old = rows(&[(-320, a()), (-300, x()), (-280, a()), (-190, x())]);
    // the core API agrees with the derivation at both canons (only the query is
    // under test below)
    let edwin = w.entity_by_handle("edwin").unwrap();
    let core = |who: &str, canon: u64| -> Rows {
        let who = Value::Ref(w.entity_by_handle(who).unwrap());
        w.at_canon(TxId(canon))
            .history(edwin, "ally_of")
            .unwrap()
            .into_iter()
            .filter(|c| c.value == who)
            .map(|c| vec![i(c.tick.0), Datum::Op(c.op)])
            .collect()
    };
    assert_eq!(core("greg", latest), new);
    assert_eq!(core("greg", before), old);
    assert_eq!(core("wren", latest), Rows::new());
    assert_eq!(core("wren", before), rows(&[(-170, a())]));
    assert_eq!(go(&w, &q("greg", latest)), new);
    assert_eq!(go(&w, &q("greg", before)), old);
    assert_eq!(go(&w, &q("wren", latest)), Rows::new());
    assert_eq!(go(&w, &q("wren", before)), rows(&[(-170, a())]));
}
