//! Slice 7, item 8 (RED): `kala_datalog::parse_checks`, the story-check file
//! of `kala doctor --checks FILE`.
//!
//! Expected API (not built yet, so this file does not compile):
//!
//! ```ignore
//! pub const MAX_CHECKS_BYTES: usize = 1 << 20;
//! pub const MAX_CHECKS: usize = 256;
//! pub struct Check { pub name: String /* no ':' */, pub query: String, pub span: Span }
//! pub fn parse_checks(text: &str) -> Result<Vec<Check>, DatalogError>;
//! ```
//!
//! Contract: the file is an EDN subset (same lexer as queries: `;` comments,
//! commas are whitespace): exactly one top-level vector of checks; each check
//! is a vector `[:check-name [:find ...]]` of a keyword and one query vector.
//! `Check.query` is the query vector's exact source text; `Check.span` is a
//! byte range of the checks text inside which that check lies. The query is
//! NOT compiled here (that needs a world): only its size is limited
//! (MAX_QUERY_BYTES, `ErrorKind::Limit`). Anything else is an error located
//! in the checks file (`Display` is `line L, column C: message`): missing
//! name, non-keyword name, no query, extra elements, a non-vector query, an
//! empty file, trailing input, a duplicate name, too many checks, a file over
//! MAX_CHECKS_BYTES.

use kala_datalog::{
    DatalogError, ErrorKind, MAX_CHECKS, MAX_CHECKS_BYTES, MAX_QUERY_BYTES, parse_checks,
};
use proptest::prelude::*;

const Q1: &str = "[:find ?a :where [?a :name \"x\"]]";
const Q2: &str = "[:find ?a ?b :at 601 :where [:greg :located_in ?a] [:greg :located_in ?b]]";

fn ok(text: &str) -> Vec<kala_datalog::Check> {
    match parse_checks(text) {
        Ok(c) => c,
        Err(e) => panic!("expected Ok for {text:?}, got {e}"),
    }
}

fn err(text: &str) -> DatalogError {
    match parse_checks(text) {
        Ok(c) => panic!(
            "expected an error for {text:?}, got {:?}",
            c.iter().map(|c| &c.name).collect::<Vec<_>>()
        ),
        Err(e) => e,
    }
}

/// An error is located: `Display` starts `line L, column C: `; returns `L`.
fn line_of(e: &DatalogError) -> usize {
    let s = e.to_string();
    let rest = s
        .strip_prefix("line ")
        .unwrap_or_else(|| panic!("not located: {s}"));
    let (l, rest) = rest.split_once(", column ").expect("column part");
    let (c, _) = rest.split_once(": ").expect("message part");
    assert!(c.parse::<usize>().unwrap() >= 1, "{s}");
    l.parse().unwrap()
}

// ------------------------------------------------------------------ happy paths

#[test]
fn one_check() {
    let cs = ok(&format!("[[:two-places {Q1}]]"));
    assert_eq!(cs.len(), 1);
    assert_eq!(cs[0].name, "two-places");
    assert_eq!(cs[0].query, Q1);
}

#[test]
fn several_checks_keep_file_order() {
    let text = format!("[[:b {Q1}] [:a {Q2}] [:c {Q1}]]");
    let cs = ok(&text);
    let names: Vec<&str> = cs.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["b", "a", "c"]);
    assert_eq!(cs[1].query, Q2);
}

#[test]
fn an_empty_list_of_checks_is_fine() {
    assert!(ok("[]").is_empty());
    assert!(ok("  ; nothing to check\n [ ]\n").is_empty());
}

#[test]
fn comments_commas_and_newlines_are_whitespace() {
    let text = format!(
        "; story checks\n[\n  ; first\n  [:a, {Q1}] ; trailing\n  [:b\n   ; between name and query\n   {Q2}]\n]\n; end\n"
    );
    let cs = ok(&text);
    assert_eq!(cs.len(), 2);
    assert_eq!(cs[0].query, Q1);
    // the comment between the name and the query is not part of the query
    assert_eq!(cs[1].query, Q2);
}

