//! Replayable dumps must retain every committed log byte.
mod common;

use common::{VARROW, code, kala, stderr};
use std::fs;
use std::path::Path;

fn success(output: &std::process::Output) {
    assert_eq!(code(output), 0, "{}", stderr(output));
}

fn load(path: &Path) {
    success(&kala(["load", VARROW, path.to_str().unwrap()]));
}

fn roundtrip(path: &Path, dir: &Path) -> String {
    let dump = dir.join("world.dump.edn");
    let replay = dir.join("replayed.kala");
    success(&kala([
        "dump",
        path.to_str().unwrap(),
        "-o",
        dump.to_str().unwrap(),
    ]));
    success(&kala([
        "transact",
        "--create",
        replay.to_str().unwrap(),
        dump.to_str().unwrap(),
    ]));
    let source = fs::read(path).unwrap();
    let restored = fs::read(&replay).unwrap();
    assert!(
        source == restored,
        "dump replay must reproduce every committed byte (source {} bytes, replay {} bytes)",
        source.len(),
        restored.len()
    );
    success(&kala(["doctor", replay.to_str().unwrap()]));
    fs::read_to_string(dump).unwrap()
}

#[test]
fn varrow_dump_replays_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let world = dir.path().join("varrow.kala");
    load(&world);
    roundtrip(&world, dir.path());
}

fn transact(path: &Path, source: &str) {
    let input = path.with_extension("writes.edn");
    fs::write(&input, source).unwrap();
    success(&kala([
        "transact",
        path.to_str().unwrap(),
        input.to_str().unwrap(),
    ]));
}

#[test]
fn schema_edits_corrections_migrations_and_inferred_declarations_replay_in_order() {
    use kala_core::{Correction, Precision, Tick, World};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    load(&path);
    transact(
        &path,
        "[[:assert :edwin :age 42 :at 0] [:assert :greg :age 43 :at 1] [:assert :mara :age 44 :at 2]]",
    );
    let mut world = World::open(&path).unwrap();
    let facts: Vec<_> = world
        .entries()
        .filter_map(|e| match &e.body {
            kala_core::log::EntryBody::Fact { attr, .. }
                if world.attr_name(*attr) == Some("age") =>
            {
                Some(e.id)
            }
            _ => None,
        })
        .collect();
    world.correct(facts[0], Correction::Delete).unwrap();
    world
        .correct(
            facts[1],
            Correction::Replace {
                tick: Some(Tick(-8)),
                value: None,
                op: None,
                precision: None,
            },
        )
        .unwrap();
    world
        .correct(
            facts[2],
            Correction::Replace {
                tick: None,
                value: None,
                op: None,
                precision: Some(Precision::Year),
            },
        )
        .unwrap();
    let mut tx = world.begin();
    tx.infer_node_type("FACTION").unwrap();
    tx.create_entity("FACTION", None).unwrap();
    tx.commit().unwrap();
    drop(world);
    transact(
        &path,
        "[[:rename :located_in :location] [:cardinality :ally_of :one] [:deprecate :alias] [:world-setting :schema :strict]]",
    );
    transact(&path, "[:world-setting :schema :infer]");
    transact(
        &path,
        "[[:assert :edwin :tmpname 1 :at 0] [:rename :tmpname :newname] [:assert :greg :newname 2 :at 1]]",
    );
    transact(&path, "[:migrate :age :string]");
    transact(&path, "[:migrate :disguised :string]");
    transact(&path, "[:migrate :present :bidirectional]");
    let dump = roundtrip(&path, dir.path());
    assert!(dump.contains(":located_in"));
    assert!(dump.contains(":location"));
    assert!(dump.find(":located_in").unwrap() < dump.find("[:rename").unwrap());
    assert!(dump.contains(":migrate"));
}

