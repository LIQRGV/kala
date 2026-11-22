//! Slice 7, item 6 (RED): `kala query` prints interval sets. Text uses
//! `IntervalSet`'s Display (`{[-280, -250), [-200, -180)}`); JSON uses
//! `{"type":"intervals","value":[[from, until|null], ..]}` (`null`: no end).
//! The expected sets are hand-derived from `varrow.yaml`.

mod common;

use common::*;
use serde_json::json;

fn rows(out: &str) -> Vec<String> {
    let mut v: Vec<String> = out
        .lines()
        .skip(2) // header and column line
        .map(|r| {
            assert!(r.starts_with("  "), "row not indented by two spaces: {r:?}");
            r[2..].to_string()
        })
        .collect();
    v.sort();
    v
}

#[test]
fn text_prints_the_interval_set_display() {
    let fx = varrow();
    // Edwin is in Varrow [-300, -250) (asserted -300, retracted -250)
    let out = fx.text(
        "query",
        &["[:find ?s :where (during [:edwin :located_in :varrow] ?s)]"],
    );
    assert_eq!(rows(&out), vec!["{[-300, -250)}".to_string()]);
}

#[test]
fn text_prints_an_unbounded_set_with_an_infinity_end() {
    let fx = varrow();
    // Osric: located_in varrow asserted -400, never retracted
    let out = fx.text(
        "query",
        &["[:find ?s :where (during [:osric :located_in :varrow] ?s)]"],
    );
    assert_eq!(rows(&out), vec!["{[-400, ∞)}".to_string()]);
}

#[test]
fn text_prints_an_empty_set() {
    let fx = varrow();
    // Edwin [-300,-250) and Greg [200,250) never meet
    let out = fx.text(
        "query",
        &["[:find ?b :where (during [:edwin :located_in :varrow] ?e) \
           (during [:greg :located_in :varrow] ?g) [(intersect ?e ?g) ?b]]"],
    );
    assert_eq!(rows(&out), vec!["{}".to_string()]);
}

#[test]
fn text_prints_the_shared_time_of_edwin_and_his_varrow_neighbours() {
    let fx = varrow();
    let out = fx.text(
        "query",
        &[
            "[:find ?both :where (during [:edwin :located_in :varrow] ?ie) \
           (during [?c :located_in :varrow] ?ic) [(!= ?c :edwin)] \
           [(intersect ?ie ?ic) ?both] [(not-empty ?both)]]",
        ],
    );
    // osric, tomas, v01, v15 share [-300,-250); hild only [-270,-250); as a set of
    // result rows that is two distinct sets
    assert_eq!(
        rows(&out),
        vec!["{[-270, -250)}".to_string(), "{[-300, -250)}".to_string()]
    );
}

#[test]
fn json_shape_of_an_interval_set() {
    let fx = varrow();
    let j = fx.json(
        "query",
        &["[:find ?s :where (during [:edwin :located_in :varrow] ?s)]"],
    );
    assert_eq!(j["columns"], json!(["?s"]));
    assert_eq!(
        j["rows"],
        json!([[{"type": "intervals", "value": [[-300, -250]]}]])
    );
}

#[test]
fn json_unbounded_end_is_null_and_multiple_intervals_are_in_order() {
    let fx = varrow();
    let j = fx.json(
        "query",
        &["[:find ?s :where (during [:osric :located_in :varrow] ?s)]"],
    );
    assert_eq!(
        j["rows"],
        json!([[{"type": "intervals", "value": [[-400, null]]}]])
    );
    // an empty set is an empty list, not null
    let j = fx.json(
        "query",
        &["[:find ?b :where (during [:edwin :located_in :varrow] ?e) \
           (during [:greg :located_in :varrow] ?g) [(intersect ?e ?g) ?b]]"],
    );
    assert_eq!(j["rows"], json!([[{"type": "intervals", "value": []}]]));
    // two intervals: Edwin's varrow set united with Greg's [200, 250)
    let j = fx.json(
        "query",
        &["[:find ?u :where (during [:edwin :located_in :varrow] ?e) \
           (during [:greg :located_in :varrow] ?g) [(union ?e ?g) ?u]]"],
    );
    assert_eq!(
        j["rows"],
        json!([[{"type": "intervals", "value": [[-300, -250], [200, 250]]}]])
    );
}

#[test]
fn a_bad_interval_query_exits_1_with_nothing_on_stdout() {
    let fx = varrow();
    let err = fx.fails(
        1,
        "query",
        &["[:find ?s :where (during [:edwin :located_in :varrow] ?s) [(not-empty ?nope)]]"],
    );
    assert!(err.starts_with("kala: error: "), "{err}");
}
