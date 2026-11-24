//! Slice 15a review fixes in the write grammar.
use kala_datalog::ast::{Item, WriteOp, WriteValue};
use kala_datalog::parse_write;

fn set_values(source: &str) -> Vec<WriteValue> {
    let tx = parse_write(source).unwrap_or_else(|e| panic!("{source}: {e}"));
    match tx.ops.into_iter().next() {
        Some(WriteOp::Set { values, .. }) => values,
        other => panic!("{source}: expected a set, got {other:?}"),
    }
}

/// `:set` takes every single value `:assert` takes: a vector that is a tagged
/// scalar is ONE value, not a member list.
#[test]
fn set_reads_a_tagged_scalar_as_one_value() {
    let values = set_values("[:set :greg :score [:f64 :inf] :at 300]");
    assert!(
        matches!(values.as_slice(), [WriteValue::Scalar(Item::Float(f, _))] if *f == f64::INFINITY),
        "{values:?}"
    );
    let values = set_values("[:set :greg :score [:f64 :-inf]]");
    assert!(
        matches!(values.as_slice(), [WriteValue::Scalar(Item::Float(f, _))] if *f == f64::NEG_INFINITY),
        "{values:?}"
    );
    let values = set_values("[:set :greg :friend [:id \"CHARACTER#2\"]]");
    assert!(
        matches!(values.as_slice(), [WriteValue::Scalar(Item::Id(id, _))] if id == "CHARACTER#2"),
        "{values:?}"
    );
}

/// Every member of a list is parsed like an `:assert` value, so tagged
/// scalars and edge triples work as members.
#[test]
fn set_member_lists_take_tagged_scalars_and_edge_triples() {
    let values = set_values("[:set :greg :scores [1.5 [:f64 :-inf] [:f64 :inf]]]");
    assert_eq!(values.len(), 3);
    assert!(matches!(&values[1], WriteValue::Scalar(Item::Float(f, _)) if *f == f64::NEG_INFINITY));
    let values = set_values("[:set :greg :friend [:mara [:id \"CHARACTER#2\"]]]");
    assert!(
        matches!(
            values.as_slice(),
            [
                WriteValue::Scalar(Item::Keyword(k, _)),
                WriteValue::Scalar(Item::Id(id, _))
            ] if k == "mara" && id == "CHARACTER#2"
        ),
        "{values:?}"
    );
    // One edge as the single value must be wrapped in a member list...
    let values = set_values("[:set :note :about [[:edwin :ally_of :mara]]]");
    assert!(
        matches!(values.as_slice(), [WriteValue::Edge { attr, .. }] if attr.name == "ally_of"),
        "{values:?}"
    );
    // ...because a bare three-element vector is a list of three members.
    let values = set_values("[:set :note :about [:edwin :ally_of :mara]]");
    assert!(
        matches!(
            values.as_slice(),
            [
                WriteValue::Scalar(Item::Keyword(a, _)),
                WriteValue::Scalar(Item::Keyword(b, _)),
                WriteValue::Scalar(Item::Keyword(c, _))
            ] if a == "edwin" && b == "ally_of" && c == "mara"
        ),
        "{values:?}"
    );
    // A two-element vector that is not a tagged scalar stays a member list.
    assert_eq!(set_values("[:set :greg :friend [:f64 :mara]]").len(), 2);
    assert_eq!(set_values("[:set :greg :alias [\"id\" \"x\"]]").len(), 2);
    assert!(set_values("[:set :greg :alias []]").is_empty());
}

#[test]
fn the_unknown_op_error_lists_set() {
    let error = parse_write("[:sett :greg :age 1]").unwrap_err();
    assert!(
        error.to_string().ends_with(
            "unknown write op :sett; expected :create, :assert, :retract, :set or :correct"
        ),
        "{error}"
    );
}
