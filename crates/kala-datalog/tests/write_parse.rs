//! Write grammar uses the query lexer, limits and error locations.
use kala_core::{Op, Precision, Tick};
use kala_datalog::ast::{WriteOp, WriteValue};
use kala_datalog::{ErrorKind, FormKind, form_kind, parse_write, parse_writes};
use proptest::prelude::*;

#[test]
fn every_operation_and_option_order() {
    for text in [
        "[:create :CHARACTER :kael]",
        "[:assert :greg :located_in :ashford :at 601]",
        "[:retract CHARACTER#1 :located_in PLACE#2 :at -300]",
        "[:assert :greg :name \"Greg the Bold\" :at 610 :precision :year]",
        "[:assert [:edwin :present :carnath] :disguised true :precision :day :at -300]",
        "[:assert edge#9 :disguised false]",
        "[:assert :greg :age -12]",
        "[:assert :greg :height 1.5]",
        "[:assert :greg :related_to [:edwin :present :carnath]]",
        "[:correct 67 :delete]",
        "[:correct entry#67 :replace :at -280]",
        "[:correct 67 :replace :value :ashford :op :retract :precision :century :at 0]",
    ] {
        let tx = parse_write(text).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(tx.ops.len(), 1, "{text}");
        assert_eq!(tx.span.start, 0);
        assert_eq!(tx.span.end, text.len());
    }
}

#[test]
fn parsed_values_preserve_precision_ticks_strings_and_edges() {
    let t = parse_write("[:assert [:edwin :present :carnath] :name \"quote \\\" and slash \\\\ and newline\\n\" :precision :year :at -300]").unwrap();
    match &t.ops[0] {
        WriteOp::Fact {
            op,
            subject,
            at,
            precision,
            value,
            ..
        } => {
            assert_eq!(*op, Op::Assert);
            assert_eq!(*at, Some(Tick(-300)));
            assert_eq!(*precision, Precision::Year);
            assert!(matches!(subject, WriteValue::Edge { .. }));
            assert!(format!("{value:?}").contains("newline\\n"));
        }
        other => panic!("{other:?}"),
    }
    for p in ["exact", "day", "month", "year", "decade", "century"] {
        parse_write(&format!("[:assert :greg :age 1 :precision :{p}]")).unwrap();
    }
}

#[test]
fn transaction_defaults_and_multiple_forms() {
    let source = "; initial comment\n[:at -300 [:create :CHARACTER :kael] [:assert :kael :age 2] [:retract :kael :age 2 :at -250]]\n, [:correct 67 :delete]";
    let txs = parse_writes(source).unwrap();
    assert_eq!(txs.len(), 2);
    assert_eq!(txs[0].at, Some(Tick(-300)));
    assert_eq!(txs[0].ops.len(), 3);
    assert!(txs[1].at.is_none());
    assert!(
        parse_write(source).is_err(),
        "single-form API must reject trailing transactions"
    );
    assert_eq!(
        parse_write("[[:create :CHARACTER :kael] [:assert :kael :age 3]]")
            .unwrap()
            .ops
            .len(),
        2
    );
}

#[test]
fn tick_and_entry_extremes() {
    for t in [i64::MIN, -300, 0, i64::MAX] {
        assert!(parse_write(&format!("[:assert :greg :age 1 :at {t}]")).is_ok());
        assert!(parse_write(&format!("[:at {t} [:assert :greg :age 1]]")).is_ok());
    }
    for id in [
        "0",
        "67",
        "entry#0",
        "entry#67",
        "entry#18446744073709551615",
    ] {
        assert!(
            parse_write(&format!("[:correct {id} :delete]")).is_ok(),
            "{id}"
        );
    }
}

#[test]
fn malformed_operations_have_spans() {
    for text in [
        "[:destroy :greg]",
        "[:create :CHARACTER]",
        "[:assert :greg :name]",
        "[:assert :greg :name \"Greg\" :precision :week]",
        "[:assert :greg :name \"Greg\" :at 0 :at 1]",
        "[:at 0 :at 1 [:assert :greg :age 1]]",
        "[:assert :greg :age 1 :precision :year :precision :exact]",
        "[:assert :greg :age 1 :at 9223372036854775808]",
        "[:correct -1 :delete]",
        "[:correct entry#-1 :delete]",
        "[:correct entry#18446744073709551616 :delete]",
        "[:correct entry# :delete]",
        "[:correct 1 :replace :op :delete]",
        "[:correct 1 :replace :value 2 :value 3]",
        "[:correct 1 :delete :at 2]",
        "[:assert ?who :age 2]",
        "[:assert :greg :age ?age]",
        "[:assert :greg :age _]",
        "[:assert [:edwin :present] :disguised true]",
        "[:create :CHARACTER :kael :at 2]",
        "[]",
    ] {
        let e = parse_write(text).expect_err(text);
        assert!(e.span.is_some(), "missing span: {text}: {e}");
        assert!(e.to_string().contains("line 1"), "{text}: {e}");
    }
}

#[test]
fn dispatch_uses_first_element_and_names_valid_forms() {
    assert_eq!(
        form_kind("; comment\n[:find ?x :where [?x :name _]]").unwrap(),
        FormKind::Query
    );
    for text in [
        "[:create :CHARACTER :kael]",
        "[:assert :greg :age 1]",
        "[:retract :greg :age 1]",
        "[:correct 1 :delete]",
        "[:at 0 [:assert :greg :age 1]]",
        "[[:assert :greg :age 1]]",
    ] {
        assert_eq!(form_kind(text).unwrap(), FormKind::Write, "{text}");
    }
    let e = form_kind("[:banana]").unwrap_err().to_string();
    assert!(e.contains(":find") && e.contains(":assert"), "{e}");
    assert!(parse_write("[:find ?x :where [?x :name _]]").is_err());
}

#[test]
fn parser_limits_input_depth_operations_and_forms() {
    let oversized = " ".repeat(kala_datalog::MAX_QUERY_BYTES + 1);
    assert_eq!(parse_writes(&oversized).unwrap_err().kind, ErrorKind::Limit);
    let deep = format!(
        "{}{}",
        "[".repeat(kala_datalog::MAX_DEPTH + 1),
        "]".repeat(kala_datalog::MAX_DEPTH + 1)
    );
    assert_eq!(parse_write(&deep).unwrap_err().kind, ErrorKind::Limit);
    let op = "[:assert :greg :age 1]";
    assert_eq!(
        parse_write(&format!("[{}]", op.repeat(257)))
            .unwrap_err()
            .kind,
        ErrorKind::Limit
    );
    assert_eq!(
        parse_writes(&op.repeat(257)).unwrap_err().kind,
        ErrorKind::Limit
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn arbitrary_input_never_panics(text in ".{0,2048}") {
        let _ = parse_write(&text);
        let _ = parse_writes(&text);
        let _ = form_kind(&text);
    }
}
