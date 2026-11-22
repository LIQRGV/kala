//! Item 4 properties: prepare never panics on mutated valid queries; single-pattern
//! queries match the native API at random ticks.
use kala_core::*;
use kala_datalog::prepare;
use kala_query::{Limits, Params, run};
use proptest::prelude::*;
use std::collections::BTreeSet;

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

fn varrow() -> &'static World {
    thread_local!(static W: &'static World = Box::leak(Box::new(load_fixture(VARROW).unwrap())));
    W.with(|w| *w)
}

const SEEDS: &[&str] = &[
    "[:find ?name :in $ ?who :at -210 :canon 41 :where [?e :handle ?who] [?e :ally_of ?a] (at -200 [?a :located_in ?p]) [?a :name ?name] [(!= ?a ?e)]]",
    "[:find ?a ?t :where [?a :located_in :varrow] [?a :name \"Edwin\"] (at ?t [?a :alive true])]",
    "[:find ?c :where [?c :child_of ?p] [?p :ruled_by _] [(< ?c ?p)]]",
    "[:find ?e :in ?t :where [?e :name ?n] [?e :disguised 1.5] (at ?t [?e :present CHARACTER#1])]",
];

const TOKENS: &[&str] = &[
    "[",
    "]",
    "(",
    ")",
    ":find",
    ":in",
    ":at",
    ":canon",
    ":where",
    "$",
    "?a",
    "?e",
    "?zz",
    "_",
    ":edwin",
    ":handle",
    ":name",
    ":ally_of",
    ":nope",
    "CHARACTER#1",
    "CHARACTER#999",
    "edge#1",
    "edge#0",
    "\"x\"",
    "\"\"",
    "0",
    "-1",
    "9223372036854775807",
    "1.5",
    "1e3",
    "true",
    "false",
    "at",
    "=",
    "!=",
    "<",
    ">=",
    "?t",
    "?who",
    "\u{202E}",
    "\u{7}",
    "é",
    " ",
    "\n",
    ";",
    ",",
];

fn mutate(seed: &str, ops: &[(u8, usize, usize, usize)]) -> String {
    let mut chars: Vec<char> = seed.chars().collect();
    for &(kind, a, b, tok) in ops {
        let len = chars.len();
        let i = if len == 0 { 0 } else { a % (len + 1) };
        match kind % 4 {
            0 => {
                let j = (i + b % 12).min(len);
                chars.drain(i..j);
            }
            1 => {
                let t: Vec<char> = TOKENS[tok % TOKENS.len()].chars().collect();
                chars.splice(i..i, t);
            }
            2 => {
                let j = (i + 1 + b % 8).min(len);
                let t: Vec<char> = TOKENS[tok % TOKENS.len()].chars().collect();
                chars.splice(i..j, t);
            }
            _ => chars.truncate(i),
        }
    }
    chars.into_iter().collect()
}