#[test]
fn query_text_is_the_exact_source_slice() {
    // internal spacing and a comment inside the query are kept as written
    let q = "[:find ?a  ; who\n   :where [?a :name \"x\"]]";
    let cs = ok(&format!("[[:q {q}]]"));
    assert_eq!(cs[0].query, q);
}

#[test]
fn names_may_use_the_usual_keyword_characters() {
    for n in ["a", "dead-with-allies", "a_b", "a.b", "a1", "A", "x?", "y!"] {
        let cs = ok(&format!("[[:{n} {Q1}]]"));
        assert_eq!(cs[0].name, n, "{n}");
    }
}

#[test]
fn the_span_lies_in_the_file_on_char_boundaries_and_covers_the_check() {
    // a multi-byte char before the checks, inside a comment
    let text = format!("; caf\u{e9} \u{1f600}\n[[:one {Q1}]\n [:two {Q2}]]");
    let cs = ok(&text);
    for (c, q) in cs.iter().zip([Q1, Q2]) {
        let (s, e) = (c.span.start, c.span.end);
        assert!(s < e && e <= text.len(), "{:?}", c.span);
        assert!(text.is_char_boundary(s) && text.is_char_boundary(e));
        let slice = &text[s..e];
        assert!(slice.contains(&format!(":{}", c.name)), "{slice:?}");
        assert!(slice.contains(q), "{slice:?}");
    }
    // the second check is after the first
    assert!(cs[0].span.end <= cs[1].span.start);
}

// ------------------------------------------------------------------ malformed

#[test]
fn an_empty_or_comment_only_file_is_an_error() {
    for t in ["", "   \n", "; just a comment\n", ",,"] {
        err(t);
    }
}

#[test]
fn the_top_level_must_be_one_vector() {
    for t in [
        "[:a [:find ?x :where [?x :n 1]]]", // one check, not wrapped
        ":a",
        "5",
        "\"s\"",
        "(  )",
        "{}",
        "[[:a [:find ?x :where [?x :n 1]]]",   // unbalanced
        "[[:a [:find ?x :where [?x :n 1]]]]]", // extra bracket
    ] {
        err(t);
    }
}

#[test]
fn trailing_input_after_the_vector_is_an_error() {
    let good = format!("[[:a {Q1}]]");
    for tail in [
        "[]",
        " x",
        " :a",
        " 1",
        "\n[[:b [:find ?a :where [?a :n 1]]]]",
    ] {
        let e = err(&format!("{good}{tail}"));
        assert_eq!(line_of(&e), if tail.starts_with('\n') { 2 } else { 1 });
    }
    // a trailing comment is fine
    ok(&format!("{good} ; ok"));
}

#[test]
fn every_malformed_check_shape_is_an_error() {
    for (what, check) in [
        ("empty check", "[]".to_string()),
        ("missing name", format!("[{Q1}]")),
        ("name only", "[:a]".to_string()),
        ("name is a string", format!("[\"a\" {Q1}]")),
        ("name is a symbol", format!("[a {Q1}]")),
        ("name is a variable", format!("[?a {Q1}]")),
        ("name is a number", format!("[5 {Q1}]")),
        ("empty keyword", format!("[: {Q1}]")),
        ("query is a keyword", "[:a :find]".to_string()),
        ("query is a string", "[:a \"[:find ?x]\"]".to_string()),
        ("query is a list", "[:a (:find ?x)]".to_string()),
        ("query is a number", "[:a 7]".to_string()),
        ("extra element", format!("[:a {Q1} {Q2}]")),
        ("extra keyword", format!("[:a {Q1} :b]")),
        ("two names", format!("[:a :b {Q1}]")),
        ("check is a list", format!("(:a {Q1})")),
        ("check is a keyword", ":a".to_string()),
        ("check is a number", "3".to_string()),
        ("query before name", format!("[{Q1} :a]")),
    ] {
        let e = err(&format!("[{check}]"));
        assert!(!e.to_string().is_empty(), "{what}: empty error message");
        // a valid check before it doesn't rescue the file
        err(&format!("[[:ok {Q1}] {check}]"));
    }
}

