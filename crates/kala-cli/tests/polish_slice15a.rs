//! Slice 15a review fixes, through the real binary.
mod common;
use common::*;
use kala_core::{Tick, Value, World};
use serde_json::Value as Json;
use std::fs;
use std::process::Output;

const EMPTY: &str = r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: located_in, type: ref }
    - { name: alias, type: string, cardinality: many }
    - { name: friend, type: ref, cardinality: many }
    - { name: about, type: ref, cardinality: many }
    - { name: ally_of, type: ref, cardinality: many, direction: bidirectional }
    - { name: name, type: string }
    - { name: age, type: i64 }
    - { name: score, type: f64 }
    - { name: scores, type: f64, cardinality: many }
entities:
  CHARACTER: [greg, edwin, mara]
  PLACE: [varrow, ashford, carnath]
"#;

fn transact(f: &Fx, text: &str, json: bool) -> Output {
    let input = f.dir.path().join("polish15a.edn");
    fs::write(&input, text).unwrap();
    let mut args = vec![input.to_str().unwrap()];
    if json {
        args.push("--json");
    }
    f.run("transact", &args)
}
fn apply(f: &Fx, text: &str) -> Json {
    let o = transact(f, text, true);
    assert_eq!(code(&o), 0, "{text}: {}", stderr(&o));
    parse_one_json(&stdout(&o))
}
/// The whole error line of a refused form; the file must be untouched.
fn refused(f: &Fx, text: &str) -> String {
    let before = fs::read(&f.path).unwrap();
    let o = transact(f, text, false);
    assert_eq!(code(&o), 1, "{text}: {}", stderr(&o));
    assert_eq!(fs::read(&f.path).unwrap(), before, "{text}");
    stderr(&o).trim_end().to_owned()
}
fn values(f: &Fx, who: &str, attr: &str, tick: i64) -> Vec<Value> {
    let w = World::open_read_only(&f.path).unwrap();
    w.as_of(w.entity_by_handle(who).unwrap(), attr, Tick(tick))
        .unwrap()
}
const PREFIX: &str = "kala: error: form 1 failed; 0 prior forms committed: ";

// --- D: the :set same-tick diagnostic ---------------------------------------

/// The conflicting assert was staged by an earlier op of the same form. It has
/// no entry ID: the form aborts, and the ID it would have had goes to an
/// unrelated later fact, so a `[:correct N …]` hint would correct the wrong
/// entry.
#[test]
fn set_conflict_with_an_earlier_op_of_the_form_names_the_op_not_an_entry() {
    let f = build(EMPTY);
    let err = refused(
        &f,
        r#"[[:set :greg :alias ["a" "b" "c"] :at 400] [:set :greg :alias ["b" "d"] :at 400]]"#,
    );
    assert_eq!(
        err,
        format!(
            "{PREFIX}op 2 (:set): same-tick conflict: the opposite op is already stored for greg \"alias\" at tick 400; \"a\" was asserted at that tick by op 1 of this transaction: change that operation instead"
        )
    );
    assert!(
        !err.contains("entry#") && !err.contains("[:correct"),
        "{err}"
    );
    // The ID the old message named is given to an unrelated fact next.
    let next = apply(&f, "[:assert :edwin :age 30 :at 1]");
    assert_eq!(next["changed"], 1);
}

#[test]
fn set_conflict_with_a_staged_retract_names_the_op() {
    let f = build(EMPTY);
    apply(&f, "[:assert :greg :age 18 :at 0]");
    let err = refused(
        &f,
        "[[:retract :greg :age 18 :at 10] [:set :greg :age 18 :at 10]]",
    );
    assert_eq!(
        err,
        format!(
            "{PREFIX}op 2 (:set): same-tick conflict: the opposite op is already stored for greg \"age\" at tick 10; 18 was retracted at that tick by op 1 of this transaction: change that operation instead"
        )
    );
}

