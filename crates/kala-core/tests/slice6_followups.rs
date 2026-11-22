//! Slice 6 item 00 (slice-5 follow-ups), RED tests for kala-core.
//!
//! (b) Reading an entity at a canon before it was defined is
//!     `Error::UnknownEntity` (as edges already are); at or after its
//!     defining tx it reads normally. Reverses the slice-5 rule "reads as
//!     empty".
//! (c) L1: error `Display` escapes the extra "must escape" characters
//!     (U+061C, U+200E, U+200F, U+2028, U+2029, U+FEFF) as `\u{..}`. The
//!     predicate itself is tested in `slice6_must_escape.rs`.
//! (d) L3: log readers refuse non-regular files (`Error::Io` with kind
//!     `InvalidInput`) and never hang or allocate unboundedly.

use kala_core::*;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
}

fn s(x: &str) -> Value {
    Value::Str(x.to_owned())
}

// ---------------------------------------------------------------------------
// (b) entities read at a canon before their definition
// ---------------------------------------------------------------------------

struct Late {
    w: World,
    varrow: EntityId,
    late: EntityId,
    /// The latest tx before `late` was defined.
    before: TxId,
    /// The tx that defines `late`.
    def_tx: TxId,
}

fn late_world() -> Late {
    let mut w = World::in_memory(schema()).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let before = w.latest_tx();
    let late = w.create_entity("CHARACTER", Some("late")).unwrap();
    let def_tx = w.latest_tx();
    assert!(def_tx > before, "defining an entity is its own tx");
    w.assert(late, "located_in", Value::Ref(varrow), Some(Tick(5)))
        .unwrap();
    w.assert(late, "name", s("Late"), Some(Tick(5))).unwrap();
    Late {
        w,
        varrow,
        late,
        before,
        def_tx,
    }
}