#[test]
fn malformed_checks_are_located_at_their_line() {
    // the broken check is on line 4
    let text = format!("; c\n[\n [:a {Q1}]\n [:b]\n]");
    assert_eq!(line_of(&err(&text)), 4);
    // an unbalanced file is located too
    let text = format!("[\n [:a {Q1}]\n [:b {Q1}\n");
    let _ = line_of(&err(&text));
    // extra element on line 3
    let text = format!("[\n [:a {Q1}]\n [:b {Q1} 5]\n]");
    assert_eq!(line_of(&err(&text)), 3);
    // a non-keyword name on line 2
    let text = format!("[\n [\"b\" {Q1}]\n]");
    assert_eq!(line_of(&err(&text)), 2);
}

#[test]
fn a_column_is_counted_in_chars() {
    // `é` before the bad token on the same line: column counts chars, not bytes
    let e = err("[ ; caf\u{e9}\n [:a] ]");
    let s = e.to_string();
    assert!(s.starts_with("line 2, column "), "{s}");
}

#[test]
fn the_error_kind_of_shape_errors_is_syntax() {
    for t in [
        "",
        "[[:a]]",
        "[[\"a\" [:find ?x]]]",
        "[[:a [:find ?x] 1]]",
        "[] x",
    ] {
        assert_eq!(err(t).kind, ErrorKind::Syntax, "{t:?}");
    }
}

// ------------------------------------------------------------------ duplicates

#[test]
fn a_duplicate_name_is_an_error_located_at_the_second_use() {
    let text = format!("[\n [:a {Q1}]\n [:b {Q2}]\n [:a {Q2}]\n]");
    let e = err(&text);
    assert_eq!(line_of(&e), 4);
}

#[test]
fn names_differing_only_in_case_or_punctuation_are_distinct() {
    let cs = ok(&format!("[[:a {Q1}] [:A {Q1}] [:a-b {Q1}] [:a_b {Q1}]]"));
    assert_eq!(cs.len(), 4);
}

#[test]
fn the_same_query_under_two_names_is_fine() {
    assert_eq!(ok(&format!("[[:a {Q1}] [:b {Q1}]]")).len(), 2);
}

// ------------------------------------------------------------------ limits

fn n_checks(n: usize) -> String {
    let mut s = String::from("[");
    for i in 0..n {
        s.push_str(&format!("[:c{i} {Q1}]"));
    }
    s.push(']');
    s
}

#[test]
fn constants_are_the_documented_ones() {
    assert_eq!(MAX_CHECKS_BYTES, 1 << 20);
    assert_eq!(MAX_CHECKS, 256);
}

#[test]
fn the_check_count_limit_boundary() {
    assert_eq!(ok(&n_checks(MAX_CHECKS)).len(), MAX_CHECKS);
    let e = err(&n_checks(MAX_CHECKS + 1));
    assert_eq!(e.kind, ErrorKind::Limit, "{e}");
}

#[test]
fn the_file_size_limit_boundary() {
    let base = format!("[[:a {Q1}]]");
    let at = format!("{base}{}", " ".repeat(MAX_CHECKS_BYTES - base.len()));
    assert_eq!(at.len(), MAX_CHECKS_BYTES);
    assert_eq!(ok(&at).len(), 1);
    let over = format!("{at} ");
    assert_eq!(err(&over).kind, ErrorKind::Limit);
    // a size error never needs a location, but must not panic on Display
    let _ = err(&over).to_string();
}

#[test]
fn the_size_limit_counts_bytes_not_chars() {
    // a comment of 3-byte chars: char count under the limit, byte count over
    let base = format!("[[:a {Q1}]]\n; ");
    let n = (MAX_CHECKS_BYTES - base.len()) / 3 + 1;
    let text = format!("{base}{}", "\u{20ac}".repeat(n));
    assert!(text.chars().count() < MAX_CHECKS_BYTES && text.len() > MAX_CHECKS_BYTES);
    assert_eq!(err(&text).kind, ErrorKind::Limit);
}

