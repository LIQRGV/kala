//! Item 6 (RED until compile/prepare exist): the headline "Edwin's allies" Datalog query on
//! `varrow.yaml` must give exactly what the native API gives, at every change point of Edwin's
//! `ally_of` history +-1, at the latest tick, at the canon before the retcon (tx#11; the retcon
//! is tx#12), with constants and with `:in` params, mixed ticks and joined names.
use kala_core::*;
use kala_datalog::prepare;
use kala_query::{Limits, Params, run};
use proptest::prelude::*;
use std::collections::BTreeSet;

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");
const BEFORE_RETCON: u64 = 11;

fn world() -> &'static World {
    thread_local!(static W: &'static World =
        Box::leak(Box::new(load_fixture(VARROW).expect("fixture loads"))));
    W.with(|w| *w)
}

fn edwin(w: &World) -> EntityId {
    w.entity_by_handle("edwin").expect("edwin")
}

type Rows = BTreeSet<Vec<Value>>;

fn dl(w: &World, text: &str, params: &[(&str, Value)]) -> Rows {
    let p = match prepare(w, text) {
        Ok(p) => p,
        Err(e) => panic!("prepare failed for {text}\n  {e}"),
    };
    let ps: Params = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    run(w, &p.plan, &ps, &Limits::default())
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

fn native(v: &View<'_>, e: EntityId, t: i64) -> Rows {
    v.neighbors(e, "ally_of", Direction::Both, Tick(t))
        .expect("native neighbors")
        .into_iter()
        .map(|(n, _)| vec![Value::Ref(n)])
        .collect()
}

const HEADLINE: &str = r#"[:find ?a :in $ ?who :where [?e :kala_handle ?who] [?e :ally_of ?a]]"#;
const PARAM_ALL: &str = r#"[:find ?a :in $ ?who ?t ?c :at ?t :canon ?c
    :where [?e :kala_handle ?who] [?e :ally_of ?a]]"#;

fn who() -> (&'static str, Value) {
    ("who", Value::Str("edwin".into()))
}

/// Every change point of edwin's ally_of history (latest and pre-retcon canon) and the known
/// story ticks, each +-1, plus the latest tick and far-out ticks.
fn ticks(w: &World) -> Vec<i64> {
    let mut cps: BTreeSet<i64> = [
        -320, -300, -280, -260, -240, -208, -200, -190, -170, -150, -60,
    ]
    .into_iter()
    .collect();
    for v in [w.at_canon(w.latest_tx()), w.at_canon(TxId(BEFORE_RETCON))] {
        for c in v.history(edwin(w), "ally_of").expect("history") {
            cps.insert(c.tick.0);
        }
    }
    let mut out = BTreeSet::new();
    for c in cps {
        out.extend([c - 1, c, c + 1]);
    }
    out.insert(w.latest_tick().0);
    out.extend([-1000, 0, 10_000]);
    out.into_iter().collect()
}

#[test]
fn the_retcon_really_changes_edwins_allies() {
    // guards the canon tests below: they would be vacuous otherwise
    let w = world();
    let (old, new) = (w.at_canon(TxId(BEFORE_RETCON)), w.at_canon(w.latest_tx()));
    assert_eq!(w.latest_tx(), TxId(12));
    assert_ne!(native(&old, edwin(w), -195), native(&new, edwin(w), -195)); // greg until -190 vs -200
    assert_ne!(native(&old, edwin(w), -170), native(&new, edwin(w), -170)); // wren existed
}

#[test]
fn headline_with_param_tick_equals_native_at_every_change_point_latest_canon() {
    let w = world();
    let v = w.at_canon(w.latest_tx());
    for t in ticks(w) {
        let got = dl(
            w,
            r#"[:find ?a :in $ ?who ?t :at ?t :where [?e :kala_handle ?who] [?e :ally_of ?a]]"#,
            &[who(), ("t", Value::I64(t))],
        );
        assert_eq!(got, native(&v, edwin(w), t), "tick {t}");
    }
}