fn each_view<R>(f: &Late, canon: TxId, mut check: impl FnMut(&View<'_>) -> R) -> Vec<R> {
    vec![
        check(&f.w.at_canon(canon)),
        check(&f.w.at_canon_main(canon)),
    ]
}

#[test]
fn as_of_before_the_entity_was_defined_is_unknown_entity() {
    let f = late_world();
    for r in each_view(&f, f.before, |v| v.as_of(f.late, "located_in", Tick(5))) {
        assert_eq!(r, Err(Error::UnknownEntity(f.late)));
    }
}

#[test]
fn as_of_value_before_the_entity_was_defined_is_unknown_entity() {
    let f = late_world();
    for r in each_view(&f, f.before, |v| {
        v.as_of_value(f.late, "name", &s("Late"), Tick(5))
    }) {
        assert_eq!(r, Err(Error::UnknownEntity(f.late)));
    }
}

#[test]
fn as_of_detail_before_the_entity_was_defined_is_unknown_entity() {
    let f = late_world();
    for r in each_view(&f, f.before, |v| v.as_of_detail(f.late, "name", Tick(5))) {
        assert_eq!(r, Err(Error::UnknownEntity(f.late)));
    }
}

#[test]
fn history_before_the_entity_was_defined_is_unknown_entity() {
    let f = late_world();
    for r in each_view(&f, f.before, |v| v.history(f.late, "located_in")) {
        assert_eq!(r, Err(Error::UnknownEntity(f.late)));
    }
}

#[test]
fn neighbors_before_the_entity_was_defined_is_unknown_entity() {
    let f = late_world();
    for d in [Direction::Out, Direction::In, Direction::Both] {
        for r in each_view(&f, f.before, |v| {
            v.neighbors(f.late, "located_in", d, Tick(5))
        }) {
            assert_eq!(r, Err(Error::UnknownEntity(f.late)), "{d:?}");
        }
    }
}

#[test]
fn an_entity_defined_earlier_still_reads_at_that_canon() {
    // varrow exists at `before`, so reading it there is fine (and empty:
    // the fact pointing at it is newer than the canon).
    let f = late_world();
    for d in [Direction::Out, Direction::In, Direction::Both] {
        for r in each_view(&f, f.before, |v| {
            v.neighbors(f.varrow, "located_in", d, Tick(5))
        }) {
            assert_eq!(r, Ok(vec![]), "{d:?}");
        }
    }
}

#[test]
fn at_the_defining_tx_the_entity_reads_normally_and_empty() {
    let f = late_world();
    for r in each_view(&f, f.def_tx, |v| v.as_of(f.late, "located_in", Tick(5))) {
        assert_eq!(r, Ok(vec![]));
    }
    for r in each_view(&f, f.def_tx, |v| v.history(f.late, "name")) {
        assert_eq!(r, Ok(vec![]));
    }
    for r in each_view(&f, f.def_tx, |v| {
        v.neighbors(f.late, "located_in", Direction::Both, Tick(5))
    }) {
        assert_eq!(r, Ok(vec![]));
    }
    for r in each_view(&f, f.def_tx, |v| {
        v.as_of_value(f.late, "name", &s("Late"), Tick(5))
    }) {
        assert_eq!(r, Ok(false));
    }
    for r in each_view(&f, f.def_tx, |v| v.as_of_detail(f.late, "name", Tick(5))) {
        assert_eq!(r, Ok(vec![]));
    }
}

#[test]
fn after_the_defining_tx_the_facts_show_up_at_their_own_canon() {
    let f = late_world();
    let latest = f.w.latest_tx();
    assert!(latest > f.def_tx);
    assert_eq!(
        f.w.at_canon(latest).as_of(f.late, "name", Tick(5)),
        Ok(vec![s("Late")])
    );
    assert_eq!(
        f.w.as_of(f.late, "located_in", Tick(5)),
        Ok(vec![Value::Ref(f.varrow)])
    );
    assert_eq!(
        f.w.at_canon(latest)
            .neighbors(f.late, "located_in", Direction::Out, Tick(5))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn canon_zero_and_every_canon_before_the_define_are_unknown_entity() {
    let f = late_world();
    for n in 0..=f.before.0 {
        let v = f.w.at_canon(TxId(n));
        assert_eq!(
            v.as_of(f.late, "name", Tick(5)),
            Err(Error::UnknownEntity(f.late)),
            "canon {n}"
        );
    }
}

// ---------------------------------------------------------------------------
// (c) error Display escapes the extra characters
// ---------------------------------------------------------------------------

const EXTRA: [(char, &str); 6] = [
    ('\u{061C}', "\\u{61c}"),
    ('\u{200E}', "\\u{200e}"),
    ('\u{200F}', "\\u{200f}"),
    ('\u{2028}', "\\u{2028}"),
    ('\u{2029}', "\\u{2029}"),
    ('\u{FEFF}', "\\u{feff}"),
];

#[test]
fn unknown_attribute_echo_escapes_the_extra_chars() {
    let f = late_world();
    for (c, want) in EXTRA {
        let attr = format!("a{c}b");
        let err = f.w.as_of(f.late, &attr, Tick(0)).unwrap_err();
        let text = err.to_string();
        assert!(!text.contains(c), "raw U+{:04X} in {text:?}", u32::from(c));
        assert!(text.contains(want), "{want} missing in {text:?}");
        // the payload keeps the raw string
        assert_eq!(err, Error::UnknownAttribute(attr));
    }
}

#[test]
fn handle_and_node_type_echo_escape_the_extra_chars() {
    let mut w = World::in_memory(schema()).unwrap();
    for (c, want) in EXTRA {
        let name = format!("x{c}y");
        let text = w.create_entity(&name, None).unwrap_err().to_string();
        assert!(!text.contains(c), "node type: {text:?}");
        assert!(text.contains(want), "node type: {want} in {text:?}");
        w.create_entity("CHARACTER", Some(&name)).unwrap();
        let text = w
            .create_entity("CHARACTER", Some(&name))
            .unwrap_err()
            .to_string();
        assert!(!text.contains(c), "handle: {text:?}");
        assert!(text.contains(want), "handle: {want} in {text:?}");
    }
}

#[test]
fn existing_escapes_are_unchanged() {
    let f = late_world();
    let text =
        f.w.as_of(f.late, "a\u{1b}b\u{202E}c", Tick(0))
            .unwrap_err()
            .to_string();
    assert!(text.contains("a\\u{1b}b\\u{202e}c"), "{text:?}");
    let text =
        f.w.as_of(f.late, "tab\there", Tick(0))
            .unwrap_err()
            .to_string();
    assert!(text.contains("tab\\there"), "{text:?}");
}

// ---------------------------------------------------------------------------
// (d) log readers refuse non-regular files
// ---------------------------------------------------------------------------

/// Runs `f` on a thread; a hang is a test failure, not a stuck test.
fn within<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(15))
        .unwrap_or_else(|_| panic!("{what}: hung (no answer in 15 s)"))
}

fn assert_refused<T: std::fmt::Debug>(what: &str, r: Result<T, Error>) {
    match r {
        Err(Error::Io {
            kind: std::io::ErrorKind::InvalidInput,
            ..
        }) => {}
        other => panic!("{what}: expected Err(Io {{ kind: InvalidInput }}), got {other:?}"),
    }
}

/// Every log reader on `path`; each must refuse it.
fn all_readers_refuse(path: &Path) {
    let p = path.to_path_buf();
    let results = within("log readers", move || {
        (
            World::open_read_only(&p).map(|_| ()),
            World::open(&p).map(|_| ()),
            kala_core::log::read_log(&p).map(|_| ()),
            doctor_log(&p, &DoctorOptions::default()).map(|_| ()),
        )
    });
    assert_refused("World::open_read_only", results.0);
    assert_refused("World::open", results.1);
    assert_refused("log::read_log", results.2);
    assert_refused("doctor_log", results.3);
}

#[test]
fn a_directory_is_refused_by_every_reader() {
    let dir = tempfile::tempdir().unwrap();
    all_readers_refuse(dir.path());
}

#[test]
fn a_fifo_is_refused_by_every_reader_without_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("pipe");
    match std::process::Command::new("mkfifo").arg(&fifo).status() {
        Ok(st) if st.success() => {}
        _ => {
            eprintln!("skipped: mkfifo unavailable");
            return;
        }
    }
    all_readers_refuse(&fifo);
}

