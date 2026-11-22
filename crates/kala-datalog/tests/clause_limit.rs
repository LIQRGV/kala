//! Slice 7, item 00 (c), L1: a total clause limit.
//!
//! Expected API: `kala_datalog::MAX_CLAUSES: usize = 512`. The total clause
//! count (patterns, predicates, nested clauses inside `(at ..)`) above it is
//! `ErrorKind::Limit`; `MAX_PATTERNS` (256) is unchanged.

use kala_core::{World, load_fixture};
use kala_datalog::{ErrorKind, MAX_CLAUSES, MAX_PATTERNS, parse, prepare};

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

fn world() -> World {
    load_fixture(VARROW).expect("fixture loads")
}

fn preds(n: usize) -> String {
    "[(= 1 1)] ".repeat(n)
}

fn pats(n: usize) -> String {
    (0..n).map(|i| format!("[?e :ally_of ?a{i}] ")).collect()
}

fn query(clauses: &str) -> String {
    format!("[:find ?e :where {clauses}]")
}

#[test]
fn constants() {
    assert_eq!(MAX_CLAUSES, 512);
    assert_eq!(MAX_PATTERNS, 256);
}

#[test]
fn three_hundred_predicates_and_one_pattern_compile() {
    let q = query(&format!("[?e :ally_of ?a] {}", preds(300)));
    parse(&q).expect("301 clauses parse");
    prepare(&world(), &q).expect("301 clauses compile");
}

#[test]
fn six_hundred_predicates_and_one_pattern_are_a_limit_error() {
    let q = query(&format!("[?e :ally_of ?a] {}", preds(600)));
    let e = parse(&q).expect_err("601 clauses");
    assert_eq!(e.kind, ErrorKind::Limit, "{e}");
    let e = prepare(&world(), &q).expect_err("601 clauses");
    assert_eq!(e.kind, ErrorKind::Limit, "{e}");
}

#[test]
fn the_limit_is_on_the_total_not_per_kind() {
    // 200 patterns (< 256) + 300 predicates = 500 <= 512: fine.
    let ok = query(&format!("{}{}", pats(200), preds(300)));
    parse(&ok).expect("500 clauses");
    // 200 patterns + 400 predicates = 600 > 512, though neither kind alone is.
    let bad = query(&format!("{}{}", pats(200), preds(400)));
    assert_eq!(parse(&bad).expect_err("600 clauses").kind, ErrorKind::Limit);
}

#[test]
fn clauses_nested_in_at_count() {
    // 250 patterns inside one (at ..) + 300 predicates = 550 > 512.
    let bad = query(&format!("(at 0 {}) {}", pats(250), preds(300)));
    assert_eq!(parse(&bad).expect_err("550 clauses").kind, ErrorKind::Limit);
    // 200 nested + 300 predicates = 500 (501 if the `at` itself counts): fine.
    let ok = query(&format!("(at 0 {}) {}", pats(200), preds(300)));
    parse(&ok).expect("500 clauses");
}

#[test]
fn exactly_512_clauses_is_fine_and_513_is_not() {
    let ok = query(&preds(MAX_CLAUSES));
    // No pattern at all may be its own error; only the Limit kind matters.
    if let Err(e) = parse(&ok) {
        assert_ne!(
            e.kind,
            ErrorKind::Limit,
            "512 clauses are within the limit: {e}"
        );
    }
    let ok = query(&format!("[?e :ally_of ?a] {}", preds(MAX_CLAUSES - 1)));
    parse(&ok).expect("512 clauses");
    let bad = query(&format!("[?e :ally_of ?a] {}", preds(MAX_CLAUSES)));
    assert_eq!(parse(&bad).expect_err("513 clauses").kind, ErrorKind::Limit);
}

#[test]
fn pattern_limit_still_applies() {
    let bad = query(&pats(MAX_PATTERNS + 1));
    assert_eq!(
        parse(&bad).expect_err("257 patterns").kind,
        ErrorKind::Limit
    );
}

fn wrapped(n: usize) -> String {
    (0..n)
        .map(|i| format!("(at -1 [?e :ally_of ?a{i}]) "))
        .collect()
}

#[test]
fn each_at_wrapper_counts_as_a_clause() {
    // 256 wrapped patterns = 256 wrappers + 256 patterns = 512 clauses: fine.
    let ok = query(&wrapped(MAX_PATTERNS));
    parse(&ok).expect("512 clauses");
    prepare(&world(), &ok).expect("512 clauses compile");
    // One more predicate makes 513.
    let bad = query(&format!("{}{}", wrapped(MAX_PATTERNS), preds(1)));
    assert_eq!(parse(&bad).expect_err("513 clauses").kind, ErrorKind::Limit);
    let e = prepare(&world(), &bad).expect_err("513 clauses");
    assert_eq!(e.kind, ErrorKind::Limit, "{e}");
}
