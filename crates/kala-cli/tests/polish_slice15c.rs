//! PR #21, second polish: world-setting keys at genesis and in a restore.

mod common;
use common::{code, kala, stderr};
use kala_core::{Schema, World};
use std::fs;

fn success(output: &std::process::Output) {
    assert_eq!(code(output), 0, "{}", stderr(output));
}

#[test]
fn create_refuses_a_genesis_setting_with_an_unknown_key() {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in [
        (
            "plain",
            "[:node-type :CHARACTER]\n[:world-setting :house_rule :x]",
        ),
        (
            "wrapped",
            "[:genesis [:node-type :CHARACTER] [:world-setting :house_rule :x]]",
        ),
    ] {
        let input = dir.path().join(format!("{name}.edn"));
        let path = dir.path().join(format!("{name}.kala"));
        fs::write(&input, text).unwrap();
        let o = kala([
            "create",
            path.to_str().unwrap(),
            "--schema",
            input.to_str().unwrap(),
        ]);
        assert_ne!(code(&o), 0, "{name}");
        let message = stderr(&o);
        assert!(message.contains("unknown world setting"), "{message}");
        for known in kala_core::WORLD_SETTING_KEYS {
            assert!(message.contains(&format!(":{known}")), "{message}");
        }
        assert!(!path.exists(), "{name}: nothing was created");
    }
    // The known keys still work at genesis.
    let input = dir.path().join("known.edn");
    let path = dir.path().join("known.kala");
    fs::write(
        &input,
        "[:world-setting :schema :strict]\n[:world-setting :tick_unit :day]",
    )
    .unwrap();
    success(&kala([
        "create",
        path.to_str().unwrap(),
        "--schema",
        input.to_str().unwrap(),
    ]));
}

/// A world whose log already holds settings with keys outside the list
/// (written before the list was enforced). Built with the two kala-core
/// switches meant for restoring such a world: no CLI command or Bolt write
/// can produce it any more.
fn legacy_world(path: &std::path::Path) {
    let mut w = World::create(
        path,
        Schema::new()
            .node_type("CHARACTER")
            .setting("house_rule", "x")
            .setting("tick_unit", "day")
            .accepting_unknown_settings(),
    )
    .unwrap();
    let mut t = w.begin();
    t.create_entity("CHARACTER", Some("greg")).unwrap();
    t.commit().unwrap();
    let mut t = w.begin();
    t.set_unknown_settings_allowed(true);
    t.world_setting("old_key", "1").unwrap();
    t.world_setting("house_rule", "y").unwrap();
    t.commit().unwrap();
}

#[test]
fn a_world_with_legacy_setting_keys_dumps_and_restores_to_an_identical_dump() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.kala");
    legacy_world(&path);
    let dump = dir.path().join("legacy.edn");
    let restored = dir.path().join("restored.kala");
    let again = dir.path().join("again.edn");
    success(&kala([
        "dump",
        path.to_str().unwrap(),
        "-o",
        dump.to_str().unwrap(),
    ]));
    let text = fs::read_to_string(&dump).unwrap();
    assert!(text.contains("[:world-setting :house_rule :x]"), "{text}");
    assert!(text.contains("[:world-setting :old_key"), "{text}");
    success(&kala([
        "transact",
        "--create",
        restored.to_str().unwrap(),
        dump.to_str().unwrap(),
    ]));
    assert_eq!(fs::read(&path).unwrap(), fs::read(&restored).unwrap());
    success(&kala([
        "dump",
        restored.to_str().unwrap(),
        "-o",
        again.to_str().unwrap(),
    ]));
    assert_eq!(text, fs::read_to_string(&again).unwrap());
    success(&kala(["doctor", restored.to_str().unwrap()]));
    let w = World::open_read_only(&restored).unwrap();
    assert_eq!(w.world_setting("house_rule"), Some("y"));
    assert_eq!(w.world_setting("old_key"), Some("1"));
    // The restored world is an ordinary one: a live write still refuses
    // an unknown key, also one the world already holds.
    for key in ["old_key", "brand_new"] {
        let source = dir.path().join("live.edn");
        fs::write(&source, format!("[:world-setting :{key} :2]")).unwrap();
        let o = kala([
            "transact",
            restored.to_str().unwrap(),
            source.to_str().unwrap(),
        ]);
        assert_ne!(code(&o), 0, "{key}");
        assert!(
            stderr(&o).contains("unknown world setting"),
            "{}",
            stderr(&o)
        );
    }
}
