//! Parser edge cases found while confirming item 3.
mod common;
use common::*;
use kala_datalog::ast::*;
use kala_datalog::{DatalogError, ErrorKind, parse};

fn ok(text: &str) -> Query {
    match parse(text) {
        Ok(q) => q,
        Err(e) => panic!("expected Ok for {text:?}, got {e}"),
    }
}

fn err(text: &str) -> DatalogError {
    match parse(text) {
        Ok(q) => panic!("expected error for {text:?}, got {}", shape(&q)),
        Err(e) => e,
    }
}

fn err_kind(text: &str, kind: ErrorKind) -> DatalogError {
    let e = err(text);
    assert_eq!(e.kind, kind, "wrong kind for {text:?}: {e:?}");
    let s = e
        .span
        .unwrap_or_else(|| panic!("no span for {text:?}: {e:?}"));
    assert!(
        s.start <= s.end && s.end <= text.len(),
        "span out of range: {s:?}"
    );
    assert!(text.is_char_boundary(s.start) && text.is_char_boundary(s.end));
    assert!(e.to_string().starts_with("line "), "not located: {e}");
    e
}

#[test]
fn canon_minus_zero_is_zero() {
    let q = ok("[:find ?e :canon -0 :where [?e :x 1]]");
    assert!(
        matches!(q.canon, Some(CanonExpr::Int(0, _))),
        "{:?}",
        q.canon
    );
}

#[test]
fn db_marker_must_come_first_in_in() {
    err_kind("[:find ?e :in ?a $ :where [?e :x ?a]]", ErrorKind::Syntax);
    err_kind(
        "[:find ?e :in ?a $ ?b :where [?e :x ?a]]",
        ErrorKind::Syntax,
    );
}

#[test]
fn empty_in_section_is_syntax() {
    err_kind("[:find ?e :in :where [?e :x 1]]", ErrorKind::Syntax);
}

#[test]
fn predicate_inside_at_is_syntax() {
    err_kind("[:find ?a :where (at 5 [(< ?a ?b)])]", ErrorKind::Syntax);
    err_kind(
        "[:find ?a :where (at 5 [?a :x ?b] [(< ?a ?b)])]",
        ErrorKind::Syntax,
    );
}

#[test]
fn bare_predicate_list_without_vector_is_an_error_with_span() {
    let t = "[:find ?a :where [?a :x ?b] (= ?a ?b)]";
    let e = err(t);
    let s = e.span.expect("span");
    assert!(s.end <= t.len() && t.is_char_boundary(s.start) && t.is_char_boundary(s.end));
    assert!(e.to_string().starts_with("line 1, column "), "{e}");
}

#[test]
fn trailing_dot_and_leading_dot_numbers_are_syntax() {
    err_kind("[:find ?e :where [?e :x 1.]]", ErrorKind::Syntax);
    err_kind("[:find ?e :where [?e :x .5]]", ErrorKind::Syntax);
}

#[test]
fn leading_bom_is_syntax_today() {
    err_kind("\u{FEFF}[:find ?e :where [?e :x 1]]", ErrorKind::Syntax);
}

#[test]
fn raw_control_chars_inside_strings_are_kept() {
    let t = "[:find ?e :where [?e :x \"a\u{7}b\nc\td\u{0}e\"]]";
    let q = ok(t);
    match &q.clauses[0] {
        Clause::Pattern(p) => match &p.v {
            Item::Str(s, _) => assert_eq!(s, "a\u{7}b\nc\td\u{0}e"),
            i => panic!("not a string: {i:?}"),
        },
        c => panic!("{c:?}"),
    }
}

fn clean(msg: &str) {
    for c in msg.chars() {
        assert!(
            !c.is_control()
                && !matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}' | '\u{FEFF}' | '\u{2028}' | '\u{2029}'),
            "raw char {:?} echoed in {msg:?}",
            c
        );
    }
}

#[test]
fn error_echo_of_very_long_token_is_cut() {
    let tok = "a".repeat(5000);
    for t in [
        format!("[:find ?e :where [?e :x {tok}]]"),
        format!("[:find ?e :where [?e :{tok} 1] bogus{tok}]"),
        format!("[:find {tok} :where [?e :x 1]]"),
    ] {
        let e = parse(&t).expect_err("must be an error");
        assert!(
            e.message.chars().count() < 200,
            "message {} chars: {}",
            e.message.len(),
            &e.message[..100]
        );
        assert!(e.to_string().chars().count() < 260, "display too long");
        clean(&e.message);
    }
}

#[test]
fn error_echo_escapes_control_and_bidi_chars() {
    for t in [
        "[:find ?e :where [?e :x foo\u{202E}bar]]",
        "[:find ?e :where [?e :x foo\u{7}bar]]",
        "[:find ?e :where [?e :x \u{202E}]]",
        "[:find ?e\u{7} :where [?e :x 1]]",
        "[:find ?e :where [?e :x 1] \u{202E}]",
        "[:find ?e :where [?e :x \"abc\\q\u{202E}\"]]",
    ] {
        if let Err(e) = parse(t) {
            clean(&e.message);
            clean(&e.to_string());
        }
    }
}

#[test]
fn string_escape_beyond_bmp_braces_is_syntax() {
    let e = err_kind(
        r#"[:find ?e :where [?e :x "\u{1F600}"]]"#,
        ErrorKind::Syntax,
    );
    clean(&e.message);
}
