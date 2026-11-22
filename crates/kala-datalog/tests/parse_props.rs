//! Property tests for the parser (RED step).
mod common;
use common::*;
use kala_datalog::parse;
use proptest::prelude::*;

fn check_err_span(text: &str) {
    if let Err(e) = parse(text) {
        if let Some(s) = e.span {
            assert!(s.start <= s.end, "{s:?}");
            assert!(
                s.end <= text.len(),
                "span {s:?} past input len {}",
                text.len()
            );
            assert!(
                text.is_char_boundary(s.start),
                "start not on char boundary: {s:?}"
            );
            assert!(
                text.is_char_boundary(s.end),
                "end not on char boundary: {s:?}"
            );
        }
        let _ = e.to_string(); // Display must not panic
    }
}

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
    ":rules",
    ":handle",
    ":x",
    "?a",
    "?b",
    "?",
    "_",
    "$",
    "at",
    "not",
    "=",
    "!=",
    "<",
    "<=",
    ">",
    ">=",
    "true",
    "false",
    "0",
    "-1",
    "+3",
    "41",
    "9223372036854775807",
    "-9223372036854775808",
    "99999999999999999999",
    "1.5",
    "-2.0",
    "1e3",
    "1e999",
    "\"s\"",
    "\"é\"",
    "\"\\u0041\"",
    "\"\\uD800\"",
    "\"\\q\"",
    "\"open",
    "CHARACTER#1",
    "edge#9",
    "TYPE#",
    "#",
    ";c\n",
    ";",
    ",",
    " ",
    "\n",
    "\r\n",
    "é",
    "✓",
    "\\",
    "\"",
    "{",
    "}",
    "'",
    "@",
    "^",
];

fn soup() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(TOKENS), 0..60).prop_map(|v| v.join(" "))
}

/// Token soup that starts like a real query so deeper parser paths are reached.
fn query_soup() -> impl Strategy<Value = String> {
    (soup(), soup()).prop_map(|(a, b)| format!("[:find ?a :in $ :where {a}] {b}"))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn never_panics_on_arbitrary_strings(s in ".*") {
        check_err_span(&s);
    }

    #[test]
    fn never_panics_on_lossy_bytes(b in prop::collection::vec(any::<u8>(), 0..300)) {
        let s = String::from_utf8_lossy(&b).into_owned();
        check_err_span(&s);
    }

    #[test]
    fn never_panics_on_token_soup(s in soup()) {
        check_err_span(&s);
    }

    #[test]
    fn never_panics_on_query_shaped_soup(s in query_soup()) {
        check_err_span(&s);
    }

    #[test]
    fn never_panics_on_truncated_valid_query(cut in 0usize..200) {
        let full = "[:find ?name :in $ ?who :at -210 :canon 41 :where [?e :handle ?who] \
                    (at -200 [?a :located_in ?p]) [(!= ?a ?e)] [?a :name \"é\\u0041\"]]";
        let mut c = cut.min(full.len());
        while !full.is_char_boundary(c) { c -= 1; }
        check_err_span(&full[..c]);
    }

    #[test]
    fn deep_nesting_never_overflows(n in 0usize..5000, open in prop::sample::select(vec!['[', '('])) {
        let s: String = std::iter::repeat_n(open, n).collect();
        let h = std::thread::Builder::new().stack_size(512 * 1024)
            .spawn(move || { check_err_span(&s); }).unwrap();
        h.join().unwrap();
    }
}

// ---- round trip of generated valid queries ----

fn var() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["a", "b", "c", "who", "t1"]).prop_map(|s| s.to_string())
}

