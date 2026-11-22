//! Slice 7, item 2 (RED): the `:rules` grammar, reserved names, head shapes,
//! section order and MAX_RULES.
//!
//! Expected API: `kala_datalog::MAX_RULES: usize = 64`, `ErrorKind::Arity`
//! and `ErrorKind::UnknownRule` (the latter two are used in rules_exec.rs).
//! Valid rule queries must `parse` (today: `Unsupported`, "slice 7").

use kala_core::{World, load_fixture};
use kala_datalog::{ErrorKind, MAX_CLAUSES, MAX_PATTERNS, MAX_RULES, parse, prepare};

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

fn world() -> World {
    load_fixture(VARROW).expect("fixture loads")
}

fn ok(text: &str) {
    if let Err(e) = parse(text) {
        panic!("expected to parse: {text}\n  {e} ({:?})", e.kind);
    }
}

fn syntax(text: &str) {
    match parse(text) {
        Err(e) => {
            assert_eq!(e.kind, ErrorKind::Syntax, "{text}\n  {e}");
            assert!(e.span.is_some(), "Syntax errors carry a span: {text}");
        }
        Ok(_) => panic!("expected a Syntax error: {text}"),
    }
}

const GP: &str = "[(grandparent ?g ?c) [?g :ally_of ?p] [?p :ally_of ?c]]";

#[test]
fn constants() {
    assert_eq!(MAX_RULES, 64);
}

#[test]
fn a_rules_section_before_where_parses() {
    ok(&format!(
        "[:find ?c :rules [{GP}] :where (grandparent :edwin ?c)]"
    ));
}

#[test]
fn rules_sit_after_in_at_and_canon() {
    ok(&format!(
        "[:find ?c :in $ ?x :at 5 :canon 3 :rules [{GP}] :where (grandparent ?x ?c)]"
    ));
}

#[test]
fn several_definitions_and_several_bodies_parse() {
    ok(
        "[:find ?a :rules [[(f ?a ?b) [?a :ally_of ?b]] [(f ?a ?b) [?b :ally_of ?a]] \
        [(g ?a) (f ?a ?b) (at -200 [?a :located_in ?p]) [(!= ?a ?b)]]] \
        :where (g ?a)]",
    );
}

#[test]
fn rules_section_after_where_is_syntax() {
    syntax(&format!(
        "[:find ?c :where (grandparent :edwin ?c) :rules [{GP}]]"
    ));
}

#[test]
fn rules_before_at_or_canon_is_syntax() {
    syntax(&format!(
        "[:find ?c :rules [{GP}] :at 5 :where (grandparent :edwin ?c)]"
    ));
    syntax(&format!(
        "[:find ?c :rules [{GP}] :canon 3 :where (grandparent :edwin ?c)]"
    ));
    syntax(&format!(
        "[:find ?c :rules [{GP}] :in $ ?x :where (grandparent ?x ?c)]"
    ));
}

#[test]
fn rules_before_find_or_twice_is_syntax() {
    syntax(&format!(
        "[:rules [{GP}] :find ?c :where (grandparent :edwin ?c)]"
    ));
    syntax(&format!(
        "[:find ?c :rules [{GP}] :rules [{GP}] :where (grandparent :edwin ?c)]"
    ));
}

#[test]
fn rules_content_must_be_exactly_one_vector_of_definitions() {
    syntax("[:find ?c :rules :where [?c :name ?n]]");
    syntax(&format!("[:find ?c :rules {GP} :where [?c :name ?n]]")); // a bare definition
    syntax(&format!("[:find ?c :rules ({GP}) :where [?c :name ?n]]")); // a list
    syntax(&format!(
        "[:find ?c :rules [{GP}] [{GP}] :where [?c :name ?n]]"
    )); // two vectors
    syntax("[:find ?c :rules [(r ?a) [?a :name ?n]] :where [?c :name ?n]]"); // def not wrapped
    syntax("[:find ?c :rules [((r ?a) [?a :name ?n])] :where [?c :name ?n]]"); // def is a list
}

#[test]
fn a_definition_needs_a_body() {
    syntax("[:find ?c :rules [[(r ?a)]] :where [?c :name ?n]]");
}

#[test]
fn reserved_names_cannot_be_rule_names() {
    let names = [
        "at",
        "not",
        "or",
        "and",
        "during",
        "intersect",
        "union",
        "difference",
        "not-empty",
        "contains",
        "count",
        "count-distinct",
        "min",
        "max",
        "sum",
        "=",
        "!=",
        "<",
        "<=",
        ">",
        ">=",
    ];
    for n in names {
        let t = format!("[:find ?c :rules [[({n} ?a) [?a :name ?x]]] :where [?c :name ?x]]");
        match parse(&t) {
            Err(e) => {
                assert_eq!(e.kind, ErrorKind::Syntax, "{n}: {e}");
                let sp = e.span.unwrap_or_else(|| panic!("{n}: no span"));
                assert!(t[sp.start..sp.end].contains(n), "{n}: span {sp:?}");
            }
            Ok(_) => panic!("reserved name `{n}` was accepted"),
        }
    }
}