/// Removing a value that a committed entry asserted at this very tick: delete
/// that assert, or replace its value.
#[test]
fn set_removing_a_value_asserted_at_the_tick_offers_delete_or_replace() {
    let f = build(EMPTY);
    let first = apply(&f, r#"[:set :greg :alias ["a" "b"] :at 400]"#);
    let a = first["entries"][0].as_u64().unwrap();
    let err = refused(&f, r#"[:set :greg :alias ["b"] :at 400]"#);
    assert_eq!(
        err,
        format!(
            "{PREFIX}op 1 (:set): same-tick conflict: the opposite op is already stored for greg \"alias\" at tick 400; \"a\" was asserted at that tick by entry#{a}: use [:correct {a} :delete] to remove that assert, or [:correct {a} :replace :value …] to change its value"
        )
    );
    // Following the hint works and gives what the :set asked for.
    apply(&f, &format!("[:correct {a} :delete]"));
    assert_eq!(values(&f, "greg", "alias", 400), [Value::Str("b".into())]);
    assert_eq!(
        apply(&f, r#"[:set :greg :alias ["b"] :at 400]"#)["changed"],
        0
    );
}

/// Asserting a value that a committed entry retracted at this very tick: the
/// fix is deleting that retract (replacing its value would not help).
#[test]
fn set_asserting_a_value_retracted_at_the_tick_offers_delete_only() {
    let f = build(EMPTY);
    apply(&f, "[:assert :greg :age 18 :at 0]");
    let retraction = apply(&f, "[:retract :greg :age 18 :at 10]");
    let r = retraction["entries"][0].as_u64().unwrap();
    let err = refused(&f, "[:set :greg :age 18 :at 10]");
    assert_eq!(
        err,
        format!(
            "{PREFIX}op 1 (:set): same-tick conflict: the opposite op is already stored for greg \"age\" at tick 10; 18 was retracted at that tick by entry#{r}: use [:correct {r} :delete] to remove that retract"
        )
    );
    assert!(!err.contains(":replace"), "{err}");
    apply(&f, &format!("[:correct {r} :delete]"));
    assert_eq!(values(&f, "greg", "age", 10), [Value::I64(18)]);
}

/// An entity without a handle reads `TYPE#n`, a ref value reads as its
/// handle, and nothing is printed in Rust debug form.
#[test]
fn same_tick_messages_label_entities_and_values() {
    let f = build(EMPTY);
    let created = apply(&f, "[:create :CHARACTER]");
    let id = created["ops"][0]["entity"].as_str().unwrap().to_owned();
    assert_eq!(id, "CHARACTER#4");
    let first = apply(&f, &format!("[:assert [:id \"{id}\"] :friend :mara :at 5]"));
    let entry = first["entries"][0].as_u64().unwrap();
    let err = refused(&f, &format!("[:set [:id \"{id}\"] :friend [] :at 5]"));
    assert_eq!(
        err,
        format!(
            "{PREFIX}op 1 (:set): same-tick conflict: the opposite op is already stored for CHARACTER#4 \"friend\" at tick 5; mara was asserted at that tick by entry#{entry}: use [:correct {entry} :delete] to remove that assert, or [:correct {entry} :replace :value …] to change its value"
        )
    );
    // The plain forms get the same subject label (no entry detail).
    let err = refused(
        &f,
        &format!("[:retract [:id \"{id}\"] :friend :mara :at 5]"),
    );
    assert_eq!(
        err,
        format!(
            "{PREFIX}op 1 (:retract): same-tick conflict: the opposite op is already stored for CHARACTER#4 \"friend\" at tick 5"
        )
    );
    apply(&f, "[:assert :greg :age 18 :at 601]");
    let err = refused(&f, "[:retract :greg :age 18 :at 601]");
    assert_eq!(
        err,
        format!(
            "{PREFIX}op 1 (:retract): same-tick conflict: the opposite op is already stored for greg \"age\" at tick 601"
        )
    );
    assert!(!err.contains("EntityId("), "{err}");
}

// --- E: :set takes every single value :assert takes --------------------------

#[test]
fn set_accepts_tagged_scalars_as_single_values_like_assert() {
    let f = build(EMPTY);
    let inf = Value::f64(f64::INFINITY).unwrap();
    let j = apply(&f, "[:set :greg :score [:f64 :inf] :at 300]");
    assert_eq!(j["changed"], 1);
    assert_eq!(values(&f, "greg", "score", 300), [inf]);
    // The same value through :assert is already held: 0 changed.
    assert_eq!(
        apply(&f, "[:assert :greg :score [:f64 :inf] :at 300]")["changed"],
        0
    );

    let edwin = f.id("edwin");
    assert_eq!(
        World::open_read_only(&f.path).unwrap().format_id(edwin),
        "CHARACTER#2"
    );
    let j = apply(&f, "[:set :greg :friend [:id \"CHARACTER#2\"] :at 300]");
    assert_eq!(j["changed"], 1);
    assert_eq!(values(&f, "greg", "friend", 300), [Value::Ref(edwin)]);
}

#[test]
fn set_member_lists_take_tagged_values_and_wrapped_edge_triples() {
    let f = build(EMPTY);
    let j = apply(&f, "[:set :greg :scores [1.5 [:f64 :-inf]] :at 300]");
    assert_eq!(j["changed"], 2);
    assert_eq!(
        values(&f, "greg", "scores", 300),
        [
            Value::f64(f64::NEG_INFINITY).unwrap(),
            Value::f64(1.5).unwrap()
        ]
    );
    let j = apply(
        &f,
        "[:set :greg :friend [:mara [:id \"CHARACTER#2\"]] :at 300]",
    );
    assert_eq!(j["changed"], 2);
    assert_eq!(
        values(&f, "greg", "friend", 300),
        [Value::Ref(f.id("edwin")), Value::Ref(f.id("mara"))]
    );

    // One edge as the single value is wrapped in a member list.
    apply(&f, "[:assert :edwin :ally_of :mara :at 1]");
    let j = apply(&f, "[:set :greg :about [[:edwin :ally_of :mara]] :at 300]");
    assert_eq!(j["changed"], 1);
    let w = World::open_read_only(&f.path).unwrap();
    let edge = w.edge_id(f.id("edwin"), "ally_of", f.id("mara")).unwrap();
    assert_eq!(values(&f, "greg", "about", 300), [Value::Ref(edge.into())]);
    drop(w);

    // A bare triple is three members, so its middle element is read as a
    // handle (documented): wrap the triple instead.
    let err = refused(&f, "[:set :greg :about [:edwin :ally_of :mara] :at 301]");
    assert_eq!(
        err,
        format!("{PREFIX}op 1 (:set): unknown entity `:ally_of`")
    );
    let j = apply(&f, "[:set :greg :friend [:edwin :mara :greg] :at 302]");
    assert_eq!(j["changed"], 1);
    assert_eq!(values(&f, "greg", "friend", 302).len(), 3);
}

// --- F: limits say what they are ---------------------------------------------

/// `:set` reads at most 256 held values. Past that the error names the
/// attribute, the tick and the way out, not the generic budget text.
#[test]
fn set_on_an_attribute_holding_more_than_256_values_says_so() {
    for held in [257, 400] {
        let f = build(EMPTY);
        let mut w = World::open(&f.path).unwrap();
        let edwin = w.entity_by_handle("edwin").unwrap();
        let mut tx = w.begin();
        for i in 0..held {
            tx.assert(edwin, "alias", Value::Str(format!("a{i}")), Some(Tick(0)))
                .unwrap();
        }
        tx.commit().unwrap();
        drop(w);
        for source in [
            "[:set :edwin :alias [] :at 10]",
            "[:set :edwin :alias [\"a1\" \"new\"] :at 10]",
        ] {
            assert_eq!(
                refused(&f, source),
                format!(
                    "{PREFIX}op 1 (:set): cannot set :alias: it holds more than 256 values at tick 10 (the :set limit); use :retract/:assert"
                ),
                "{held} held, {source}"
            );
        }
        // At a tick where it holds nothing, the same form is fine.
        assert_eq!(
            apply(&f, "[:set :edwin :alias [\"x\"] :at -5]")["changed"],
            1
        );
    }
}

// --- G: the capped resolved-warnings line ------------------------------------

/// Doctor lists current overlaps; resolved ones no longer exist, so only the
/// introduced line points at it.
#[test]
fn capped_resolved_warnings_do_not_point_at_doctor() {
    let f = build(EMPTY);
    let members = (0..17)
        .map(|n| format!("\"n{n:02}\""))
        .collect::<Vec<_>>()
        .join(" ");
    apply(&f, &format!("[:set :edwin :name [{members}] :at 0]"));
    let output = transact(&f, "[:set :edwin :name [] :at 10]", false);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let out = stdout(&output);
    let lines: Vec<&str> = out.lines().filter(|l| l.starts_with('…')).collect();
    assert_eq!(
        lines,
        [
            "… and 36 more overlap warnings (run kala doctor to list them)",
            "… and 36 more resolved overlap warnings",
        ],
        "{out}"
    );
}
