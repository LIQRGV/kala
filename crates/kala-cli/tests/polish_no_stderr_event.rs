//! A schema commit through the CLI writes nothing but errors to stderr.
mod common;

use common::*;
use std::fs;

fn transact(f: &Fx, source: &str) -> std::process::Output {
    let input = f.dir.path().join("w.edn");
    fs::write(&input, source).unwrap();
    f.run("transact", &[input.to_str().unwrap()])
}

#[test]
fn schema_commit_prints_no_event_line_on_stderr() {
    let f = varrow();
    for source in [
        "[:attribute :title :string]",
        "[:assert :edwin :age 42 :at 0]",
        "[:node-type :FACTION]",
    ] {
        let o = transact(&f, source);
        assert_eq!(code(&o), 0, "{source}: {}", stderr(&o));
        let err = stderr(&o);
        assert!(
            !err.lines().any(|l| l.contains("event=")),
            "{source}: stderr must not carry diagnostics: {err:?}"
        );
        assert!(err.trim().is_empty(), "{source}: {err:?}");
    }
}
