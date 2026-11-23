//! World creation and atomic replay through the real CLI.
mod common;
use common::{code, kala, stderr, stdout};
use kala_core::World;
use serde_json::Value as Json;
use std::fs;
use std::path::Path;

fn success(output: &std::process::Output) {
    assert_eq!(code(output), 0, "{}", stderr(output));
}
fn replay(dir: &Path, name: &str, source: &str) -> std::process::Output {
    let input = dir.join("input.edn");
    fs::write(&input, source).unwrap();
    kala([
        "transact",
        "--create",
        dir.join(name).to_str().unwrap(),
        input.to_str().unwrap(),
    ])
}

#[test]
fn empty_world_infers_and_handleless_create_has_a_stable_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.kala");
    success(&kala(["create", path.to_str().unwrap()]));
    let o = kala(["schema", path.to_str().unwrap(), "--json"]);
    success(&o);
    let schema: Json = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(schema["node_types"].as_array().unwrap().len(), 0);
    assert_eq!(schema["attributes"].as_array().unwrap().len(), 0);
    let source = dir.path().join("writes.edn");
    fs::write(
        &source,
        "[[:create :CHARACTER :ann] [:assert :ann :name \"Ann\"] [:create :FACTION]]",
    )
    .unwrap();
    success(&kala([
        "transact",
        path.to_str().unwrap(),
        source.to_str().unwrap(),
    ]));
    let world = World::open_read_only(&path).unwrap();
    assert_eq!(
        world.format_id(world.entity_by_handle("ann").unwrap()),
        "CHARACTER#1"
    );
    let faction = world.parse_id("FACTION#1").unwrap();
    assert_eq!(world.handle_of(faction), None);
    assert_eq!(world.latest_tx().0, 1);
}

#[test]
fn schema_file_forms_and_genesis_wrapper_are_the_same_strict_genesis() {
    let dir = tempfile::tempdir().unwrap();
    let forms = "[:world-setting :schema :strict]\n[:node-type :CHARACTER]\n[:attribute :name :string :one :state :indexed]";
    for (name, text) in [
        ("plain", forms.to_owned()),
        ("wrapped", format!("[:genesis {forms}]")),
    ] {
        let input = dir.path().join(format!("{name}.edn"));
        let path = dir.path().join(format!("{name}.kala"));
        fs::write(&input, text).unwrap();
        success(&kala([
            "create",
            path.to_str().unwrap(),
            "--schema",
            input.to_str().unwrap(),
        ]));
        let world = World::open_read_only(&path).unwrap();
        assert_eq!(world.latest_tx().0, 0);
        assert_eq!(world.node_types().count(), 1);
        assert_eq!(world.attributes().count(), 1);
        let writes = dir.path().join("writes.edn");
        fs::write(&writes, "[:create :UNDECLARED :ann]").unwrap();
        assert_ne!(
            code(&kala([
                "transact",
                path.to_str().unwrap(),
                writes.to_str().unwrap()
            ])),
            0
        );
    }
    assert_eq!(
        fs::read(dir.path().join("plain.kala")).unwrap(),
        fs::read(dir.path().join("wrapped.kala")).unwrap()
    );
}

#[test]
fn replay_refuses_existing_missing_genesis_and_failed_forms_without_placement() {
    let dir = tempfile::tempdir().unwrap();
    let existing = dir.path().join("existing.kala");
    fs::write(&existing, b"preserve me").unwrap();
    let o = replay(dir.path(), "existing.kala", "[:genesis]");
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    assert_eq!(fs::read(existing).unwrap(), b"preserve me");
    for source in [
        "[:create :CHARACTER :ann]",
        "[:genesis] [:create :CHARACTER :ann] [:assert :missing :age 1]",
        "[:genesis [:create :CHARACTER :ann]]",
        "[:genesis] [:genesis]",
    ] {
        let o = replay(dir.path(), "failed.kala", source);
        assert_ne!(code(&o), 0, "{source}: {}", stdout(&o));
        assert!(!dir.path().join("failed.kala").exists(), "{source}");
        assert!(
            fs::read_dir(dir.path()).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );
    }
}

#[test]
fn normal_transact_refuses_genesis_with_create_hint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    drop(World::create(&path, kala_core::Schema::new()).unwrap());
    let before = fs::read(&path).unwrap();
    let input = dir.path().join("dump.edn");
    fs::write(&input, "[:genesis]").unwrap();
    let o = kala(["transact", path.to_str().unwrap(), input.to_str().unwrap()]);
    assert_ne!(code(&o), 0);
    assert!(stderr(&o).contains("--create"), "{}", stderr(&o));
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn create_replay_lifts_size_operations_and_forms_limits_but_normal_mode_keeps_them() {
    let dir = tempfile::tempdir().unwrap();
    let big = "q".repeat(100 * 1024);
    let cases = [
        format!(
            "[[:create :CHARACTER :ann] [:attribute :bio :string :unindexed] [:assert :ann :bio \"{big}\"]]"
        ),
        format!(
            "[{}]",
            (0..257)
                .map(|i| format!("[:create :CHARACTER :person{i}]"))
                .collect::<String>()
        ),
        (0..257)
            .map(|i| format!("[:create :CHARACTER :person{i}]\n"))
            .collect::<String>(),
    ];
    for (i, body) in cases.iter().enumerate() {
        let name = format!("large{i}.kala");
        let o = replay(dir.path(), &name, &format!("[:genesis]\n{body}"));
        success(&o);
        let text = stdout(&o);
        assert!(
            text.contains("replayed") && text.contains("transactions") && text.contains("entries"),
            "{text}"
        );
        assert_eq!(text.lines().count(), 1, "no per-transaction output");
        let ordinary = dir.path().join(format!("ordinary{i}.kala"));
        drop(World::create(&ordinary, kala_core::Schema::new()).unwrap());
        let input = dir.path().join("normal.edn");
        fs::write(&input, body).unwrap();
        let o = kala([
            "transact",
            ordinary.to_str().unwrap(),
            input.to_str().unwrap(),
        ]);
        assert_ne!(code(&o), 0, "normal-mode limit {i} was lifted");
    }
}