/// (source text, expected shape text) of an item.
fn item_strat() -> impl Strategy<Value = (String, String)> {
    prop_oneof![
        var().prop_map(|v| (format!("?{v}"), format!("?{v}"))),
        Just(("_".to_string(), "_".to_string())),
        prop::sample::select(vec!["edwin", "mara", "varrow"])
            .prop_map(|k| (format!(":{k}"), format!(":{k}"))),
        (1u32..5, 1u64..1000).prop_map(|(t, n)| (format!("TYPE{t}#{n}"), format!("#TYPE{t}#{n}"))),
        any::<i64>().prop_map(|n| (format!("{n}"), format!("{n}"))),
        any::<bool>().prop_map(|b| (format!("{b}"), format!("{b}"))),
        "[a-zA-Z0-9 é✓]{0,12}".prop_map(|s| (format!("\"{s}\""), format!("{s:?}"))),
        (-1000i32..1000).prop_map(|n| (
            format!("{n}.5"),
            format!("{:?}f", n as f64 + if n < 0 { -0.5 } else { 0.5 })
        )),
    ]
}

fn tick_strat() -> impl Strategy<Value = (String, String)> {
    prop_oneof![
        any::<i64>().prop_map(|n| (format!("{n}"), format!("{n}"))),
        var().prop_map(|v| (format!("?{v}"), format!("?{v}"))),
    ]
}

fn attr() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["ally_of", "located_in", "name", "child_of", "handle"])
        .prop_map(|s| s.to_string())
}

fn pattern_strat() -> impl Strategy<Value = (String, String)> {
    (item_strat(), attr(), item_strat()).prop_map(|((et, es), a, (vt, vs))| {
        (format!("[{et} :{a} {vt}]"), format!("[{es} :{a} {vs}]"))
    })
}

fn clause_strat() -> impl Strategy<Value = (String, String)> {
    prop_oneof![
        3 => pattern_strat(),
        1 => (tick_strat(), prop::collection::vec(pattern_strat(), 1..4)).prop_map(|((tt, ts), ps)| (
            format!("(at {tt} {})", ps.iter().map(|p| p.0.clone()).collect::<Vec<_>>().join(" ")),
            format!("(at {ts} {})", ps.iter().map(|p| p.1.clone()).collect::<Vec<_>>().join(" ")),
        )),
        1 => (prop::sample::select(vec!["=", "!=", "<", "<=", ">", ">="]), item_strat(), item_strat())
            .prop_map(|(op, (lt, ls), (rt, rs))| (format!("[({op} {lt} {rt})]"), format!("[({op} {ls} {rs})]"))),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn generated_valid_queries_round_trip(
        find in prop::collection::vec(var(), 1..4),
        db in any::<bool>(),
        params in prop::collection::vec(var(), 0..3),
        at in prop::option::of(tick_strat()),
        canon in prop::option::of(prop_oneof![
            any::<u64>().prop_map(|n| (format!("{n}"), format!("{n}"))),
            var().prop_map(|v| (format!("?{v}"), format!("?{v}"))),
        ]),
        clauses in prop::collection::vec(clause_strat(), 1..8),
        sep in prop::sample::select(vec![" ", ", ", "\n", " ; note\n", "\r\n"]),
    ) {
        let mut text = String::from("[:find");
        for v in &find { text += &format!(" ?{v}"); }
        let mut inputs = Vec::new();
        if db { inputs.push("$".to_string()); }
        for p in &params { inputs.push(format!("?{p}")); }
        if !inputs.is_empty() { text += &format!("{sep}:in {}", inputs.join(" ")); }
        if let Some((t, _)) = &at { text += &format!("{sep}:at {t}"); }
        if let Some((c, _)) = &canon { text += &format!("{sep}:canon {c}"); }
        text += &format!("{sep}:where");
        for (c, _) in &clauses { text += &format!("{sep}{c}"); }
        text += "]";

        let expected = format!(
            "find({}) in({}) at({}) canon({}) where{{ {} }}",
            find.iter().map(|v| format!("?{v}")).collect::<Vec<_>>().join(" "),
            inputs.join(" "),
            at.as_ref().map(|a| a.1.clone()).unwrap_or_default(),
            canon.as_ref().map(|a| a.1.clone()).unwrap_or_default(),
            clauses.iter().map(|c| c.1.clone()).collect::<Vec<_>>().join(" "),
        );
        let q = parse(&text).unwrap_or_else(|e| panic!("valid query refused: {e}\n{text}"));
        prop_assert_eq!(shape(&q), expected);
    }
}
