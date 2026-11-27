//! Conservative query dependencies, extracted from the shared prepared IR.
use kala_core::{Txn, World};
use kala_query::{AttrRef, Op, Plan, RowAction};
/// Most attribute and type names a merged read set keeps (see
/// [`Reads::merge`]).
const MAX_PENDING: usize = 256;
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
    /// Adds another query's dependencies to these. A transaction that has
    /// not written yet keeps one merged set for all its queries; past
    /// [`MAX_PENDING`] names it is coarsened to "everything", so the set
    /// stays small however many queries a long transaction runs.
    pub fn merge(&mut self, other: &Self) {
        self.all |= other.all;
        self.definitions |= other.definitions;
        for (mine, theirs) in [
            (&mut self.attributes, &other.attributes),
            (&mut self.types, &other.types),
        ] {
            for name in theirs {
                if !mine.contains(name) {
                    mine.push(name.clone());
                }
            }
        }
        if self.all || self.attributes.len() + self.types.len() > MAX_PENDING {
            self.all = true;
            self.attributes.clear();
            self.types.clear();
        }
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