#[test]
fn headline_with_constant_at_equals_native() {
    let w = world();
    let v = w.at_canon(w.latest_tx());
    for t in ticks(w) {
        let text =
            format!("[:find ?a :in $ ?who :at {t} :where [?e :kala_handle ?who] [?e :ally_of ?a]]");
        assert_eq!(dl(w, &text, &[who()]), native(&v, edwin(w), t), "tick {t}");
        let constant = format!("[:find ?a :at {t} :where [:edwin :ally_of ?a]]");
        assert_eq!(dl(w, &constant, &[]), native(&v, edwin(w), t), "tick {t}");
    }
}

#[test]
fn headline_without_at_reads_at_the_latest_tick() {
    let w = world();
    let v = w.at_canon(w.latest_tx());
    assert_eq!(
        dl(w, HEADLINE, &[who()]),
        native(&v, edwin(w), w.latest_tick().0)
    );
}

#[test]
fn headline_at_the_canon_before_the_retcon_equals_native_at_that_canon() {
    let w = world();
    let old = w.at_canon(TxId(BEFORE_RETCON));
    for t in ticks(w) {
        let text = format!(
            "[:find ?a :in $ ?who :at {t} :canon {BEFORE_RETCON} :where [?e :kala_handle ?who] [?e :ally_of ?a]]"
        );
        assert_eq!(
            dl(w, &text, &[who()]),
            native(&old, edwin(w), t),
            "tick {t} canon 11"
        );
    }
}

#[test]
fn tick_and_canon_params_equal_native() {
    let w = world();
    for canon in [BEFORE_RETCON, 12] {
        let v = w.at_canon(TxId(canon));
        for t in ticks(w) {
            let got = dl(
                w,
                PARAM_ALL,
                &[who(), ("t", Value::I64(t)), ("c", Value::I64(canon as i64))],
            );
            assert_eq!(got, native(&v, edwin(w), t), "tick {t} canon {canon}");
        }
    }
}

#[test]
fn canon_past_latest_reads_the_latest() {
    let w = world();
    let v = w.at_canon(w.latest_tx());
    let got = dl(
        w,
        "[:find ?a :at -195 :canon 9999 :where [:edwin :ally_of ?a]]",
        &[],
    );
    assert_eq!(got, native(&v, edwin(w), -195));
}

#[test]
fn mixed_ticks_in_one_query_are_the_intersection_of_the_native_sets() {
    let w = world();
    let pairs = [
        (-210, -200),
        (-210, -150),
        (-250, -100),
        (-300, -280),
        (-210, -210),
        (-195, -175),
    ];
    for canon in [BEFORE_RETCON, 12] {
        let v = w.at_canon(TxId(canon));
        for (t1, t2) in pairs {
            let text = format!(
                "[:find ?a :canon {canon} :where (at {t1} [:edwin :ally_of ?a]) (at {t2} [:edwin :ally_of ?a])]"
            );
            let want: Rows = native(&v, edwin(w), t1)
                .intersection(&native(&v, edwin(w), t2))
                .cloned()
                .collect();
            assert_eq!(dl(w, &text, &[]), want, "ticks ({t1}, {t2}) canon {canon}");
        }
    }
}

#[test]
fn allies_at_one_moment_who_are_not_allies_at_another_is_native_difference() {
    // Datalog has no negation yet, so compare the two single-moment sets instead:
    // allies(-210) − allies(-200) is computed natively from the same two Datalog reads.
    let w = world();
    let a = dl(w, "[:find ?a :at -210 :where [:edwin :ally_of ?a]]", &[]);
    let b = dl(w, "[:find ?a :at -200 :where [:edwin :ally_of ?a]]", &[]);
    let v = w.at_canon(w.latest_tx());
    assert_eq!(a, native(&v, edwin(w), -210));
    assert_eq!(b, native(&v, edwin(w), -200));
    assert_ne!(a, b, "the moments should differ for this to say anything");
}

#[test]
fn mixed_ticks_with_variable_tick_bound_by_a_param() {
    let w = world();
    let v = w.at_canon(w.latest_tx());
    let text = "[:find ?a :in $ ?t1 ?t2 :where (at ?t1 [:edwin :ally_of ?a]) (at ?t2 [:edwin :ally_of ?a])]";
    for (t1, t2) in [(-210, -200), (-250, -100), (-300, 700)] {
        let want: Rows = native(&v, edwin(w), t1)
            .intersection(&native(&v, edwin(w), t2))
            .cloned()
            .collect();
        assert_eq!(
            dl(w, text, &[("t1", Value::I64(t1)), ("t2", Value::I64(t2))]),
            want,
            "({t1}, {t2})"
        );
    }
}

