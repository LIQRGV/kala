//! Slice 7, item 7 (RED): `kala query` with aggregates in `:find`. An
//! aggregate column is named by its normalized text: the text header prints
//! `(count ?c)` (no extra `?` in front) and JSON `columns` holds
//! `"(count ?c)"`. Plain variables keep `?p`. Counts are `i64` values.
//!
//! Expected answers are hand-derived from `varrow.yaml` (the located_in lines):
//! at -200 varrow holds 5 characters (osric, tomas, v01, v15, mara), carnath 3,
//! redcliff 3, ashford/dunmere/eldoria/highkeep/oakhollow/saltmarsh/stonebridge
//! 2 each, graymoor/mistral/thornwick/whitefall 1 each: 29 placements in all.

mod common;

use common::*;
use serde_json::json;

const Q: &str = "[:find ?p (count ?c) :at -200 :where [?c :located_in ?p]]";

fn lines(out: &str) -> Vec<String> {
    out.lines().map(str::to_string).collect()
}

#[test]
fn text_header_prints_the_aggregate_column_by_its_text() {
    let fx = varrow();
    let out = fx.text("query", &[Q]);
    let l = lines(&out);
    assert!(l[0].ends_with(": 14 rows"), "{}", l[0]);
    assert_eq!(l[1], "  ?p  (count ?c)");
    let mut rows: Vec<&str> = l[2..].iter().map(String::as_str).collect();
    rows.sort();
    assert_eq!(rows.len(), 14);
    for want in [
        "  varrow  5",
        "  carnath  3",
        "  redcliff  3",
        "  mistral  1",
    ] {
        assert!(rows.contains(&want), "missing {want:?} in {rows:?}");
    }
}

#[test]
fn text_header_for_every_aggregate_and_for_an_aggregate_first() {
    let fx = varrow();
    let out = fx.text(
        "query",
        &["[:find (count ?c) (count-distinct ?p) :at -200 :where [?c :located_in ?p]]"],
    );
    let l = lines(&out);
    assert_eq!(l[1], "  (count ?c)  (count-distinct ?p)");
    // 29 placements in 14 different places
    assert_eq!(l[2], "  29  14");
    let out = fx.text(
        "query",
        &["[:find (min ?t) (max ?t) (sum ?t) :where [:greg :located_in ?p ?t]]"],
    );
    let l = lines(&out);
    assert_eq!(l[1], "  (min ?t)  (max ?t)  (sum ?t)");
    // greg's change points: -100 150 200 250 601
    assert_eq!(l[2], "  -100  601  1101");
    let out = fx.text(
        "query",
        &[Q.replace("?p (count ?c)", "(count ?c) ?p").as_str()],
    );
    assert_eq!(lines(&out)[1], "  (count ?c)  ?p");
}

#[test]
fn json_columns_and_rows_for_an_aggregate() {
    let fx = varrow();
    let j = fx.json(
        "query",
        &["[:find (count ?c) :at -200 :where [?c :located_in ?p]]"],
    );
    assert_eq!(j["command"], json!("query"));
    assert_eq!(j["columns"], json!(["(count ?c)"]));
    assert_eq!(j["rows"], json!([[{"type": "i64", "value": 29}]]));
    let j = fx.json("query", &[Q]);
    assert_eq!(j["columns"], json!(["?p", "(count ?c)"]));
    let rows = j["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 14);
    let varrow = rows
        .iter()
        .find(|r| r[0]["value"]["handle"] == json!("varrow"))
        .expect("a varrow row");
    assert_eq!(varrow[1], json!({"type": "i64", "value": 5}));
}

#[test]
fn json_columns_are_normalized_aggregate_text() {
    let fx = varrow();
    let j = fx.json(
        "query",
        &["[:find ( count-distinct   ?c ) :at -200 :where [?c :located_in ?p]]"],
    );
    assert_eq!(j["columns"], json!(["(count-distinct ?c)"]));
    assert_eq!(j["rows"], json!([[{"type": "i64", "value": 29}]]));
}

#[test]
fn empty_input_prints_no_rows() {
    let fx = varrow();
    let out = fx.text(
        "query",
        &["[:find (count ?c) :at -1000 :where [?c :located_in ?p]]"],
    );
    let l = lines(&out);
    assert!(l[0].ends_with(": 0 rows"), "{}", l[0]);
    assert_eq!(l[1], "  (count ?c)");
    assert_eq!(l[2], "  (no rows)");
}

#[test]
fn explain_shows_the_aggregate_line() {
    let fx = varrow();
    let out = fx.text("explain", &[Q]);
    assert!(
        out.lines()
            .any(|l| l.trim_start().starts_with("Aggregate ?p: count ?c -> ")),
        "{out}"
    );
}
