//! Parser tests (RED step): `kala_datalog::parse` and the `ast` module.
mod common;
use common::*;
use kala_datalog::ast::*;
use kala_datalog::{DatalogError, ErrorKind, MAX_DEPTH, MAX_PATTERNS, MAX_QUERY_BYTES, parse};

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
    e
}

fn slice(text: &str, s: kala_datalog::Span) -> &str {
    &text[s.start..s.end]
}

/// Parse a single value-position item through a 3-element pattern.
fn val(text: &str) -> Item {
    let qt = q(&format!("[?e :x {text}]"));
    match &ok(&qt).clauses[0] {
        Clause::Pattern(p) => p.v.clone(),
        c => panic!("not a pattern: {c:?}"),
    }
}

const HEADLINE: &str = r#"[:find ?name
 :in $ ?who                       ; parameters
 :at -210                         ; query default tick (optional; else latest tick)
 :canon 41                        ; read canon tx <= 41 (optional; else latest)
 :where
 [?e :handle ?who]
 [?e :ally_of ?a]                 ; read at the query default tick
 (at -200 [?a :located_in ?p])    ; this pattern reads at its own tick
 [?a :name ?name]
 [(!= ?a ?e)]]                    ; predicate
"#;

#[test]
fn headline_query_structure() {
    let q = ok(HEADLINE);
    assert_eq!(
        shape(&q),
        "find(?name) in($ ?who) at(-210) canon(41) where{ [?e :handle ?who] [?e :ally_of ?a] \
         (at -200 [?a :located_in ?p]) [?a :name ?name] [(!= ?a ?e)] }"
    );
}

#[test]
fn headline_query_spans() {
    let t = HEADLINE;
    let q = ok(t);
    assert_eq!(slice(t, q.span), &t[..t.rfind(']').unwrap() + 1]);
    assert_eq!(slice(t, q.find[0].span), "?name");
    assert_eq!(q.find[0].span.start, t.find("?name").unwrap());
    assert!(matches!(q.inputs[0], Input::Db(s) if slice(t, s) == "$"));
    match &q.inputs[1] {
        Input::Var(s) => assert_eq!(slice(t, s.span), "?who"),
        i => panic!("{i:?}"),
    }
    match q.at.as_ref().unwrap() {
        TickExpr::Int(-210, s) => assert_eq!(slice(t, *s), "-210"),
        x => panic!("{x:?}"),
    }
    match q.canon.as_ref().unwrap() {
        CanonExpr::Int(41, s) => assert_eq!(slice(t, *s), "41"),
        x => panic!("{x:?}"),
    }
    match &q.clauses[1] {
        Clause::Pattern(p) => {
            assert_eq!(slice(t, p.span), "[?e :ally_of ?a]");
            assert_eq!(slice(t, p.a.span), ":ally_of");
            assert_eq!(p.a.name, "ally_of");
        }
        c => panic!("{c:?}"),
    }
    match &q.clauses[2] {
        Clause::At {
            span,
            patterns,
            tick,
            ..
        } => {
            assert_eq!(slice(t, *span), "(at -200 [?a :located_in ?p])");
            assert!(matches!(tick, TickExpr::Int(-200, s) if slice(t, *s) == "-200"));
            assert_eq!(slice(t, patterns[0].span), "[?a :located_in ?p]");
        }
        c => panic!("{c:?}"),
    }
    match &q.clauses[4] {
        Clause::Pred(p) => assert_eq!(slice(t, p.span), "[(!= ?a ?e)]"),
        c => panic!("{c:?}"),
    }
}

// ---- tokens ----

#[test]
fn item_kinds() {
    assert!(matches!(val(":edwin"), Item::Keyword(k, _) if k == "edwin"));
    assert!(matches!(val("?v"), Item::Var(s) if s.name == "v"));
    assert!(matches!(val("_"), Item::Any(_)));
    assert!(matches!(val("true"), Item::Bool(true, _)));
    assert!(matches!(val("false"), Item::Bool(false, _)));
    assert!(matches!(val("CHARACTER#1"), Item::Id(s, _) if s == "CHARACTER#1"));
    assert!(matches!(val("edge#9"), Item::Id(s, _) if s == "edge#9"));
    assert!(matches!(val("\"hi\""), Item::Str(s, _) if s == "hi"));
}

