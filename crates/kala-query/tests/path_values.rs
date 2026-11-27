//! Traversal values have a validated shape and deterministic equality/order.
use kala_core::{EdgeId, EntityId, NodeTypeId};
use kala_query::{Datum, PathValue};
use std::collections::{BTreeSet, HashSet};

fn node(n: u64) -> EntityId {
    EntityId::new(NodeTypeId(1), n).unwrap()
}
fn edge(n: u64) -> EdgeId {
    EdgeId::new(n).unwrap()
}
fn path(nodes: &[u64], edges: &[u64], forward: &[bool]) -> Datum {
    Datum::Path(
        PathValue::new(
            nodes.iter().copied().map(node).collect(),
            edges.iter().copied().map(edge).collect(),
            forward.to_vec(),
        )
        .unwrap(),
    )
}

#[test]
fn path_value_shape_cannot_omit_nodes_or_directions() {
    for (nodes, edges, forward) in [
        (vec![], vec![], vec![]),
        (vec![node(1)], vec![edge(1)], vec![true]),
        (vec![node(1), node(2)], vec![], vec![]),
        (vec![node(1), node(2)], vec![edge(1)], vec![]),
        (vec![node(1), node(2)], vec![edge(1)], vec![true, false]),
    ] {
        assert!(PathValue::new(nodes, edges, forward).is_err());
    }
    let zero = PathValue::new(vec![node(1)], vec![], vec![]).unwrap();
    assert_eq!(zero.nodes(), &[node(1)]);
    assert!(zero.edges().is_empty());
    assert!(zero.forward().is_empty());
}

#[test]
fn path_equality_hashing_and_order_include_history_and_direction() {
    let zero = path(&[1], &[], &[]);
    let first = path(&[1, 2], &[1], &[true]);
    let another_edge = path(&[1, 2], &[2], &[true]);
    let reverse = path(&[1, 2], &[1], &[false]);
    let another_end = path(&[1, 3], &[1], &[true]);
    assert_eq!(first, first.clone());
    assert_ne!(first, another_edge);
    assert_ne!(first, reverse);
    assert_ne!(first, another_end);
    let values = [
        zero.clone(),
        first.clone(),
        another_edge.clone(),
        reverse.clone(),
        another_end.clone(),
        first.clone(),
    ];
    assert_eq!(values.clone().into_iter().collect::<HashSet<_>>().len(), 5);
    let ordered = values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    // Documented order: nodes lexicographically, then edges, then flags.
    assert_eq!(
        ordered,
        vec![zero, reverse, first, another_edge, another_end]
    );
}