#[test]
fn large_native_transaction_and_utf8_numeric_extremes_roundtrip_exactly() {
    use kala_core::{AttrDef, Cardinality, Kind, Schema, Tick, Value, ValueType, World};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let schema = Schema::new()
        .node_type("CHARACTER")
        .attribute(
            AttrDef::new("title", ValueType::Str, Kind::State, Cardinality::Many).unindexed(),
        )
        .attribute(AttrDef::new(
            "number",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "fraction",
            ValueType::F64,
            Kind::State,
            Cardinality::Many,
        ));
    let mut world = World::create(&path, schema).unwrap();
    let ann = world.create_entity("CHARACTER", Some("ann")).unwrap();
    let mut tx = world.begin();
    let mut controls: String = (0..=159).filter_map(char::from_u32).collect();
    controls.push_str(" quotes: \" backslash: \\ newline:\n tab:\t emoji: 🐉 \u{202e}\u{2066}");
    controls.push_str(&"large".repeat(21_000));
    tx.assert(ann, "title", Value::Str(controls), Some(Tick(-1)))
        .unwrap();
    for (i, value) in [
        0.1,
        1e300,
        -2.5,
        f64::MIN_POSITIVE,
        f64::from_bits(1),
        f64::MAX,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ]
    .into_iter()
    .enumerate()
    {
        tx.assert(ann, "fraction", Value::F64(value), Some(Tick(i as i64)))
            .unwrap();
    }
    for tick in [i64::MIN, -1, 0, i64::MAX] {
        tx.assert(ann, "number", Value::I64(tick), Some(Tick(tick)))
            .unwrap();
    }
    for i in 0..300 {
        tx.create_entity("CHARACTER", Some(&format!("p{i}")))
            .unwrap();
    }
    tx.commit().unwrap();
    drop(world);
    roundtrip(&path, dir.path());
}

#[test]
fn dump_is_deterministic_read_only_and_refuses_existing_output_unless_forced() {
    use common::stdout;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    load(&path);
    let before = fs::read(&path).unwrap();
    let locked = kala_core::World::open(&path).unwrap();
    let first = kala(["dump", path.to_str().unwrap()]);
    success(&first);
    let second = kala(["dump", path.to_str().unwrap()]);
    success(&second);
    assert_eq!(first.stdout, second.stdout);
    let text = stdout(&first);
    assert!(text.contains("[:genesis"));
    assert!(text.contains(";; tx#1"));
    assert!(text.contains("--create"));
    let out = dir.path().join("exists.edn");
    fs::write(&out, b"preserve").unwrap();
    assert_ne!(
        code(&kala([
            "dump",
            path.to_str().unwrap(),
            "-o",
            out.to_str().unwrap()
        ])),
        0
    );
    assert_eq!(fs::read(&out).unwrap(), b"preserve");
    success(&kala([
        "dump",
        path.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--force",
    ]));
    assert_eq!(fs::read(out).unwrap(), first.stdout);
    assert_eq!(fs::read(path).unwrap(), before);
    drop(locked);
}

#[test]
fn torn_tail_is_reported_and_only_committed_prefix_is_dumped() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    load(&path);
    let committed = fs::read(&path).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&[0x29, 0, 0])
        .unwrap();
    let out = dir.path().join("dump.edn");
    let o = kala(["dump", path.to_str().unwrap(), "-o", out.to_str().unwrap()]);
    success(&o);
    let warning = stderr(&o);
    assert!(
        warning.contains("committed") && (warning.contains("tail") || warning.contains("torn")),
        "{warning}"
    );
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        committed.len() as u64 + 3
    );
    let restored = dir.path().join("restored.kala");
    success(&kala([
        "transact",
        "--create",
        restored.to_str().unwrap(),
        out.to_str().unwrap(),
    ]));
    assert_eq!(fs::read(restored).unwrap(), committed);
}

