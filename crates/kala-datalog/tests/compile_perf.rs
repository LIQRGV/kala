//! M1: one recursive rotation must not re-resolve the whole body once per
//! head position. The release threshold is the plan's acceptance budget.
use kala_core::{AttrDef, Cardinality, Kind, Schema, ValueType, World};

fn rotation(arity: usize) -> String {
    let vars: Vec<_> = (0..arity).map(|i| format!("?x{i}")).collect();
    let rotated: Vec<_> = vars.iter().cycle().skip(1).take(arity).cloned().collect();
    format!(
        "[:find ?x0 :rules [[(r {}) [?e :name ?x0] (r {})]] :where (r {})]",
        vars.join(" "),
        rotated.join(" "),
        vars.join(" ")
    )
}

fn world() -> World {
    World::in_memory(Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "name",
        ValueType::Str,
        Kind::State,
        Cardinality::One,
    )))
    .unwrap()
}

#[test]
fn rotating_types_small_correctness_twin() {
    let world = world();
    let result = kala_datalog::prepare(&world, &rotation(12));
    assert!(result.is_ok(), "{result:?}");
}

#[test]
#[cfg_attr(debug_assertions, ignore)]
fn rotating_two_thousand_positions_prepares_under_two_hundred_ms() {
    let world = world();
    let source = rotation(2000);
    let start = std::time::Instant::now();
    let result = kala_datalog::prepare(&world, &source);
    let elapsed = start.elapsed();
    assert!(result.is_ok(), "{result:?}");
    assert!(
        elapsed < std::time::Duration::from_millis(200),
        "M1 rotation took {elapsed:?}"
    );
}
