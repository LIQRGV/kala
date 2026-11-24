//! Slice 15b, stage 4: the write commands' concurrency surface, through the
//! real binary: `--concurrency`, `--busy-timeout`, `.concurrency`, the
//! `[:concurrency …]` form and what `kala dump` makes of it.
//!
//! A real conflict needs another commit between a form's begin and its
//! commit, which two processes can't be made to do reliably; the exit code
//! and message of a conflict are tested in-process, in
//! `src/cmd/transact.rs`.

mod common;
use common::*;
use serde_json::Value as Json;
use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const WORLD: &str = r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: located_in, type: ref }
    - { name: age, type: i64 }
    - { name: tag, type: string, cardinality: many }
entities:
  CHARACTER: [greg, edwin]
  PLACE: [varrow, ashford, carnath]
transactions:
  - tick: 600
    ops:
      - assert: [greg, located_in, varrow]
"#;

fn transact(f: &Fx, text: &str, options: &[&str]) -> Output {
    let input = f.dir.path().join("writes.edn");
    fs::write(&input, text).unwrap();
    let mut args = vec![input.to_str().unwrap()];
    args.extend_from_slice(options);
    f.run("transact", &args)
}

fn repl(f: &Fx, text: &str, flags: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kala"))
        .arg("repl")
        .arg(&f.path)
        .args(flags)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    feed(&mut child, text.as_bytes());
    child.wait_with_output().unwrap()
}