#[test]
fn item_spans_cover_whole_token() {
    let t = q("[?e :x CHARACTER#1]");
    let qq = ok(&t);
    let Clause::Pattern(p) = &qq.clauses[0] else {
        panic!()
    };
    let Item::Id(_, s) = &p.v else { panic!() };
    assert_eq!(slice(&t, *s), "CHARACTER#1");
    let Item::Var(v) = &p.e else { panic!() };
    assert_eq!(slice(&t, v.span), "?e");
}

#[test]
fn integers() {
    for (txt, n) in [
        ("0", 0i64),
        ("7", 7),
        ("-5", -5),
        ("+5", 5),
        ("9223372036854775807", i64::MAX),
        ("-9223372036854775808", i64::MIN),
    ] {
        assert!(matches!(val(txt), Item::Int(x, _) if x == n), "{txt}");
    }
}

#[test]
fn integer_out_of_range_is_syntax_error_with_span() {
    for txt in [
        "9223372036854775808",
        "-9223372036854775809",
        "99999999999999999999999",
    ] {
        let t = q(&format!("[?e :x {txt}]"));
        let e = err_kind(&t, ErrorKind::Syntax);
        let s = e.span.expect("span");
        assert_eq!(slice(&t, s), txt);
    }
}

#[test]
fn floats() {
    assert!(matches!(val("1.5"), Item::Float(f, _) if f == 1.5));
    assert!(matches!(val("-2.0"), Item::Float(f, _) if f == -2.0));
    assert!(matches!(val("1e3"), Item::Float(f, _) if f == 1000.0));
    assert!(matches!(val("2.5E-1"), Item::Float(f, _) if f == 0.25));
}

#[test]
fn float_overflow_is_syntax_error() {
    // Decision: a float that overflows to infinity is refused (NaN/inf never valid values).
    let t = q("[?e :x 1e999]");
    let e = err_kind(&t, ErrorKind::Syntax);
    assert_eq!(slice(&t, e.span.unwrap()), "1e999");
}