#[test]
fn rule_names_must_be_bare_symbols() {
    syntax("[:find ?c :rules [[(?r ?a) [?a :name ?x]]] :where [?c :name ?x]]");
    syntax("[:find ?c :rules [[(:r ?a) [?a :name ?x]]] :where [?c :name ?x]]");
    syntax("[:find ?c :rules [[(\"r\" ?a) [?a :name ?x]]] :where [?c :name ?x]]");
    syntax("[:find ?c :rules [[(5 ?a) [?a :name ?x]]] :where [?c :name ?x]]");
}

#[test]
fn head_needs_at_least_one_variable() {
    syntax("[:find ?c :rules [[(r) [?c :name ?x]]] :where [?c :name ?x]]");
}

#[test]
fn head_variables_must_be_distinct() {
    syntax("[:find ?c :rules [[(r ?a ?a) [?a :name ?x]]] :where [?c :name ?x]]");
}

#[test]
fn head_must_only_hold_variables() {
    syntax("[:find ?c :rules [[(r ?a 1) [?a :name ?x]]] :where [?c :name ?x]]");
    syntax("[:find ?c :rules [[(r ?a :edwin) [?a :name ?x]]] :where [?c :name ?x]]");
    syntax("[:find ?c :rules [[(r ?a _) [?a :name ?x]]] :where [?c :name ?x]]");
    syntax("[:find ?c :rules [[(r ?a \"s\") [?a :name ?x]]] :where [?c :name ?x]]");
}

#[test]
fn rule_calls_parse_in_where_and_body_with_every_arg_kind() {
    ok(
        "[:find ?c :in $ ?p :rules [[(r ?a ?b) [?a :ally_of ?b]] [(s ?a) (r ?a _) (r :edwin ?a)]] \
        :where (r ?c ?p) (r \"x\" 3) (r true -2.5) (r varrow#1 _) (s ?c)]",
    );
}

// ------------------------------------------------------------------ limits

fn many_defs(n: usize) -> String {
    let defs: String = (0..n)
        .map(|i| format!("[(r{i} ?a) [?a :name ?x]] "))
        .collect();
    format!("[:find ?c :rules [{defs}] :where [?c :name ?x]]")
}

#[test]
fn max_rules_definitions_are_accepted_and_one_more_is_a_limit_error() {
    let at = many_defs(MAX_RULES);
    parse(&at).unwrap_or_else(|e| panic!("{e}"));
    prepare(&world(), &at).unwrap_or_else(|e| panic!("{e}"));
    let over = many_defs(MAX_RULES + 1);
    assert_eq!(parse(&over).expect_err("65 defs").kind, ErrorKind::Limit);
    assert_eq!(
        prepare(&world(), &over).expect_err("65 defs").kind,
        ErrorKind::Limit
    );
}

#[test]
fn max_rules_counts_definitions_not_names() {
    // 65 bodies of ONE name is still 65 definitions.
    let defs: String = (0..=MAX_RULES).map(|_| "[(r ?a) [?a :name ?x]] ").collect();
    let t = format!("[:find ?c :rules [{defs}] :where [?c :name ?x]]");
    assert_eq!(parse(&t).expect_err("65 defs").kind, ErrorKind::Limit);
}

#[test]
fn rule_body_patterns_count_toward_max_patterns() {
    let body: String = (0..MAX_PATTERNS)
        .map(|i| format!("[?a :ally_of ?b{i}] "))
        .collect();
    // MAX_PATTERNS - 1 in the rule + 1 in :where = 256 in total: fine
    let body_ok: String = (0..MAX_PATTERNS - 1)
        .map(|i| format!("[?a :ally_of ?b{i}] "))
        .collect();
    let ok_q = format!("[:find ?c :rules [[(r ?a) {body_ok}]] :where [?c :name ?x]]");
    parse(&ok_q).unwrap_or_else(|e| panic!("{e}"));
    // 256 in the rule + 1 in :where = 257 tips it over
    let bad = format!("[:find ?c :rules [[(r ?a) {body}]] :where [?c :name ?x]]");
    assert_eq!(parse(&bad).expect_err("257").kind, ErrorKind::Limit);
    assert_eq!(
        prepare(&world(), &bad).expect_err("257").kind,
        ErrorKind::Limit
    );
}

#[test]
fn rule_body_clauses_count_toward_max_clauses() {
    let preds = |n: usize| "[(= 1 1)] ".repeat(n);
    // 300 in the rule + 300 in :where + 1 pattern each > 512, neither section alone is
    let bad = format!(
        "[:find ?c :rules [[(r ?a) [?a :name ?x] {}]] :where [?c :name ?x] {}]",
        preds(300),
        preds(300)
    );
    assert_eq!(parse(&bad).expect_err("602").kind, ErrorKind::Limit);
    const _: () = assert!(MAX_CLAUSES < 602);
    let fine = format!(
        "[:find ?c :rules [[(r ?a) [?a :name ?x] {}]] :where [?c :name ?x] {}]",
        preds(200),
        preds(200)
    );
    parse(&fine).unwrap_or_else(|e| panic!("{e}"));
}
