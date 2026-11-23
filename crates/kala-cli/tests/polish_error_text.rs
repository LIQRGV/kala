//! Migration refusals print entities as handles and values as `kala asof`
//! prints them, never as Rust Debug output.
mod common;

use common::*;
use std::fs;

fn transact(f: &Fx, source: &str) -> std::process::Output {
    let input = f.dir.path().join("w.edn");
    fs::write(&input, source).unwrap();
    f.run("transact", &[input.to_str().unwrap()])
}

fn write(f: &Fx, source: &str) {
    let o = transact(f, source);
    assert_eq!(code(&o), 0, "{source}: {}", stderr(&o));
}

fn no_debug(err: &str) {
    for fragment in ["EntityId(", "Str(", "I64(", "Ref(", "Bool(", "F64("] {
        assert!(!err.contains(fragment), "{fragment:?} leaked: {err}");
    }
}

#[test]
fn unconvertible_value_is_listed_as_handle_tick_and_quoted_value() {
    let f = varrow();
    write(
        &f,
        "[[:assert :tomas :x \"x\" :at 0] [:assert :edwin :x \"a\\\"b\" :at 3]]",
    );
    // What `kala asof` prints for the same value.
    let shown = stdout(&f.run("asof", &["edwin", "x", "--at", "3"]));
    assert!(shown.contains(r#"x = "a\"b""#), "{shown}");
    let o = transact(&f, "[:migrate :x :i64]");
    assert_eq!(code(&o), 1, "{}", stdout(&o));
    let err = stderr(&o);
    assert!(err.contains("2 unconvertible"), "{err}");
    assert!(err.contains(r#"tomas @0 "x""#), "{err}");
    assert!(err.contains(r#"edwin @3 "a\"b""#), "{err}");
    no_debug(&err);
}

#[test]
fn both_ways_pair_is_listed_with_handles() {
    let f = varrow();
    write(
        &f,
        "[[:assert :edwin :y :greg :at 0] [:assert :greg :y :edwin :at 5] [:assert :tomas :y :osric :at 0]]",
    );
    let o = transact(&f, "[:migrate :y :bidirectional]");
    assert_eq!(code(&o), 1, "{}", stdout(&o));
    let err = stderr(&o);
    assert!(err.contains("edwin") && err.contains("greg"), "{err}");
    assert!(
        !err.contains("osric"),
        "only the offending pair is listed: {err}"
    );
    no_debug(&err);
}