#[cfg(unix)]
#[test]
fn all_dump_and_system_restore_keep_private_modes_accounts_and_secrets() {
    use common::stdout;
    use std::os::unix::fs::PermissionsExt;
    const PASSWORD: &str = "unique-dump-test-password-943265";
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("server");
    fs::create_dir(&source).unwrap();
    load(&source.join("varrow.kala"));
    // Keep the account bootstrap lock inside its own process: another test's
    // concurrently spawned child must not inherit this process's native lock.
    let mut add = std::process::Command::new(env!("CARGO_BIN_EXE_kala"))
        .args(["user", "add"])
        .arg(&source)
        .args(["alice@127.0.0.1", "--password-stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        writeln!(add.stdin.take().unwrap(), "{PASSWORD}").unwrap();
    }
    let added = add.wait_with_output().unwrap();
    success(&added);
    let grant = kala([
        "user",
        "grant",
        source.to_str().unwrap(),
        "alice@127.0.0.1",
        "backup",
        "varrow",
    ]);
    success(&grant);
    let list = kala(["user", "list", source.to_str().unwrap()]);
    success(&list);
    assert!(stdout(&list).contains("backup:varrow"));
    let single = kala(["dump", source.join("system.kala").to_str().unwrap()]);
    assert_ne!(code(&single), 0);
    assert!(stderr(&single).contains("--all"), "{}", stderr(&single));
    assert!(single.stdout.is_empty());
    let out = dir.path().join("backup");
    let dumped = kala([
        "dump",
        "--all",
        source.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    success(&dumped);
    assert_eq!(
        fs::metadata(&out).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for name in ["varrow", "system"] {
        let file = out.join(format!("{name}.dump.edn"));
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let system_dump = fs::read_to_string(out.join("system.dump.edn")).unwrap();
    assert!(
        system_dump
            .lines()
            .take(4)
            .any(|line| line.to_ascii_lowercase().contains("password hash"))
    );
    assert!(system_dump.contains("$argon2"));
    assert!(!system_dump.contains(PASSWORD));
    let restore = dir.path().join("restore");
    fs::create_dir(&restore).unwrap();
    let restored = kala([
        "transact",
        "--create",
        restore.join("system.kala").to_str().unwrap(),
        out.join("system.dump.edn").to_str().unwrap(),
    ]);
    success(&restored);
    assert!(stderr(&restored).to_ascii_lowercase().contains("account"));
    assert_eq!(
        fs::metadata(restore.join("system.kala"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::read(source.join("system.kala")).unwrap(),
        fs::read(restore.join("system.kala")).unwrap()
    );
    let listed = kala(["user", "list", restore.to_str().unwrap()]);
    success(&listed);
    assert_eq!(list.stdout, listed.stdout);
    for output in [&added, &grant, &list, &single, &dumped, &restored, &listed] {
        for bytes in [&output.stdout, &output.stderr] {
            let text = String::from_utf8_lossy(bytes);
            assert!(!text.contains(PASSWORD), "plaintext password leaked");
            assert!(
                !text.contains("$argon2"),
                "password hash leaked outside dump file"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn all_dump_refuses_nonempty_or_symlinked_output_and_requires_a_directory_destination() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("server");
    fs::create_dir(&source).unwrap();
    load(&source.join("world.kala"));
    assert_ne!(code(&kala(["dump", "--all", source.to_str().unwrap()])), 0);
    let out = dir.path().join("out");
    fs::create_dir(&out).unwrap();
    let sentinel = out.join("world.dump.edn");
    fs::write(&sentinel, b"preserve").unwrap();
    assert_ne!(
        code(&kala([
            "dump",
            "--all",
            source.to_str().unwrap(),
            "-o",
            out.to_str().unwrap()
        ])),
        0
    );
    assert_eq!(fs::read(&sentinel).unwrap(), b"preserve");
    fs::remove_file(&sentinel).unwrap();
    let alias = dir.path().join("alias");
    symlink(&out, &alias).unwrap();
    assert_ne!(
        code(&kala([
            "dump",
            "--all",
            source.to_str().unwrap(),
            "-o",
            alias.to_str().unwrap()
        ])),
        0
    );
    assert_eq!(fs::read_dir(out).unwrap().count(), 0);
}

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    #[test]
    fn random_writes_corrections_and_schema_operations_replay_byte_identically(
        steps in prop::collection::vec((any::<i16>(), any::<i16>(), any::<u8>()), 1..14)
    ) {
        use kala_core::{AttrDef, Cardinality, Correction, Kind, MigrationOptions, Precision, Schema, Tick, Value, ValueType, World};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("world.kala");
        let mut world = World::create(&path, Schema::new().node_type("CHARACTER")).unwrap();
        let entity = world.create_entity("CHARACTER", None).unwrap();
        for (i, (number, tick, options)) in steps.into_iter().enumerate() {
            let name = format!("value{i}");
            let mut tx = world.begin();
            if options & 1 == 0 {
                tx.infer_attribute(&name, &Value::I64(number as i64)).unwrap();
            } else {
                tx.declare_attribute(AttrDef::new(&name, ValueType::I64, Kind::State, Cardinality::One)).unwrap();
            }
            tx.assert(entity, &name, Value::I64(number as i64), Some(Tick(tick as i64))).unwrap();
            tx.commit().unwrap();
            let entry = world.entries().filter(|entry| matches!(entry.body, kala_core::log::EntryBody::Fact { .. })).last().unwrap().id;
            let correction = match (options >> 1) % 4 {
                0 => Correction::Delete,
                1 => Correction::Replace { value: None, tick: Some(Tick(tick as i64 - 1)), op: None, precision: None },
                2 => Correction::Replace { value: None, tick: None, op: None, precision: Some(Precision::Year) },
                _ => Correction::Replace { value: Some(Value::I64(number as i64 + 1)), tick: None, op: None, precision: None },
            };
            world.correct(entry, correction).unwrap();
            let renamed = format!("renamed{i}");
            let mut tx = world.begin();
            tx.rename_attribute(&name, &renamed).unwrap();
            tx.set_cardinality(&renamed, Cardinality::Many).unwrap();
            tx.commit().unwrap();
            if options & 8 != 0 {
                let mut tx = world.begin();
                tx.migrate_attribute(&renamed, MigrationOptions { value_type: Some(ValueType::Str), ..MigrationOptions::default() }).unwrap();
                tx.commit().unwrap();
            }
            if options & 16 != 0 {
                let mut tx = world.begin();
                tx.deprecate_attribute(&renamed).unwrap();
                tx.commit().unwrap();
            }
        }
        drop(world);
        roundtrip(&path, dir.path());
    }

    #[test]
    fn finite_f64_printer_roundtrips_identical_bits(
        numbers in prop::collection::vec(any::<u64>().prop_filter_map("finite f64", |bits| {
            let number = f64::from_bits(bits);
            number.is_finite().then_some(number)
        }), 1..20)
    ) {
        use kala_core::{AttrDef, Cardinality, Kind, Schema, Tick, Value, ValueType, World};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("world.kala");
        let mut world = World::create(&path, Schema::new().node_type("NUMBER")
            .attribute(AttrDef::new("value", ValueType::F64, Kind::State, Cardinality::Many))).unwrap();
        let entity = world.create_entity("NUMBER", None).unwrap();
        let mut tx = world.begin();
        for (i, number) in numbers.into_iter().enumerate() {
            tx.assert(entity, "value", Value::F64(number), Some(Tick(i as i64))).unwrap();
        }
        tx.commit().unwrap();
        drop(world);
        roundtrip(&path, dir.path());
    }
}

#[test]
fn native_names_handles_infinities_and_genesis_settings_roundtrip() {
    use kala_core::{AttrDef, Cardinality, Kind, Schema, Tick, Value, ValueType, World};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(
        &path,
        Schema::new()
            .node_type("A B[🐉]")
            .attribute(AttrDef::new(
                "name \"with\" punctuation",
                ValueType::Str,
                Kind::State,
                Cardinality::One,
            ))
            .attribute(AttrDef::new(
                "fraction",
                ValueType::F64,
                Kind::State,
                Cardinality::Many,
            ))
            .attribute(
                AttrDef::new("links, to", ValueType::Ref, Kind::State, Cardinality::Many)
                    .inverse("from; here"),
            )
            .setting("schema", "strict")
            .setting("tick_unit", "one moon"),
    )
    .unwrap();
    let first = world
        .create_entity("A B[🐉]", Some("handle odd; \"🧙\""))
        .unwrap();
    let second = world.create_entity("A B[🐉]", None).unwrap();
    let mut tx = world.begin();
    tx.assert(
        first,
        "name \"with\" punctuation",
        Value::Str("Ann".into()),
        Some(Tick(-1)),
    )
    .unwrap();
    tx.assert(first, "fraction", Value::F64(f64::INFINITY), Some(Tick(0)))
        .unwrap();
    tx.assert(
        first,
        "fraction",
        Value::F64(f64::NEG_INFINITY),
        Some(Tick(1)),
    )
    .unwrap();
    tx.assert(first, "links, to", Value::Ref(second), Some(Tick(2)))
        .unwrap();
    tx.commit().unwrap();
    drop(world);
    roundtrip(&path, dir.path());
}

#[test]
fn inferred_only_declarations_roundtrip_before_explicit_confirmation() {
    use kala_core::{Schema, Value, World};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(&path, Schema::new()).unwrap();
    let mut tx = world.begin();
    tx.infer_node_type("FACTION").unwrap();
    tx.infer_attribute("age", &Value::I64(42)).unwrap();
    tx.commit().unwrap();
    let mut tx = world.begin();
    tx.declare_node_type("FACTION").unwrap();
    tx.commit().unwrap();
    drop(world);
    roundtrip(&path, dir.path());
}

#[test]
fn every_escaped_bmp_character_and_supplementary_control_roundtrips() {
    use kala_core::{AttrDef, Cardinality, Kind, Schema, Tick, Value, ValueType, World};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(
        &path,
        Schema::new().node_type("CHARACTER").attribute(
            AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed(),
        ),
    )
    .unwrap();
    let entity = world.create_entity("CHARACTER", Some("ann")).unwrap();
    let mut text: String = (0..=0xffff)
        .filter_map(char::from_u32)
        .filter(|c| kala_core::must_escape(*c))
        .collect();
    text.push_str("\u{e0001}\u{e007f}\u{10ffff}");
    world
        .assert(entity, "bio", Value::Str(text), Some(Tick(0)))
        .unwrap();
    drop(world);
    roundtrip(&path, dir.path());
}

#[cfg(unix)]
#[test]
fn all_dump_tightens_an_existing_empty_directory_before_writing() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("server");
    fs::create_dir(&source).unwrap();
    success(&kala([
        "user",
        "add",
        source.to_str().unwrap(),
        "alice@localhost",
        "--trust",
    ]));
    let out = dir.path().join("backup");
    fs::create_dir(&out).unwrap();
    fs::set_permissions(&out, fs::Permissions::from_mode(0o777)).unwrap();
    let output = kala([
        "dump",
        "--all",
        source.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    success(&output);
    assert_eq!(
        fs::metadata(&out).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(out.join("system.dump.edn"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn forcing_dump_output_onto_source_world_is_refused_without_changing_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    load(&path);
    let before = fs::read(&path).unwrap();
    let output = kala([
        "dump",
        path.to_str().unwrap(),
        "-o",
        path.to_str().unwrap(),
        "--force",
    ]);
    assert_ne!(code(&output), 0, "dump must never replace its input world");
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_node_type_names_outside_bare_id_alphabet_roundtrip() {
    use kala_core::{AttrDef, Cardinality, Kind, Schema, Tick, Value, ValueType, World};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let names = ["FACTION-TYPE", "123", "é", "?question"];
    let mut schema = Schema::new()
        .node_type("START")
        .attribute(AttrDef::new(
            "links",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "number",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ));
    for name in names {
        schema = schema.node_type(name);
    }
    let mut world = World::create(&path, schema).unwrap();
    let start = world.create_entity("START", None).unwrap();
    let mut tx = world.begin();
    for (i, name) in names.into_iter().enumerate() {
        let other = tx.create_entity(name, None).unwrap();
        tx.assert(start, "links", Value::Ref(other), Some(Tick(0)))
            .unwrap();
        tx.assert(other, "number", Value::I64(i as i64), Some(Tick(0)))
            .unwrap();
    }
    tx.commit().unwrap();
    drop(world);
    roundtrip(&path, dir.path());
}

#[test]
fn native_removal_of_reverse_cardinality_roundtrips() {
    use kala_core::{AttrDef, Cardinality, Kind, Schema, ValueType, World};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut def = AttrDef::new("rules", ValueType::Ref, Kind::State, Cardinality::Many);
    def.reverse_cardinality = Some(Cardinality::One);
    let mut world = World::create(&path, Schema::new().attribute(def.clone())).unwrap();
    def.reverse_cardinality = None;
    let mut tx = world.begin();
    tx.declare_attribute(def).unwrap();
    tx.commit().unwrap();
    drop(world);
    roundtrip(&path, dir.path());
}

#[test]
fn bidirectional_reference_corrections_track_the_chains_current_subject() {
    use kala_core::{
        AttrDef, Cardinality, Correction, Kind, Precision, Schema, Tick, Value, ValueType, World,
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut world = World::create(
        &path,
        Schema::new().node_type("CHARACTER").attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        ),
    )
    .unwrap();
    let low = world.create_entity("CHARACTER", Some("low")).unwrap();
    let middle = world.create_entity("CHARACTER", Some("middle")).unwrap();
    let high = world.create_entity("CHARACTER", Some("high")).unwrap();
    let base = world
        .assert(middle, "ally_of", Value::Ref(high), Some(Tick(-100)))
        .unwrap()
        .entries[0];
    // The first correction normalizes (middle, low) into (low, middle).
    world
        .correct(
            base,
            Correction::Replace {
                value: Some(Value::Ref(low)),
                tick: None,
                op: None,
                precision: None,
            },
        )
        .unwrap();
    assert!(
        world
            .as_of_value(low, "ally_of", &Value::Ref(middle), Tick(-100))
            .unwrap()
    );
    // The next correction starts from the revised subject low, not the base's
    // subject middle. Keeping only the original subject loses this distinction.
    world
        .correct(
            base,
            Correction::Replace {
                value: Some(Value::Ref(high)),
                tick: None,
                op: None,
                precision: None,
            },
        )
        .unwrap();
    world
        .correct(
            base,
            Correction::Replace {
                value: None,
                tick: None,
                op: None,
                precision: Some(Precision::Year),
            },
        )
        .unwrap();
    assert!(
        world
            .as_of_value(low, "ally_of", &Value::Ref(high), Tick(-100))
            .unwrap()
    );
    assert!(
        !world
            .as_of_value(middle, "ally_of", &Value::Ref(high), Tick(-100))
            .unwrap()
    );
    drop(world);
    roundtrip(&path, dir.path());
}

#[cfg(unix)]
#[test]
fn all_dump_refuses_output_symlink_with_trailing_separator_or_dot() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("server");
    fs::create_dir(&source).unwrap();
    load(&source.join("world.kala"));
    let mut violations = Vec::new();
    for (index, suffix) in ["/", "/."].into_iter().enumerate() {
        let output_dir = dir.path().join(format!("output{index}"));
        fs::create_dir(&output_dir).unwrap();
        let alias = dir.path().join(format!("alias{index}"));
        symlink(&output_dir, &alias).unwrap();
        let spelling = format!("{}{suffix}", alias.display());
        let output = kala(["dump", "--all", source.to_str().unwrap(), "-o", &spelling]);
        let files = fs::read_dir(&output_dir).unwrap().count();
        if code(&output) == 0 || files != 0 {
            violations.push(format!(
                "{suffix:?}: exit {}, created {files} files",
                code(&output)
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "symlink destinations were followed: {violations:?}"
    );
}
