//! `kala transact` refuses `_KALA_` names and never turns a user world into a system world.
mod common;

use common::*;
use std::fs;

fn transact(f: &Fx, source: &str) -> std::process::Output {
    let input = f.dir.path().join("w.edn");
    fs::write(&input, source).unwrap();
    f.run("transact", &[input.to_str().unwrap()])
}

fn refused(source: &str) {
    let f = varrow();
    let before = fs::read(&f.path).unwrap();
    let o = transact(&f, source);
    assert_ne!(code(&o), 0, "{source}: {}", stdout(&o));
    let err = stderr(&o);
    assert!(err.contains("_KALA_"), "{source}: {err}");
    assert!(err.to_lowercase().contains("reserved"), "{source}: {err}");
    assert_eq!(fs::read(&f.path).unwrap(), before, "{source}");
    // The world did not become a system world: ordinary writes still work.
    let o = transact(&f, "[:assert :edwin :age 42 :at 0]");
    assert_eq!(code(&o), 0, "{}", stderr(&o));
}

#[test]
fn create_with_reserved_node_type_is_refused() {
    refused("[:create :_KALA_SYSTEM :x]");
}

#[test]
fn node_type_form_with_reserved_name_is_refused() {
    refused("[:node-type :_KALA_SYSTEM]");
}

#[test]
fn attribute_form_with_reserved_name_is_refused() {
    refused("[:attribute :_KALA_x :string]");
}

#[test]
fn assert_inferring_reserved_attribute_is_refused() {
    refused("[:assert :edwin :_KALA_foo 1]");
}

#[test]
fn rename_to_reserved_name_is_refused() {
    refused("[:rename :name :_KALA_x]");
}