/// `/dev/zero` is an endless file: a reader that slurps it allocates until
/// the process dies. So the check runs in a child process with a memory
/// cap (`ulimit -v`); the child aborts if the reader allocates unboundedly.
#[test]
fn dev_zero_is_refused_not_slurped() {
    const CHILD: &str = "KALA_S6_DEV_ZERO_CHILD";
    let zero = Path::new("/dev/zero");
    if !zero.exists() {
        eprintln!("skipped: no /dev/zero");
        return;
    }
    if std::env::var_os(CHILD).is_some() {
        all_readers_refuse(zero);
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let mut child = match std::process::Command::new("sh")
        .arg("-c")
        .arg("ulimit -v 3000000 && exec \"$0\" --exact dev_zero_is_refused_not_slurped --nocapture")
        .arg(&exe)
        .env(CHILD, "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            eprintln!("skipped: no sh");
            return;
        }
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("child reading /dev/zero did not finish in 60 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        use std::io::Read;
        let _ = e.read_to_string(&mut err);
    }
    assert!(
        status.success(),
        "child failed ({status}); a signal or abort means unbounded allocation:\n{err}"
    );
}

#[test]
fn a_regular_log_still_opens_after_the_change() {
    // Guard against over-refusing: ordinary files keep working.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    drop(World::create(&p, schema()).unwrap());
    assert!(World::open_read_only(&p).is_ok());
    assert!(kala_core::log::read_log(&p).is_ok());
    assert!(doctor_log(&p, &DoctorOptions::default()).is_ok());
    assert!(World::open(&p).is_ok());
}

#[test]
fn a_symlink_to_a_regular_log_still_opens() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    drop(World::create(&p, schema()).unwrap());
    let link = dir.path().join("link.kala");
    if std::os::unix::fs::symlink(&p, &link).is_err() {
        return;
    }
    assert!(World::open_read_only(&link).is_ok());
    assert!(kala_core::log::read_log(&link).is_ok());
}