#[test]
fn transact_takes_a_default_concurrency_mode() {
    let f = build(WORLD);
    for (tick, options) in [
        (601, &[][..]),
        (602, &["--concurrency", "optimistic"][..]),
        (603, &["--concurrency", "weak"][..]),
        (604, &["--concurrency=weak", "--json"][..]),
    ] {
        let o = transact(
            &f,
            &format!(
                "[:set :greg :located_in :ashford :at {tick}]\n[:assert :greg :age {tick} :at {tick}]"
            ),
            options,
        );
        assert_eq!(code(&o), 0, "{options:?}: {}", stderr(&o));
    }
    for bad in ["pessimistic", "banana", ""] {
        let o = transact(&f, "[:assert :greg :age 1]", &["--concurrency", bad]);
        assert_eq!(code(&o), 2, "{bad}: {}", stderr(&o));
        assert!(
            stderr(&o).contains("optimistic") && stderr(&o).contains("weak"),
            "the usage error lists the modes: {}",
            stderr(&o)
        );
    }
    // A restore replays alone into a private file: no default to choose.
    let dump = f.dir.path().join("w.dump.edn");
    assert_eq!(code(&f.run("dump", &["-o", dump.to_str().unwrap()])), 0);
    let o = kala([
        "transact",
        "--create",
        f.dir.path().join("new.kala").to_str().unwrap(),
        dump.to_str().unwrap(),
        "--concurrency",
        "weak",
    ]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
}

#[test]
fn a_form_names_its_own_mode_and_is_not_an_op() {
    let f = build(WORLD);
    // The form's own mode wins over the flag either way.
    for (tick, form_mode, flag) in [
        (601, "weak", "optimistic"),
        (602, "optimistic", "weak"),
        (603, "weak", "weak"),
    ] {
        let o = transact(
            &f,
            &format!("[[:concurrency :{form_mode}] [:assert :greg :age {tick} :at {tick}]]"),
            &["--concurrency", flag, "--json"],
        );
        assert_eq!(code(&o), 0, "{}", stderr(&o));
        let doc: Json = serde_json::from_str(&stdout(&o)).unwrap();
        assert_eq!(doc["changed"], 1);
        assert_eq!(doc["ops"].as_array().unwrap().len(), 1);
        assert_eq!(doc["ops"][0]["position"], 1);
        assert_eq!(doc["ops"][0]["kind"], "assert");
    }
    // Text: the op is still `op 1`.
    let o = transact(
        &f,
        "[:at 700 [:concurrency :weak] [:assert :greg :age 7]]",
        &[],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(
        stdout(&o).contains("  op 1 (:assert): tick 700, 1 changed"),
        "{}",
        stdout(&o)
    );

    for (text, want) in [
        (
            "[[:assert :greg :age 9 :at 800] [:concurrency :weak]]",
            "[:concurrency …] must be the first op of a transaction, once",
        ),
        (
            "[[:concurrency :pessimistic] [:assert :greg :age 9 :at 800]]",
            "concurrency mode :pessimistic is not supported yet; expected :weak or :optimistic",
        ),
        (
            "[[:concurrency :banana] [:assert :greg :age 9 :at 800]]",
            "unknown concurrency mode :banana; expected :weak or :optimistic",
        ),
        (
            "[:concurrency :weak]",
            "[:concurrency …] is not a transaction by itself",
        ),
    ] {
        let before = fs::read(&f.path).unwrap();
        let o = transact(&f, text, &[]);
        assert_eq!(code(&o), 1, "{text}: {}", stderr(&o));
        assert!(stderr(&o).contains(want), "{text}: {}", stderr(&o));
        assert_eq!(fs::read(&f.path).unwrap(), before, "{text}");
    }
    // A dry run accepts the form too.
    let o = transact(
        &f,
        "[[:concurrency :weak] [:assert :greg :age 9 :at 800]]",
        &["--dry-run"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
}

#[test]
fn busy_timeout_takes_a_duration() {
    let f = build(WORLD);
    for (i, good) in ["0", "3", "250ms", "5s", "2m", "0ms", "60m"]
        .iter()
        .enumerate()
    {
        let o = transact(
            &f,
            &format!("[:assert :greg :age {i} :at {}]", 700 + i),
            &["--busy-timeout", good],
        );
        assert_eq!(code(&o), 0, "{good}: {}", stderr(&o));
    }
    for bad in [
        "soon",
        "-1",
        "5 s",
        "1.5s",
        "5h",
        "61m",
        "3601",
        "ms",
        "",
        "9999999999999999999999s",
    ] {
        let before = fs::read(&f.path).unwrap();
        let o = transact(
            &f,
            "[:assert :greg :age 99]",
            &[&format!("--busy-timeout={bad}")],
        );
        assert_eq!(code(&o), 2, "`{bad}`: {}", stderr(&o));
        assert!(
            stderr(&o).contains("expected a duration such as `500ms`, `5s` or `2m`"),
            "`{bad}`: {}",
            stderr(&o)
        );
        assert_eq!(fs::read(&f.path).unwrap(), before);
    }
}

/// Every command that writes a world file takes the flag.
#[test]
fn every_writing_command_takes_busy_timeout() {
    let f = build(WORLD);
    let dir = f.dir.path();
    let path = |name: &str| dir.join(name).to_str().unwrap().to_owned();
    let fixture = path("f.yaml");
    fs::write(&fixture, WORLD).unwrap();
    let dump = path("w.dump.edn");
    assert_eq!(code(&f.run("dump", &["-o", &dump])), 0);
    let accounts = dir.join("server");
    fs::create_dir(&accounts).unwrap();
    let accounts = accounts.to_str().unwrap().to_owned();
    for args in [
        vec!["create", &path("created.kala"), "--busy-timeout", "1s"],
        vec![
            "load",
            &fixture,
            &path("loaded.kala"),
            "--busy-timeout",
            "1s",
        ],
        vec![
            "load",
            &fixture,
            &path("loaded.kala"),
            "--force",
            "--busy-timeout",
            "0",
        ],
        vec![
            "transact",
            "--create",
            &path("restored.kala"),
            &dump,
            "--busy-timeout",
            "1s",
        ],
        vec![
            "user",
            "grant",
            &accounts,
            "root@localhost",
            "read",
            "story",
            "--busy-timeout",
            "1s",
        ],
        vec![
            "user",
            "revoke",
            &accounts,
            "root@localhost",
            "read",
            "story",
            "--busy-timeout=250ms",
        ],
        vec![
            "user",
            "add",
            &accounts,
            "local@localhost",
            "--trust",
            "--busy-timeout",
            "0",
        ],
        vec![
            "user",
            "set",
            &accounts,
            "local@localhost",
            "--trust",
            "--busy-timeout",
            "0",
        ],
        vec![
            "user",
            "drop",
            &accounts,
            "local@localhost",
            "--busy-timeout",
            "0",
        ],
    ] {
        let o = kala(&args);
        assert_eq!(code(&o), 0, "{args:?}: {}", stderr(&o));
    }
    let o = repl(
        &f,
        "[:assert :greg :age 5 :at 900]\n",
        &["--write", "--busy-timeout", "1s"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    // Commands that never take the writer lock don't have it.
    for args in [
        vec!["repl", f.path.to_str().unwrap(), "--busy-timeout", "1s"],
        vec![
            "asof",
            f.path.to_str().unwrap(),
            "greg",
            "--busy-timeout",
            "1s",
        ],
        vec!["user", "list", &accounts, "--busy-timeout", "1s"],
    ] {
        let o = kala(&args);
        assert_eq!(code(&o), 2, "{args:?}: {}", stderr(&o));
    }
    // A bad duration on `kala user` is a usage error that echoes no value.
    let o = kala([
        "user",
        "grant",
        &accounts,
        "root@localhost",
        "read",
        "story",
        "--busy-timeout",
        "soon",
    ]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    assert!(!stderr(&o).contains("soon"), "{}", stderr(&o));
}

/// With the lock held elsewhere (a commit in progress, here the lock
/// itself), `0` gives up at once and a short timeout after about that long.
#[test]
fn a_held_lock_is_exit_three_after_the_busy_timeout() {
    let f = build(WORLD);
    let before = fs::read(&f.path).unwrap();
    let held = fs::File::open(&f.path).unwrap();
    held.try_lock().expect("the lock is free");
    for (flag, at_least, at_most) in [
        ("0", Duration::ZERO, Duration::from_millis(1500)),
        (
            "400ms",
            Duration::from_millis(400),
            Duration::from_millis(3000),
        ),
        ("1", Duration::from_secs(1), Duration::from_millis(4000)),
    ] {
        let started = Instant::now();
        let o = transact(&f, "[:assert :greg :age 18]", &["--busy-timeout", flag]);
        let took = started.elapsed();
        assert_eq!(code(&o), 3, "{flag}: {}", stderr(&o));
        let err = stderr(&o);
        assert!(
            err.contains("world is locked by another writer")
                && err.contains("nothing was written")
                && err.contains("--busy-timeout"),
            "{flag}: {err}"
        );
        assert!(took >= at_least, "{flag}: gave up after {took:?}");
        // Generous: process start-up is in it, the default 5 s is not.
        assert!(took < at_most, "{flag}: waited {took:?}");
        assert_eq!(fs::read(&f.path).unwrap(), before);
    }
    // The REPL's open waits the same way.
    let started = Instant::now();
    let o = repl(
        &f,
        "[:assert :greg :age 18]\n",
        &["--write", "--busy-timeout", "0"],
    );
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(started.elapsed() < Duration::from_millis(1500));
    // `kala load --force` must not replace a world mid-commit.
    let fixture = f.dir.path().join("f.yaml");
    fs::write(&fixture, WORLD).unwrap();
    let started = Instant::now();
    let o = kala([
        "load",
        fixture.to_str().unwrap(),
        f.path.to_str().unwrap(),
        "--force",
        "--busy-timeout",
        "200ms",
    ]);
    assert_eq!(code(&o), 1, "{}", stderr(&o));
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert!(started.elapsed() < Duration::from_millis(3000));
    assert_eq!(fs::read(&f.path).unwrap(), before);
    drop(held);
    let o = transact(&f, "[:assert :greg :age 18]", &["--busy-timeout", "0"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
}

/// `kala user` waits for `system.kala`'s lock as long as `--busy-timeout`
/// says, not kala-core's default 5 s.
#[test]
fn kala_user_waits_for_the_system_lock_as_long_as_asked() {
    let dir = tempfile::tempdir().unwrap();
    let server = dir.path().to_str().unwrap();
    let o = kala(["user", "list", server]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let system = dir.path().join("system.kala");
    let before = fs::read(&system).unwrap();
    let held = fs::File::open(&system).unwrap();
    held.try_lock().expect("the lock is free");
    let started = Instant::now();
    let o = kala([
        "user",
        "grant",
        server,
        "root@localhost",
        "read",
        "story",
        "--busy-timeout",
        "300ms",
    ]);
    let took = started.elapsed();
    assert_ne!(code(&o), 0, "{}", stdout(&o));
    assert!(
        stderr(&o).contains("being written by another writer"),
        "{}",
        stderr(&o)
    );
    assert!(took >= Duration::from_millis(300), "gave up after {took:?}");
    assert!(took < Duration::from_millis(3000), "waited {took:?}");
    assert_eq!(fs::read(&system).unwrap(), before);
}

#[test]
fn the_write_repl_has_a_concurrency_setting() {
    let f = build(WORLD);
    let o = repl(
        &f,
        ".concurrency\n.concurrency weak\n.concurrency\n\
         [:set :greg :located_in :ashford :at 601]\n\
         .concurrency optimistic\n.concurrency\n\
         [[:concurrency :weak] [:assert :greg :age 3 :at 602]]\n.concurrency\n",
        &["--write"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    let settings: Vec<&str> = out
        .lines()
        .filter(|l| l.starts_with("concurrency: "))
        .collect();
    assert_eq!(
        settings,
        [
            "concurrency: optimistic",
            "concurrency: weak",
            // A form's own mode is for that form only.
            "concurrency: optimistic",
            "concurrency: optimistic",
        ],
        "{out}"
    );
    assert!(out.contains("form 1: committed tx#"), "{out}");

    // The flag sets the session's starting value.
    let o = repl(&f, ".concurrency\n", &["--write", "--concurrency", "weak"]);
    assert_eq!(
        (code(&o), stdout(&o).as_str()),
        (0, "concurrency: weak\n"),
        "{}",
        stderr(&o)
    );

    for (bad, want) in [
        (
            ".concurrency banana\n",
            "usage: .concurrency [optimistic|weak]",
        ),
        (
            ".concurrency pessimistic\n",
            "usage: .concurrency [optimistic|weak]",
        ),
        (
            ".concurrency weak weak\n",
            "usage: .concurrency [optimistic|weak]",
        ),
    ] {
        let o = repl(&f, bad, &["--write"]);
        assert_eq!(code(&o), 1, "{bad}");
        assert!(stderr(&o).contains(want), "{bad}: {}", stderr(&o));
        assert_eq!(stdout(&o), "", "{bad}");
    }
    // A read-only session has no commits to set a mode for.
    let o = repl(&f, ".concurrency weak\n", &[]);
    assert_eq!(code(&o), 1);
    assert!(
        stderr(&o).contains(".concurrency needs a write session; start with --write"),
        "{}",
        stderr(&o)
    );
    let o = repl(&f, ".quit\n", &["--concurrency", "weak"]);
    assert_eq!(code(&o), 2, "--concurrency needs --write: {}", stderr(&o));

    for flags in [&[][..], &["--write"][..]] {
        let o = repl(&f, ".help\n", flags);
        assert_eq!(code(&o), 0, "{}", stderr(&o));
        assert!(
            stdout(&o).contains(".concurrency [optimistic|weak]"),
            "{}",
            stdout(&o)
        );
    }
}

fn dump(world: &Path) -> String {
    let o = kala(["dump", world.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    stdout(&o)
}

#[test]
fn dump_never_emits_the_mode_and_restores_identically() {
    let f = build(WORLD);
    let o = transact(
        &f,
        "[[:concurrency :weak] [:set :greg :located_in :ashford :at 601] [:create :CHARACTER :kael]]
         [:at 700 [:concurrency :optimistic] [:assert :kael :located_in :varrow] [:assert :kael :tag \"weak\"]]",
        &["--concurrency", "weak", "--json"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let first = serde_json::Deserializer::from_slice(&o.stdout)
        .into_iter::<Json>()
        .next()
        .unwrap()
        .unwrap();
    let entry = first["entries"][0].as_u64().unwrap();
    let o = transact(
        &f,
        &format!("[[:concurrency :weak] [:correct {entry} :replace :at 599]]"),
        &[],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = repl(
        &f,
        ".concurrency weak\n[:assert :greg :tag \"repl\" :at 710]\n",
        &["--write"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let text = dump(&f.path);
    assert!(!text.contains("concurrency"), "{text}");
    assert!(text.contains(";; tx#4"), "{text}");
    let restored = f.dir.path().join("restored.kala");
    let dump_file = f.dir.path().join("w.dump.edn");
    fs::write(&dump_file, &text).unwrap();
    let o = kala([
        "transact",
        "--create",
        restored.to_str().unwrap(),
        dump_file.to_str().unwrap(),
    ]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(dump(&restored), text);
    assert_eq!(
        fs::read(&restored).unwrap(),
        fs::read(&f.path).unwrap(),
        "the restored log is byte-identical"
    );
    let o = kala(["doctor", restored.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stdout(&o));
}

/// Two processes, each with its own default mode, writing one world: both
/// modes take the same lock and lose nothing (blind writes never conflict).
#[test]
fn weak_and_optimistic_writers_share_a_world() {
    let f = build(WORLD);
    let forms = |who: &str| -> String {
        (0..15)
            .map(|i| format!("[:assert :greg :tag \"{who}{i}\" :at {}]\n", 700 + i))
            .collect()
    };
    let spawn = |who: &str, mode: &str| {
        let input = f.dir.path().join(format!("{who}.edn"));
        fs::write(&input, forms(who)).unwrap();
        Command::new(env!("CARGO_BIN_EXE_kala"))
            .arg("transact")
            .arg(&f.path)
            .arg(&input)
            .args(["--concurrency", mode, "--busy-timeout", "30s"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let (a, b) = (spawn("a", "weak"), spawn("b", "optimistic"));
    for child in [a, b] {
        let o = child.wait_with_output().unwrap();
        assert_eq!(code(&o), 0, "{}", stderr(&o));
        assert_eq!(
            stdout(&o).matches(": committed tx#").count(),
            15,
            "{}",
            stdout(&o)
        );
    }
    let o = f.run("doctor", &[]);
    assert_eq!(code(&o), 0, "{}", stdout(&o));
    let doc = f.json("asof", &["greg", "tag", "@800"]);
    let held = doc.to_string();
    for who in ["a", "b"] {
        for i in 0..15 {
            assert!(held.contains(&format!("\"{who}{i}\"")), "{who}{i}: {held}");
        }
    }
    // Every form is its own transaction: 30 after the fixture's two.
    assert_eq!(doc["canon"], 32, "{held}");
}