#[test]
fn unlimited_replay_retains_depth_and_live_validation() {
    let dir = tempfile::tempdir().unwrap();
    let nested = format!("[:genesis] {}0{}", "[".repeat(64), "]".repeat(64));
    let o = replay(dir.path(), "deep.kala", &nested);
    assert_ne!(code(&o), 0);
    assert!(!dir.path().join("deep.kala").exists());
    for (i, text) in [
        "[:genesis] [[:create :CHARACTER :ann] [:assert :ann :age 1 :at 0] [:retract :ann :age 1 :at 0]]",
        "[:genesis] [[:create :CHARACTER :ann] [:assert :ann :age 1] [:assert :ann :age \"wrong\"]]",
        "[:genesis] [[:create :CHARACTER :ann] [:create :CHARACTER :ann]]",
        "[:genesis] [[:create :CHARACTER :ann] [:assert :ann :age 1] [:deprecate :age] [:assert :ann :age 2]]",
    ].iter().enumerate() {
        let name = format!("invalid{i}.kala");
        let o = replay(dir.path(), &name, text);
        assert_ne!(code(&o), 0, "{text}");
        assert!(!dir.path().join(name).exists());
    }
}

#[test]
fn inferred_markers_cannot_bypass_strict_mode_or_change_inference_defaults() {
    let dir = tempfile::tempdir().unwrap();
    for (i, source) in [
        "[:genesis [:world-setting :schema :strict]] [:node-type :CHARACTER :inferred]",
        "[:genesis [:world-setting :schema :strict]] [:attribute :age :i64 :inferred]",
        "[:genesis] [:attribute :age :i64 :inferred :many]",
        "[:genesis] [:attribute :age :i64 :inferred :unindexed]",
        "[:genesis] [:attribute :friend :ref :inferred :bidirectional]",
        "[:genesis] [:attribute :born :i64 :inferred :event]",
    ]
    .iter()
    .enumerate()
    {
        let name = format!("inferred{i}.kala");
        let o = replay(dir.path(), &name, source);
        assert_ne!(code(&o), 0, "{source} incorrectly succeeded");
        assert!(!dir.path().join(name).exists());
    }
}

#[cfg(unix)]
#[test]
fn create_and_replay_refuse_dangling_symlink_targets() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("world.kala");
    let absent = dir.path().join("absent.kala");
    symlink(&absent, &target).unwrap();
    let created = kala(["create", target.to_str().unwrap()]);
    assert_eq!(code(&created), 2, "{}", stderr(&created));
    let restored = replay(dir.path(), "world.kala", "[:genesis]");
    assert_eq!(code(&restored), 2, "{}", stderr(&restored));
    assert!(
        fs::symlink_metadata(target)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!absent.exists());
}

#[test]
fn create_replay_json_reports_one_document_with_committed_counts() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("dump.edn");
    let target = dir.path().join("world.kala");
    fs::write(
        &input,
        "[:genesis]\n[:create :CHARACTER :ann]\n[:assert :ann :name \"Ann\"]",
    )
    .unwrap();
    let o = kala([
        "transact",
        "--create",
        target.to_str().unwrap(),
        input.to_str().unwrap(),
        "--json",
    ]);
    success(&o);
    let json: Json = serde_json::from_slice(&o.stdout).unwrap();
    let world = World::open_read_only(&target).unwrap();
    assert_eq!(json["transactions"], 2);
    assert_eq!(json["entries"], world.entries().count());
}

#[test]
fn malformed_system_restore_errors_never_echo_input_or_fake_hashes() {
    const SENTINEL: &str = "sentinel-fake-hash-restore-test-83741";
    let dir = tempfile::tempdir().unwrap();
    for (i, source) in [
        format!("[:genesis [:node-type :_KALA_SYSTEM]] [:{SENTINEL}]"),
        format!("[:genesis [:node-type :_KALA_SYSTEM]] [:create :_KALA_SYSTEM :{SENTINEL}] [:assert :{SENTINEL} :password_hash \"{SENTINEL}\" :at 0] [:assert :{SENTINEL} :password_hash 123 :at 1]"),
    ].iter().enumerate() {
        let name = format!("system{i}.kala");
        let output = replay(dir.path(), &name, source);
        assert_ne!(code(&output), 0, "malformed account input was accepted");
        assert!(!dir.path().join(name).exists());
        assert!(!stdout(&output).contains(SENTINEL));
        assert!(!stderr(&output).contains(SENTINEL));
    }
}
