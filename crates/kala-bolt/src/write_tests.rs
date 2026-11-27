use super::*;

#[test]
fn cumulative_summary_details_are_bounded_before_final_encoding() {
    use kala_core::overlap::{Interval, Overlap, Side};
    let mut world = World::in_memory(kala_core::Schema::new().node_type("CHARACTER")).unwrap();
    let entity = world.create_entity("CHARACTER", Some("greg")).unwrap();
    let mut result = world.begin().commit().unwrap();
    result.warnings.introduced = vec![
        Overlap {
            entity,
            attr: "alias".into(),
            side: Side::Forward,
            a: kala_core::Value::Str("a".repeat(100)),
            b: kala_core::Value::Str("b".repeat(100)),
            during: Interval {
                from: kala_core::Tick(0),
                until: None
            },
        };
        100
    ];
    let mut summary = Summary::new(2048);
    for _ in 0..256 {
        summary.add(&world, &result, &[], true);
        let optional = Value::Map(
            [
                ("entries".into(), ids(&summary.entries)),
                ("ids".into(), Value::List(summary.mapping.clone())),
                (
                    "notifications".into(),
                    Value::List(summary.notifications.clone()),
                ),
            ]
            .into(),
        );
        let retained_bytes = packstream::encode(&optional).unwrap().len();
        assert!(
            retained_bytes <= 2048,
            "summary retained {retained_bytes} bytes before final encoding despite its 2048-byte bound"
        );
    }
    let metadata = summary.metadata("varrow", false, 2048, 32);
    let retained = match metadata.get("notifications") {
        Some(Value::List(items)) => items.len(),
        None => 0,
        other => panic!("invalid notifications: {other:?}"),
    };
    let Some(Value::Map(kala)) = metadata.get("kala") else {
        panic!("missing Kala metadata")
    };
    let Some(Value::Integer(omitted)) = kala.get("notifications_omitted") else {
        panic!("missing exact omitted count: {kala:?}")
    };
    assert_eq!(retained + usize::try_from(*omitted).unwrap(), 256 * 100);
}

/// A request's deadline is carried into the executor's own cancellation
/// checks; no helper thread watches the clock. (A thread per query costs
/// every read, and `spawn` panics when the OS refuses a thread.) A thread
/// that is not started can't be observed from a test, so this pins the
/// source: the request path names no thread API.
#[test]
fn the_request_path_spawns_no_thread() {
    for (file, source) in [
        ("query.rs", include_str!("query.rs")),
        ("session.rs", include_str!("session.rs")),
        ("metadata.rs", include_str!("metadata.rs")),
    ] {
        for api in [
            "thread::scope",
            "thread::spawn",
            "thread::Builder",
            ".spawn(",
        ] {
            assert!(!source.contains(api), "{file} uses {api}");
        }
    }
}