#[test]
fn names_of_allies_equal_native_as_of() {
    let w = world();
    for canon in [BEFORE_RETCON, 12] {
        let v = w.at_canon(TxId(canon));
        for t in ticks(w) {
            let text = format!(
                "[:find ?a ?n :at {t} :canon {canon} :where [:edwin :ally_of ?a] [?a :name ?n]]"
            );
            let mut want = Rows::new();
            for (n, _) in v
                .neighbors(edwin(w), "ally_of", Direction::Both, Tick(t))
                .unwrap()
            {
                for name in v.as_of(n, "name", Tick(t)).unwrap() {
                    want.insert(vec![Value::Ref(n), name]);
                }
            }
            assert_eq!(dl(w, &text, &[]), want, "tick {t} canon {canon}");
        }
    }
}

#[test]
fn names_only_find_matches_native_names() {
    let w = world();
    let v = w.at_canon(w.latest_tx());
    let t = -210;
    let want: Rows = v
        .neighbors(edwin(w), "ally_of", Direction::Both, Tick(t))
        .unwrap()
        .into_iter()
        .flat_map(|(n, _)| v.as_of(n, "name", Tick(t)).unwrap())
        .map(|name| vec![name])
        .collect();
    let got = dl(
        w,
        "[:find ?n :in $ ?who :at -210 :where [?e :kala_handle ?who] [?e :ally_of ?a] [?a :name ?n]]",
        &[who()],
    );
    assert_eq!(got, want);
    assert!(!got.is_empty());
}

#[test]
fn ally_query_is_symmetric_from_the_other_side() {
    // bidirectional: "who is allied with Mara" via the same pattern must agree with native
    let w = world();
    let mara = w.entity_by_handle("mara").unwrap();
    let v = w.at_canon(w.latest_tx());
    for t in [-300, -208, -150, -60, 0] {
        let text = format!("[:find ?a :at {t} :where [:mara :ally_of ?a]]");
        assert_eq!(dl(w, &text, &[]), native(&v, mara, t), "tick {t}");
    }
}

#[test]
fn sweep_every_tick_in_a_range_datalog_equals_native() {
    let w = world();
    let p = prepare(
        w,
        "[:find ?a :in $ ?who ?t ?c :at ?t :canon ?c :where [?e :kala_handle ?who] [?e :ally_of ?a]]",
    )
    .expect("prepare");
    for canon in [BEFORE_RETCON, 12] {
        let v = w.at_canon(TxId(canon));
        for t in -450..=720i64 {
            let ps: Params = [
                ("who".to_string(), Value::Str("edwin".into())),
                ("t".to_string(), Value::I64(t)),
                ("c".to_string(), Value::I64(canon as i64)),
            ]
            .into_iter()
            .collect();
            let got: Rows = run(w, &p.plan, &ps, &Limits::default())
                .unwrap_or_else(|e| panic!("tick {t} canon {canon}: {e}"))
                .rows
                .into_iter()
                .map(|r| {
                    r.into_iter()
                        .map(|d| d.as_value().unwrap().clone())
                        .collect()
                })
                .collect();
            assert_eq!(got, native(&v, edwin(w), t), "tick {t} canon {canon}");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Any tick (extremes included), any canon: Datalog == native.
    #[test]
    fn prop_datalog_equals_native(
        t in prop_oneof![-500i64..800, Just(i64::MIN), Just(i64::MAX), any::<i64>()],
        canon in 2u64..=12,
        who_edwin in any::<bool>(),
    ) {
        let w = world();
        let (handle, e) = if who_edwin { ("edwin", edwin(w)) } else { ("mara", w.entity_by_handle("mara").unwrap()) };
        let v = w.at_canon(TxId(canon));
        let got = dl(
            w,
            "[:find ?a :in $ ?who ?t ?c :at ?t :canon ?c :where [?e :kala_handle ?who] [?e :ally_of ?a]]",
            &[("who", Value::Str(handle.into())), ("t", Value::I64(t)), ("c", Value::I64(canon as i64))],
        );
        prop_assert_eq!(got, native(&v, e, t));
    }
}