#[test]
fn each_query_is_limited_to_max_query_bytes() {
    let head = "[:find ?a :where [?a :n 1]";
    let pad = |n: usize| format!("{head}{}]", " ".repeat(n));
    let at = pad(MAX_QUERY_BYTES - head.len() - 1);
    assert_eq!(at.len(), MAX_QUERY_BYTES);
    let cs = ok(&format!("[[:a {at}]]"));
    assert_eq!(cs[0].query.len(), MAX_QUERY_BYTES);
    let over = pad(MAX_QUERY_BYTES - head.len());
    assert_eq!(over.len(), MAX_QUERY_BYTES + 1);
    let e = err(&format!("[[:a {over}]]"));
    assert_eq!(e.kind, ErrorKind::Limit, "{e}");
    // the second of two checks is the oversized one: still refused
    err(&format!("[[:ok {Q1}] [:big {over}]]"));
}

#[test]
fn deep_nesting_is_refused_not_a_stack_overflow() {
    let text = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    err(&text);
    let q = format!("{}{}", "[".repeat(50_000), "]".repeat(50_000));
    let _ = parse_checks(&format!("[[:a {q}]]"));
}

// ------------------------------------------------------------------ never panics

fn spans_are_sane(text: &str, r: &Result<Vec<kala_datalog::Check>, DatalogError>) {
    match r {
        Ok(cs) => {
            assert!(cs.len() <= MAX_CHECKS);
            let mut names = std::collections::BTreeSet::new();
            for c in cs {
                assert!(names.insert(c.name.clone()), "duplicate {}", c.name);
                assert!(!c.name.starts_with(':') && !c.name.is_empty());
                assert!(c.query.starts_with('[') && c.query.ends_with(']'));
                assert!(c.query.len() <= MAX_QUERY_BYTES);
                assert!(c.span.start <= c.span.end && c.span.end <= text.len());
                assert!(text.is_char_boundary(c.span.start));
                assert!(text.is_char_boundary(c.span.end));
                assert!(text[c.span.start..c.span.end].contains(&c.query));
            }
        }
        Err(e) => {
            if let Some(s) = e.span {
                assert!(s.start <= s.end && s.end <= text.len(), "{s:?}");
                assert!(text.is_char_boundary(s.start) && text.is_char_boundary(s.end));
            }
            let _ = e.to_string();
        }
    }
}

const TOKENS: &[&str] = &[
    "[",
    "]",
    "(",
    ")",
    "{",
    "}",
    ":a",
    ":b",
    ":find",
    ":where",
    "?x",
    "\"s\"",
    "\"",
    "5",
    "-3",
    ";",
    "\n",
    " ",
    ",",
    "[:a [:find ?x :where [?x :n 1]]]",
    "[:find ?x :where [?x :n 1]]",
    "\u{e9}",
    "\u{1f600}",
    "#",
    "\\",
    ":",
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn arbitrary_text_never_panics(s in ".{0,200}") {
        let r = parse_checks(&s);
        spans_are_sane(&s, &r);
    }

    #[test]
    fn token_soup_never_panics(toks in proptest::collection::vec(proptest::sample::select(TOKENS), 0..40)) {
        let s: String = toks.concat();
        let r = parse_checks(&s);
        spans_are_sane(&s, &r);
    }

    /// Any list of distinct valid names with valid queries round-trips.
    #[test]
    fn generated_valid_files_round_trip(
        names in proptest::collection::btree_set("[a-z][a-z0-9_-]{0,8}", 0..20),
        sep in proptest::sample::select(vec![" ", "\n", ", ", " ; c\n"]),
    ) {
        let names: Vec<String> = names.into_iter().collect();
        let body: Vec<String> = names.iter().map(|n| format!("[:{n} {Q2}]")).collect();
        let text = format!("[{}]", body.join(sep));
        let cs = parse_checks(&text).unwrap_or_else(|e| panic!("{text:?}: {e}"));
        prop_assert_eq!(cs.len(), names.len());
        for (c, n) in cs.iter().zip(&names) {
            prop_assert_eq!(&c.name, n);
            prop_assert_eq!(c.query.as_str(), Q2);
        }
        spans_are_sane(&text, &Ok(cs));
    }
}
