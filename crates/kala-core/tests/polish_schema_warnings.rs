//! Guards: schema changes that do not touch cardinality introduce or resolve
//! no overlap warnings of their own, on a world that already has overlaps.
//! There is no deterministic observable for "the full-world overlap scan was
//! skipped" (no counter or hook in kala-core), so only these guards exist; they
//! pass today and must keep passing when the scan is optimized.

use kala_core::*;

fn schema(predeclared: bool) -> Schema {
    let mut s = Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "owns",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ));
    if predeclared {
        s = s.node_type("FACTION").attribute(AttrDef::new(
            "age",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ));
    }
    s
}

struct W {
    world: World,
    greg: EntityId,
    carnath: EntityId,
}

/// Greg is in Varrow from 200 and in Ashford from 601: one standing overlap.
fn overlapping_world(predeclared: bool) -> W {
    let mut world = World::in_memory(schema(predeclared)).unwrap();
    let greg = world.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = world.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = world.create_entity("PLACE", Some("ashford")).unwrap();
    let carnath = world.create_entity("PLACE", Some("carnath")).unwrap();
    world
        .assert(greg, "located_in", Value::Ref(varrow), Some(Tick(200)))
        .unwrap();
    world
        .assert(greg, "located_in", Value::Ref(ashford), Some(Tick(601)))
        .unwrap();
    assert_eq!(world.overlaps("located_in").unwrap().len(), 1);
    W {
        world,
        greg,
        carnath,
    }
}

fn warnings_of(mut w: W, infer: bool, extra_overlap: bool) -> Warnings {
    let mut tx = w.world.begin();
    if infer {
        tx.infer_attribute("age", &Value::I64(42)).unwrap();
    }
    tx.assert(w.greg, "age", Value::I64(42), Some(Tick(0)))
        .unwrap();
    if extra_overlap {
        tx.assert(w.greg, "located_in", Value::Ref(w.carnath), Some(Tick(700)))
            .unwrap();
    }
    tx.commit().unwrap().warnings
}

#[test]
fn inferring_an_attribute_on_an_overlapping_world_reports_no_spurious_warnings() {
    let warnings = warnings_of(overlapping_world(false), true, false);
    assert!(warnings.introduced.is_empty(), "{warnings:?}");
    assert!(warnings.resolved.is_empty(), "{warnings:?}");
    assert_eq!(warnings, warnings_of(overlapping_world(true), false, false));
}

#[test]
fn inferring_an_attribute_does_not_change_the_warnings_of_a_real_overlap_write() {
    let inferred = warnings_of(overlapping_world(false), true, true);
    let declared = warnings_of(overlapping_world(true), false, true);
    assert!(
        !inferred.introduced.is_empty(),
        "the extra write creates an overlap: {inferred:?}"
    );
    assert_eq!(inferred, declared);
}

#[test]
fn declaring_a_node_type_reports_no_spurious_warnings() {
    let mut w = overlapping_world(false);
    let mut tx = w.world.begin();
    tx.infer_node_type("FACTION").unwrap();
    tx.create_entity("FACTION", Some("red_hand")).unwrap();
    let inferred = tx.commit().unwrap().warnings;
    let mut w = overlapping_world(true);
    let mut tx = w.world.begin();
    tx.create_entity("FACTION", Some("red_hand")).unwrap();
    let declared = tx.commit().unwrap().warnings;
    assert!(
        inferred.introduced.is_empty() && inferred.resolved.is_empty(),
        "{inferred:?}"
    );
    assert_eq!(inferred, declared);
}

#[test]
fn many_to_one_still_reports_the_overlaps_it_introduces() {
    let mut w = overlapping_world(false);
    let sword = w.world.create_entity("PLACE", Some("hold")).unwrap();
    let other = w.world.create_entity("PLACE", Some("keep")).unwrap();
    w.world
        .assert(w.greg, "owns", Value::Ref(sword), Some(Tick(0)))
        .unwrap();
    w.world
        .assert(w.greg, "owns", Value::Ref(other), Some(Tick(10)))
        .unwrap();
    let mut tx = w.world.begin();
    tx.set_cardinality("owns", Cardinality::One).unwrap();
    let warnings = tx.commit().unwrap().warnings;
    assert_eq!(warnings.introduced.len(), 1, "{warnings:?}");
    assert!(warnings.resolved.is_empty(), "{warnings:?}");
    assert_eq!(warnings.introduced[0].entity, w.greg);
}