fn check(text: &str) {
    match prepare(varrow(), text) {
        Ok(p) => {
            // a plan that compiled must also run without panicking (errors are fine)
            let _ = run(varrow(), &p.plan, &Params::new(), &Limits::default());
            let _ = p.plan.explain(varrow());
        }
        Err(e) => {
            if let Some(s) = e.span {
                assert!(
                    s.start <= s.end && s.end <= text.len(),
                    "span {s:?} out of range for {text:?}"
                );
                assert!(text.is_char_boundary(s.start) && text.is_char_boundary(s.end));
                let d = e.to_string();
                assert!(d.starts_with("line "), "unlocated error {d:?} for {text:?}");
                assert!(
                    !d.chars().any(|c| c.is_control() || c == '\u{202E}'),
                    "raw char in {d:?}"
                );
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn prepare_never_panics_on_mutated_queries(
        seed in 0..SEEDS.len(),
        ops in prop::collection::vec((any::<u8>(), any::<usize>(), any::<usize>(), any::<usize>()), 0..6),
    ) {
        check(&mutate(SEEDS[seed], &ops));
    }

    #[test]
    fn prepare_never_panics_on_token_soup(toks in prop::collection::vec(0..TOKENS.len(), 0..40)) {
        let body: Vec<&str> = toks.iter().map(|&i| TOKENS[i]).collect();
        check(&format!("[:find ?e :where {}]", body.join(" ")));
        check(&body.join(" "));
    }
}

const ATTRS: &[&str] = &[
    "name",
    "alias",
    "label",
    "located_in",
    "ruled_by",
    "born_in",
    "bought",
    "present",
    "biography",
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// `[?e :attr ?v]` at tick T equals `attr_scan(attr, T)`.
    #[test]
    fn variable_pattern_equals_attr_scan(ai in 0..ATTRS.len(), t in -450i64..750) {
        let w = varrow();
        let attr = ATTRS[ai];
        let text = format!("[:find ?e ?v :in $ ?t :at ?t :where [?e :{attr} ?v]]");
        let p = prepare(w, &text).unwrap();
        let ps: Params = [("t".to_string(), Value::I64(t))].into();
        let got: BTreeSet<Vec<Value>> = run(w, &p.plan, &ps, &Limits::default()).unwrap().rows.into_iter()
            .map(|r| r.into_iter().map(|d| d.as_value().unwrap().clone()).collect()).collect();
        let native: BTreeSet<Vec<Value>> =
            w.attr_scan(attr, Tick(t)).unwrap().into_iter().map(|(e, v)| vec![Value::Ref(e), v]).collect();
        prop_assert_eq!(got, native);
    }

    /// `[?e :attr ?v]` with the entity as a parameter equals `as_of` (or `neighbors` for the
    /// bidirectional attribute).
    #[test]
    fn entity_param_pattern_equals_as_of(ai in 0..ATTRS.len(), ei in 0usize..8, t in -450i64..750) {
        let w = varrow();
        let attr = ATTRS[ai];
        let who = ["edwin", "greg", "mara", "lisa", "tomas", "hild", "varrow", "ashford"][ei];
        let e = w.entity_by_handle(who).unwrap();
        let text = format!("[:find ?v :in $ ?e ?t :at ?t :where [?e :{attr} ?v]]");
        let p = prepare(w, &text).unwrap();
        let ps: Params = [("e".to_string(), Value::Ref(e)), ("t".to_string(), Value::I64(t))].into();
        let got: BTreeSet<Vec<Value>> = run(w, &p.plan, &ps, &Limits::default()).unwrap().rows.into_iter()
            .map(|r| r.into_iter().map(|d| d.as_value().unwrap().clone()).collect()).collect();
        let native: BTreeSet<Vec<Value>> = w.as_of(e, attr, Tick(t)).unwrap().into_iter().map(|v| vec![v]).collect();
        prop_assert_eq!(got, native);
    }

    /// Bound ref value (R index side) equals the native reverse read.
    #[test]
    fn bound_ref_value_equals_neighbors_in(pi in 0usize..3, t in -450i64..750) {
        let w = varrow();
        let place = ["varrow", "ashford", "eldoria"][pi];
        let pid = w.entity_by_handle(place).unwrap();
        let text = "[:find ?x :in $ ?p ?t :at ?t :where [?x :located_in ?p]]";
        let p = prepare(w, text).unwrap();
        let ps: Params = [("p".to_string(), Value::Ref(pid)), ("t".to_string(), Value::I64(t))].into();
        let got: BTreeSet<Vec<Value>> = run(w, &p.plan, &ps, &Limits::default()).unwrap().rows.into_iter()
            .map(|r| r.into_iter().map(|d| d.as_value().unwrap().clone()).collect()).collect();
        let native: BTreeSet<Vec<Value>> = w.neighbors(pid, "located_in", Direction::In, Tick(t)).unwrap()
            .into_iter().map(|(e, _)| vec![Value::Ref(e)]).collect();
        prop_assert_eq!(got, native);
    }
}
