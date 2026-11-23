//! Slice 7, item 00 (e): `[?p :kala_handle :varrow]` (a handle keyword where the
//! handle *string* belongs) is a TypeMismatch whose message tells the author
//! what to write instead, using the handle from the query.

use kala_core::{World, load_fixture};
use kala_datalog::{ErrorKind, prepare};

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

fn world() -> World {
    load_fixture(VARROW).expect("fixture loads")
}

#[test]
fn handle_keyword_in_value_position_hints_both_spellings() {
    let w = world();
    for h in ["varrow", "greg", "edwin"] {
        let q = format!("[:find ?p :where [?p :kala_handle :{h}]]");
        let e = prepare(&w, &q).expect_err("must be rejected");
        assert_eq!(e.kind, ErrorKind::TypeMismatch, "{e}");
        assert!(
            e.message.contains("[?c :located_in :varrow]"),
            "hint for the entity form missing: {}",
            e.message
        );
        assert!(
            e.message.contains(&format!("[?p :kala_handle \"{h}\"]")),
            "hint for the string form with the actual handle missing: {}",
            e.message
        );
    }
}

#[test]
fn the_string_form_still_works() {
    let w = world();
    let p = prepare(&w, r#"[:find ?p :where [?p :kala_handle "varrow"]]"#).expect("compiles");
    let rs = kala_query::run(&w, &p.plan, &kala_query::Params::new(), &Default::default()).unwrap();
    assert_eq!(rs.rows.len(), 1);
}

#[test]
fn other_type_mismatches_do_not_get_the_handle_hint() {
    let w = world();
    let e = prepare(&w, r#"[:find ?p :where [?p :located_in "varrow"]]"#)
        .expect_err("string where a ref belongs");
    assert_eq!(e.kind, ErrorKind::TypeMismatch);
    assert!(!e.message.contains("[?p :kala_handle"), "{}", e.message);
}
