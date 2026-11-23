//! Latest names and canon-specific definitions survive the Bolt mapping.
#[path = "common/client.rs"]
mod client;
mod common;

use client::{Client, string, structure};
use common::{Running, VARROW};
use kala_bolt::{ServerConfig, packstream::Value};

#[test]
fn renamed_properties_and_relationships_use_new_names_at_old_canon() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("varrow.kala");
    let mut world = kala_core::load_fixture_to(VARROW, &path).unwrap();
    let mut tx = world.begin();
    let renamed = tx.rename_attribute("name", "title");
    assert!(
        renamed.is_ok(),
        "rename must be available before serving: {renamed:?}"
    );
    tx.rename_attribute("present", "visiting").unwrap();
    tx.commit().unwrap();
    // A field first defined after tx#12 must not appear on an old-canon node.
    let edwin = world.entity_by_handle("edwin").unwrap();
    let mut tx = world.begin();
    tx.infer_attribute("age", &kala_core::Value::I64(42))
        .unwrap();
    tx.assert(
        edwin,
        "age",
        kala_core::Value::I64(42),
        Some(kala_core::Tick(-250)),
    )
    .unwrap();
    tx.commit().unwrap();
    drop(world);

    let server = Running::in_dir(
        dir,
        ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            ..ServerConfig::default()
        },
    );
    let mut client = Client::ready(&server);
    for canon in ["", " :canon 12"] {
        let rows = client.query(&format!(
            "// kala:datalog\n[:find ?e :at -230{canon} :where [?e :handle \"edwin\"]]"
        ));
        assert_eq!(rows.len(), 1);
        let fields = structure(rows[0][0].clone(), 0x4e);
        let Value::Map(properties) = &fields[2] else {
            panic!("node properties must be a map: {fields:?}")
        };
        assert_eq!(properties["title"], string("Edwin"));
        assert!(!properties.contains_key("name"));
        if canon.is_empty() {
            assert_eq!(properties["age"], Value::Integer(42));
        } else {
            assert!(!properties.contains_key("age"));
        }
        let rows = client.query(&format!(
            "// kala:datalog\n[:find ?e :at -230{canon} :where [?e :disguised true]]"
        ));
        assert_eq!(rows.len(), 1);
        let fields = structure(rows[0][0].clone(), 0x52);
        assert_eq!(fields[3], string("visiting"));
    }
}
