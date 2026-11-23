//! The CLI migration report says when an inverse name or reverse cardinality was dropped.
mod common;

use common::*;
use std::fs;

#[test]
fn migration_report_names_the_dropped_inverse_and_reverse() {
    let f = varrow();
    let input = f.dir.path().join("w.edn");
    fs::write(
        &input,
        "[[:attribute :parent_of :ref :many :inverse :child_of :reverse :one] \
          [:assert :edwin :parent_of :tomas :at 0]]",
    )
    .unwrap();
    let o = f.run("transact", &[input.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    fs::write(&input, "[:migrate :parent_of :bidirectional]").unwrap();
    let o = f.run("transact", &[input.to_str().unwrap()]);
    assert_eq!(code(&o), 0, "must succeed: {}", stderr(&o));
    let out = stdout(&o);
    let dropped: Vec<&str> = out.lines().filter(|l| l.contains("dropped")).collect();
    assert!(
        !dropped.is_empty(),
        "no line says something was dropped: {out}"
    );
    let line = dropped.join("\n");
    assert!(line.contains("child_of"), "{out}");
    assert!(line.contains("reverse"), "{out}");
}