#[test]
fn string_escapes() {
    let v = val(r#""a\"b\\c\nd\te\rf\u00e9\u0041""#);
    assert!(matches!(v, Item::Str(s, _) if s == "a\"b\\c\nd\te\rf\u{e9}A"));
}

#[test]
fn string_utf8_passthrough_and_raw_chars() {
    assert!(matches!(val("\"héllo ✓\""), Item::Str(s, _) if s == "héllo ✓"));
    assert!(matches!(val("\"\""), Item::Str(s, _) if s.is_empty()));
}

#[test]
fn string_errors_have_spans() {
    for bad in [
        r#""bad \x escape""#,
        r#""\uD800""#,    // lone high surrogate
        r#""\uDC00""#,    // lone low surrogate
        r#""\u12""#,      // too few hex digits
        r#""\u{1F600}""#, // braces form not supported
        r#""unterminated"#,
        r#""ends with backslash\"#,
    ] {
        let t = q(&format!("[?e :x {bad}]"));
        let t = if bad.starts_with("\"unterminated") || bad.ends_with('\\') {
            // unterminated: the closing brackets become part of / after the string
            q(&format!("[?e :x {bad}"))
        } else {
            t
        };
        let e = err_kind(&t, ErrorKind::Syntax);
        let s = e.span.unwrap_or_else(|| panic!("no span for {bad}"));
        assert!(s.start <= s.end && s.end <= t.len(), "{bad}: {s:?}");
        assert!(t.is_char_boundary(s.start) && t.is_char_boundary(s.end));
        assert!(
            s.start >= t.find('"').unwrap(),
            "{bad}: span before the string"
        );
    }
}

#[test]
fn unterminated_string_span_starts_at_opening_quote() {
    let t = q("[?e :x \"oops");
    let e = err_kind(&t, ErrorKind::Syntax);
    assert_eq!(e.span.unwrap().start, t.find('"').unwrap());
}

#[test]
fn bare_unknown_symbol_is_syntax_error() {
    err_kind(&q("[?e :x foo]"), ErrorKind::Syntax);
    err_kind(&q("[?e :x CHARACTER#]"), ErrorKind::Syntax);
    err_kind(&q("[?e :x #1]"), ErrorKind::Syntax);
}

#[test]
fn commas_are_whitespace() {
    let a = ok("[:find ?a, ?b, :where, [?a, :x, ?b],]");
    let b = ok("[:find ?a ?b :where [?a :x ?b]]");
    assert_eq!(shape(&a), shape(&b));
}

#[test]
fn comments_and_crlf() {
    let t = "; leading comment\r\n[:find ?a ; trailing\r\n :where\r\n [?a :x 1] ; c\r\n]\r\n; end";
    let q = ok(t);
    assert_eq!(shape(&q), "find(?a) in() at() canon() where{ [?a :x 1] }");
    // comment at EOF without newline
    ok("[:find ?a :where [?a :x 1]] ; no newline");
}

#[test]
fn trailing_garbage_and_empty_input() {
    err_kind("", ErrorKind::Syntax);
    err_kind("   ; only a comment", ErrorKind::Syntax);
    err_kind(
        "[:find ?a :where [?a :x 1]] [:find ?b :where [?b :x 1]]",
        ErrorKind::Syntax,
    );
    err_kind("[:find ?a :where [?a :x 1]] )", ErrorKind::Syntax);
    err_kind("[:find ?a :where [?a :x 1]", ErrorKind::Syntax); // unclosed
    err_kind("[:find ?a :where [?a :x 1]))", ErrorKind::Syntax);
    err_kind("(:find ?a :where [?a :x 1])", ErrorKind::Syntax); // outer must be a vector
}

// ---- sections ----

#[test]
fn minimal_query() {
    let q = ok("[:find ?a ?b :where [?a :knows ?b]]");
    assert_eq!(
        shape(&q),
        "find(?a ?b) in() at() canon() where{ [?a :knows ?b] }"
    );
}

#[test]
fn in_with_and_without_db() {
    assert_eq!(
        shape(&ok("[:find ?a :in $ ?p ?q :where [?a :x ?p]]")),
        "find(?a) in($ ?p ?q) at() canon() where{ [?a :x ?p] }"
    );
    assert_eq!(
        shape(&ok("[:find ?a :in ?p :where [?a :x ?p]]")),
        "find(?a) in(?p) at() canon() where{ [?a :x ?p] }"
    );
    assert_eq!(
        shape(&ok("[:find ?a :in $ :where [?a :x 1]]")),
        "find(?a) in($) at() canon() where{ [?a :x 1] }"
    );
}

#[test]
fn at_and_canon_forms() {
    assert_eq!(
        shape(&ok("[:find ?a :at -210 :where [?a :x 1]]")),
        "find(?a) in() at(-210) canon() where{ [?a :x 1] }"
    );
    assert_eq!(
        shape(&ok("[:find ?a :in ?t :at ?t :where [?a :x 1]]")),
        "find(?a) in(?t) at(?t) canon() where{ [?a :x 1] }"
    );
    assert_eq!(
        shape(&ok("[:find ?a :canon 41 :where [?a :x 1]]")),
        "find(?a) in() at() canon(41) where{ [?a :x 1] }"
    );
    assert_eq!(
        shape(&ok("[:find ?a :in ?c :canon ?c :where [?a :x 1]]")),
        "find(?a) in(?c) at() canon(?c) where{ [?a :x 1] }"
    );
    assert_eq!(
        shape(&ok("[:find ?a :canon 0 :where [?a :x 1]]")),
        "find(?a) in() at() canon(0) where{ [?a :x 1] }"
    );
    assert_eq!(
        shape(&ok("[:find ?a :at 9223372036854775807 :where [?a :x 1]]")),
        "find(?a) in() at(9223372036854775807) canon() where{ [?a :x 1] }"
    );
    assert_eq!(
        shape(&ok(
            "[:find ?a :canon 18446744073709551615 :where [?a :x 1]]"
        )),
        "find(?a) in() at() canon(18446744073709551615) where{ [?a :x 1] }"
    );
}

#[test]
fn negative_canon_refused() {
    let t = "[:find ?a :canon -1 :where [?a :x 1]]";
    let e = err_kind(t, ErrorKind::Syntax);
    assert_eq!(slice(t, e.span.unwrap()), "-1");
}

#[test]
fn canon_out_of_u64_range_refused() {
    err_kind(
        "[:find ?a :canon 18446744073709551616 :where [?a :x 1]]",
        ErrorKind::Syntax,
    );
}

#[test]
fn section_order_enforced() {
    err_kind("[:find ?a :where [?a :x 1] :at 1]", ErrorKind::Syntax);
    err_kind("[:find ?a :at 1 :in $ :where [?a :x 1]]", ErrorKind::Syntax);
    err_kind(
        "[:find ?a :canon 1 :at 1 :where [?a :x 1]]",
        ErrorKind::Syntax,
    );
    err_kind("[:in $ :find ?a :where [?a :x 1]]", ErrorKind::Syntax);
    err_kind("[:where [?a :x 1] :find ?a]", ErrorKind::Syntax);
}

#[test]
fn duplicate_sections_refused() {
    err_kind("[:find ?a :find ?b :where [?a :x 1]]", ErrorKind::Syntax);
    err_kind(
        "[:find ?a :in $ :in ?p :where [?a :x 1]]",
        ErrorKind::Syntax,
    );
    err_kind("[:find ?a :at 1 :at 2 :where [?a :x 1]]", ErrorKind::Syntax);
    err_kind(
        "[:find ?a :canon 1 :canon 2 :where [?a :x 1]]",
        ErrorKind::Syntax,
    );
    err_kind(
        "[:find ?a :where [?a :x 1] :where [?a :x 2]]",
        ErrorKind::Syntax,
    );
}

#[test]
fn missing_or_empty_sections_refused() {
    err_kind("[:where [?a :x 1]]", ErrorKind::Syntax);
    err_kind("[:find ?a]", ErrorKind::Syntax);
    err_kind("[:find ?a :where]", ErrorKind::Syntax);
    err_kind("[:find :where [?a :x 1]]", ErrorKind::Syntax); // empty :find
    err_kind("[]", ErrorKind::Syntax);
    err_kind("[:find ?a :at :where [?a :x 1]]", ErrorKind::Syntax); // :at with no value
    err_kind("[:find ?a :canon :where [?a :x 1]]", ErrorKind::Syntax);
}

#[test]
fn unknown_section_refused() {
    err_kind("[:find ?a :limit 3 :where [?a :x 1]]", ErrorKind::Syntax);
}

// `rules_section_unsupported` was deleted: `:rules` is supported now, and
// rules_parse.rs covers `:rules` after `:where` (Syntax), section order and shapes.
#[test]
fn rules_section_parses_into_the_ast() {
    let qq = ok("[:find ?a :rules [[(r ?a) [?a :x 1]]] :where (r ?a)]");
    assert_eq!(qq.rules.len(), 1);
    assert_eq!(qq.rules[0].name, "r");
    assert_eq!(qq.calls.len(), 1);
}

#[test]
fn bad_find_items() {
    err_kind("[:find a :where [?a :x 1]]", ErrorKind::Syntax);
    err_kind("[:find 1 :where [?a :x 1]]", ErrorKind::Syntax);
    err_kind("[:find _ :where [?a :x 1]]", ErrorKind::Syntax);
    err_kind("[:find ? :where [?a :x 1]]", ErrorKind::Syntax);
}

#[test]
fn aggregates_in_find_now_parse() {
    // Slice 7 item 7: aggregates are supported. Their meaning, malformed
    // forms (Syntax) and type errors are covered by tests/aggregates.rs; here
    // only the parser contract: they are accepted, never "Unsupported".
    for agg in [
        "(count ?c)",
        "(count-distinct ?c)",
        "(sum ?c)",
        "(max ?c)",
        "(min ?c)",
    ] {
        ok(&format!("[:find ?p {agg} :where [?c :x ?p]]"));
    }
}

#[test]
fn bad_in_items() {
    err_kind("[:find ?a :in x :where [?a :x 1]]", ErrorKind::Syntax);
    err_kind("[:find ?a :in 5 :where [?a :x 1]]", ErrorKind::Syntax);
}

// ---- clauses ----

#[test]
fn at_clause_forms() {
    assert_eq!(
        shape(&ok("[:find ?a :where (at -200 [?a :x ?p] [?a :y ?q])]")),
        "find(?a) in() at() canon() where{ (at -200 [?a :x ?p] [?a :y ?q]) }"
    );
    assert_eq!(
        shape(&ok("[:find ?a :in ?t :where (at ?t [?a :x ?p])]")),
        "find(?a) in(?t) at() canon() where{ (at ?t [?a :x ?p]) }"
    );
    // variable bound earlier in the query (binding is checked by the compiler, not the parser)
    assert_eq!(
        shape(&ok("[:find ?a :where [?a :born ?t] (at ?t [?a :x ?p])]")),
        "find(?a) in() at() canon() where{ [?a :born ?t] (at ?t [?a :x ?p]) }"
    );
}

#[test]
fn at_clause_errors() {
    err_kind("[:find ?a :where (at -200)]", ErrorKind::Syntax); // no patterns
    err_kind("[:find ?a :where (at [?a :x 1])]", ErrorKind::Syntax); // no tick
    err_kind("[:find ?a :where (at \"x\" [?a :x 1])]", ErrorKind::Syntax);
    err_kind("[:find ?a :where (at 1.5 [?a :x 1])]", ErrorKind::Syntax);
    err_kind("[:find ?a :where (at 1 5)]", ErrorKind::Syntax);
}

#[test]
fn nested_at_refused() {
    err_kind(
        "[:find ?a :where (at 1 (at 2 [?a :x 1]))]",
        ErrorKind::Syntax,
    );
}

#[test]
fn predicates_with_each_op() {
    for (sym, name) in [
        ("=", "="),
        ("!=", "!="),
        ("<", "<"),
        ("<=", "<="),
        (">", ">"),
        (">=", ">="),
    ] {
        let t = q(&format!("[?e :x ?a] [?e :y ?b] [({sym} ?a ?b)]"));
        let qq = ok(&t);
        let Clause::Pred(p) = &qq.clauses[2] else {
            panic!("not a pred")
        };
        assert_eq!(p.op.symbol(), name);
        assert_eq!(slice(&t, p.span), format!("[({sym} ?a ?b)]"));
    }
}

#[test]
fn predicate_operand_kinds() {
    assert_eq!(
        shape(&ok(
            "[:find ?a :where [?a :age ?n] [(> ?n 18)] [(!= ?a :edwin)] [(<= 1.5 ?n)] [(= ?a CHARACTER#1)] [(= ?n \"x\")]]"
        )),
        "find(?a) in() at() canon() where{ [?a :age ?n] [(> ?n 18)] [(!= ?a :edwin)] [(<= 1.5f ?n)] [(= ?a #CHARACTER#1)] [(= ?n \"x\")] }"
    );
}

#[test]
fn predicate_errors() {
    err_kind(&q("[(~ ?a ?b)]"), ErrorKind::Syntax); // unknown op
    err_kind(&q("[(like ?a ?b)]"), ErrorKind::Syntax);
    err_kind(&q("[(= ?a)]"), ErrorKind::Syntax); // too few operands
    err_kind(&q("[(= ?a ?b ?c)]"), ErrorKind::Syntax); // too many
    err_kind(&q("[()]"), ErrorKind::Syntax);
    err_kind(&q("[(= ?a ?b) ?c]"), ErrorKind::Syntax);
}

#[test]
fn wrong_arity_patterns() {
    // 4, 5 and 6 elements are history patterns, with optional op and precision.
    ok(&q("[?e :a ?v ?t]"));
    ok(&q("[?e :a ?v ?t ?x]"));
    // an integer in op position has no meaning
    err_kind(&q("[?e :a ?v 1 2]"), ErrorKind::Syntax);
    err_kind(&q("[?e :a]"), ErrorKind::Syntax);
    err_kind(&q("[?e]"), ErrorKind::Syntax);
    err_kind(&q("[]"), ErrorKind::Syntax);
    // Six elements expose precision; seven still have no meaning.
    ok(&q("[?e :a ?v ?t ?x ?y]"));
    err_kind(&q("[?e :a ?v ?t ?x ?y ?z]"), ErrorKind::Syntax);
}

#[test]
fn four_element_pattern_inside_at_is_a_syntax_error() {
    err_kind("[:find ?a :where (at 1 [?a :x ?v ?t])]", ErrorKind::Syntax);
}

#[test]
fn join_forms_stay_unsupported() {
    for c in [
        "(not-join [?e] [?e :x 1])",
        "(or-join [?e] [?e :x 1] [?e :y 1])",
    ] {
        let e = err_kind(&q(c), ErrorKind::Unsupported);
        assert!(e.message.contains("not supported"), "{c}: {}", e.message);
    }
    // A plain `(name arg..)` is a rule call now: it parses (the compiler reports
    // UnknownRule when no `:rules` definition names it).
    let qq = ok(&q("(ancestor ?a ?d)"));
    assert_eq!(qq.calls.len(), 1);
    assert_eq!(qq.calls[0].name, "ancestor");
}

#[test]
fn pattern_attribute_must_be_keyword() {
    // Variable / literal attribute positions are not in the grammar. Either kind of
    // refusal is acceptable to the spec; we only require an error with a span.
    for bad in ["[?e ?a ?v]", "[?e _ ?v]", "[?e \"x\" ?v]", "[?e 1 ?v]"] {
        let e = err(&q(bad));
        assert!(e.span.is_some(), "{bad}");
    }
}

#[test]
fn clause_must_be_vector_or_list() {
    err_kind(&q("?e"), ErrorKind::Syntax);
    err_kind(&q(":x"), ErrorKind::Syntax);
    err_kind(&q("5"), ErrorKind::Syntax);
}

#[test]
fn handle_pseudo_attribute_is_just_an_attr_name() {
    let t = q("[?e :handle :edwin]");
    let qq = ok(&t);
    let Clause::Pattern(p) = &qq.clauses[0] else {
        panic!()
    };
    assert_eq!(p.a.name, "handle");
    assert!(matches!(&p.v, Item::Keyword(k, _) if k == "edwin"));
}

// ---- errors, Display ----

#[test]
fn display_is_line_column_message() {
    let t = "[:find ?a\n :at 99999999999999999999\n :where [?a :x 1]]";
    let e = err_kind(t, ErrorKind::Syntax);
    assert_eq!(e.to_string(), format!("line 2, column 6: {}", e.message));
}

#[test]
fn display_column_counts_chars_not_bytes() {
    let t = "[:find ?a :where [?a :name \"éé✓\"] [?a :age 99999999999999999999]]";
    let e = err_kind(t, ErrorKind::Syntax);
    let byte_off = t.find("99999").unwrap();
    let char_col = t[..byte_off].chars().count() + 1;
    assert_ne!(char_col, byte_off + 1, "test must contain multibyte chars");
    assert_eq!(e.span.unwrap().start, byte_off);
    assert_eq!(
        e.to_string(),
        format!("line 1, column {char_col}: {}", e.message)
    );
}

#[test]
fn display_line_counting_with_crlf() {
    let t = "[:find ?a\r\n\r\n :where [?a :x 99999999999999999999]]";
    let e = err_kind(t, ErrorKind::Syntax);
    let col = t[t.rfind('\n').unwrap() + 1..t.find("99999").unwrap()]
        .chars()
        .count()
        + 1;
    assert_eq!(
        e.to_string(),
        format!("line 3, column {col}: {}", e.message)
    );
}

#[test]
fn errors_always_carry_a_span_for_syntax_and_unsupported() {
    for t in [
        "",
        "[",
        "[:find ?a :where [?a :x 1]",
        "[:find ?a :where [?a :x 1]] x",
        "[:find ?a :where [?a :x 1 :foo]]",
        "[:find ?a :where (or-join [?a] [?a :x 1] [?a :y 1])]",
        "[:find ?a :at :where [?a :x 1]]",
    ] {
        let e = err(t);
        let s = e.span.unwrap_or_else(|| panic!("no span for {t:?}: {e:?}"));
        assert!(s.start <= s.end && s.end <= t.len(), "{t:?}: {s:?}");
    }
}

#[test]
fn eof_error_span_is_in_bounds() {
    let t = "[:find ?a :where [?a :x";
    let e = err_kind(t, ErrorKind::Syntax);
    let s = e.span.unwrap();
    assert!(s.end <= t.len());
    // Display must not panic on a span at end of input
    let _ = e.to_string();
}

// ---- limits ----

#[test]
fn limit_constants_exported() {
    assert_eq!(MAX_QUERY_BYTES, 64 * 1024);
    assert_eq!(MAX_DEPTH, 32);
    assert_eq!(MAX_PATTERNS, 256);
}

#[test]
fn query_exactly_at_byte_limit_ok_one_over_is_limit() {
    let base = "[:find ?a :where [?a :x 1]]";
    // pad with a trailing comment so the text is valid at any length
    let mut at_limit = format!("{base} ;");
    at_limit.push_str(&"c".repeat(MAX_QUERY_BYTES - at_limit.len()));
    assert_eq!(at_limit.len(), MAX_QUERY_BYTES);
    ok(&at_limit);

    let over = format!("{at_limit}c");
    assert_eq!(over.len(), MAX_QUERY_BYTES + 1);
    let e = err_kind(&over, ErrorKind::Limit);
    // span is optional for a whole-input limit, but if present it must be in bounds
    if let Some(s) = e.span {
        assert!(s.end <= over.len());
    }
}

#[test]
fn over_byte_limit_of_garbage_is_limit_not_syntax() {
    let junk = "x".repeat(MAX_QUERY_BYTES + 1);
    err_kind(&junk, ErrorKind::Limit);
}

#[test]
fn byte_limit_counts_bytes_not_chars() {
    // multibyte chars inside a comment: char count under the limit, byte count over
    let base = "[:find ?a :where [?a :x 1]] ;";
    let n = (MAX_QUERY_BYTES - base.len()) / 3 + 1; // each '✓' is 3 bytes
    let t = format!("{base}{}", "✓".repeat(n));
    assert!(t.len() > MAX_QUERY_BYTES && t.chars().count() < MAX_QUERY_BYTES);
    err_kind(&t, ErrorKind::Limit);
}

fn nested(n: usize) -> String {
    format!("[:find ?a :where {}{}]", "[".repeat(n), "]".repeat(n))
}

#[test]
fn depth_over_limit_is_limit_error() {
    let t = nested(MAX_DEPTH + 1); // total depth MAX_DEPTH + 2
    err_kind(&t, ErrorKind::Limit);
    let t = format!("[{}{}]", "[".repeat(33), "]".repeat(33));
    err_kind(&t, ErrorKind::Limit);
}

#[test]
fn depth_limit_also_applies_to_lists() {
    let t = format!("[:find ?a :where {}{}]", "(".repeat(40), ")".repeat(40));
    err_kind(&t, ErrorKind::Limit);
    let t = format!("[:find ?a :where {}x{}]", "[(".repeat(20), ")]".repeat(20));
    err_kind(&t, ErrorKind::Limit);
}

#[test]
fn many_open_brackets_return_limit_quickly() {
    let t = "[".repeat(100_000);
    let start = std::time::Instant::now();
    // run on a small stack to prove the parser does not recurse unboundedly
    let h = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || parse(&t).map(|_| ()))
        .unwrap();
    let r = h.join().expect("parser overflowed the stack or panicked");
    assert_eq!(r.unwrap_err().kind, ErrorKind::Limit);
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn deep_but_closed_nesting_on_small_stack() {
    let t = format!("{}{}", "[".repeat(30_000), "]".repeat(30_000));
    let h = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || parse(&t).map(|_| ()))
        .unwrap();
    assert_eq!(h.join().unwrap().unwrap_err().kind, ErrorKind::Limit);
}

fn n_patterns(n: usize) -> String {
    let body: Vec<String> = (0..n).map(|_| "[?e :x ?v]".to_string()).collect();
    q(&body.join(" "))
}

#[test]
fn pattern_count_limit() {
    let qq = ok(&n_patterns(MAX_PATTERNS));
    assert_eq!(qq.clauses.len(), MAX_PATTERNS);
    err_kind(&n_patterns(MAX_PATTERNS + 1), ErrorKind::Limit);
}

#[test]
fn pattern_count_includes_patterns_inside_at() {
    // 255 plain + one (at ..) holding 2 patterns = 257 patterns
    let mut body: Vec<String> = (0..MAX_PATTERNS - 1)
        .map(|_| "[?e :x ?v]".to_string())
        .collect();
    body.push("(at 1 [?e :x ?v] [?e :y ?v])".to_string());
    err_kind(&q(&body.join(" ")), ErrorKind::Limit);
    // 255 plain + (at ..) with one pattern = 256: ok
    let mut body: Vec<String> = (0..MAX_PATTERNS - 1)
        .map(|_| "[?e :x ?v]".to_string())
        .collect();
    body.push("(at 1 [?e :x ?v])".to_string());
    ok(&q(&body.join(" ")));
}
