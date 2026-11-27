//! Conservative query dependencies, extracted from the shared prepared IR.
use kala_core::{Txn, World};
use kala_query::{AttrRef, Op, Plan, RowAction};
#[derive(Default)]
pub(crate) struct Reads {
    attributes: Vec<String>,
    types: Vec<String>,
    definitions: bool,
    pub all: bool,
}
impl Reads {
    pub fn plan(plan: &Plan, world: &World) -> Self {
        let mut reads = Self {
            attributes: plan
                .attribute_names()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            ..Self::default()
        };
        let mut todo = vec![&plan.root];
        while let Some(op) = todo.pop() {
            match op {
                Op::NodeScan { node_type, .. } => reads.types.push(node_type.clone()),
                Op::NodeSeek { .. }
                | Op::Expand {
                    attr: AttrRef::Handle,
                    ..
                } => reads.definitions = true,
                Op::Row { action, .. } => match action {
                    RowAction::Nodes { labels, .. } => {
                        let types: Vec<_> = labels
                            .iter()
                            .filter(|l| world.node_types().any(|t| t == l.as_str()))
                            .cloned()
                            .collect();
                        if types.is_empty() {
                            reads.definitions = true;
                        }
                        // Unknown labels currently denote the label attribute, but
                        // declaring that node type changes their interpretation.
                        reads.types.extend(labels.iter().cloned());
                    }
                    RowAction::NodeByHandle { .. } => reads.definitions = true,
                    RowAction::Source { .. } => reads.all = true,
                    _ => {}
                },
                _ => {}
            }
            match op {
                Op::Unit => {}
                Op::Optional { input, body } | Op::Not { input, body } => {
                    todo.push(input);
                    todo.push(body);
                }
                Op::Join { left, right }
                | Op::Difference { left, right }
                | Op::Intersect { left, right } => {
                    todo.push(left);
                    todo.push(right);
                }
                Op::Union { inputs } => todo.extend(inputs),
                Op::Fixpoint { rels, body } => {
                    todo.push(body);
                    for rel in rels {
                        todo.extend(&rel.rules);
                    }
                }
                Op::Row { input, .. }
                | Op::Output { input, .. }
                | Op::NodeScan { input, .. }
                | Op::NodeSeek { input, .. }
                | Op::Expand { input, .. }
                | Op::Filter { input, .. }
                | Op::Project { input, .. }
                | Op::History { input, .. }
                | Op::During { input, .. }
                | Op::Apply { input, .. }
                | Op::Aggregate { input, .. }
                | Op::Rel { input, .. } => todo.push(input),
            }
        }
        reads
    }
    pub fn note(&self, tx: &Txn<'_>) {
        if self.all {
            tx.note_read_all();
            return;
        }
        if self.definitions {
            tx.note_entity_definitions();
        }
        for attr in &self.attributes {
            if tx.note_read_attribute(attr).is_err() {
                tx.note_read_all();
            }
        }
        for name in &self.types {
            if let Err(error) = tx.note_type_scan(name)
                && !matches!(error, kala_core::Error::UnknownNodeType(_))
            {
                tx.note_read_all();
            }
        }
    }
}
