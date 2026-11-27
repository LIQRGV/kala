//! Conservative query dependencies, extracted from the shared prepared IR.
use crate::{AttrRef, Op, Plan, RowAction};
use kala_core::{EntityId, Txn, World};
use std::collections::BTreeSet;
/// Most attribute and type names a merged read set keeps (see
/// [`Reads::merge`]).
const MAX_PENDING: usize = 256;
/// Bounded dependencies of snapshot queries, retained until a transaction opens its writer.
#[derive(Default)]
pub struct Reads {
    attributes: Vec<String>,
    types: Vec<String>,
    definitions: bool,
    entities: BTreeSet<EntityId>,
    all: bool,
}
impl Reads {
    /// Collect dependencies independent of how many rows the plan finds.
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
    /// Record a graph identity whose properties were rendered, including in a list or path.
    pub fn rendered(&mut self, id: EntityId) {
        if !self.all {
            self.entities.insert(id);
            self.bound();
        }
    }
    fn bound(&mut self) {
        if self.all || self.entities.len() + self.attributes.len() + self.types.len() > MAX_PENDING
        {
            self.all = true;
            self.entities.clear();
            self.attributes.clear();
            self.types.clear();
        }
    }
    /// Schema visibility for this plan's attribute and label names. Keep these
    /// bits with materialized rows when a writer may open on a later snapshot.
    /// If a name disappears or becomes defined, recording its new lookup alone
    /// can lose the dependency on the old name or on an absent node type.
    pub fn defined_names(&self, world: &World) -> Vec<bool> {
        let view = world.at_canon(world.latest_tx());
        self.attributes
            .iter()
            .map(|name| view.resolve_attribute(name).is_ok())
            .chain(
                self.types
                    .iter()
                    .map(|name| world.node_types().any(|ty| ty == name)),
            )
            .collect()
    }

    /// A newly declared node type can change a label predicate on an older
    /// snapshot even before an entity of that type exists.
    pub fn same_type_names(&self, from: &World, to: &World) -> bool {
        self.types.iter().all(|name| {
            from.node_types().any(|ty| ty == name) == to.node_types().any(|ty| ty == name)
        })
    }
    /// Adds another query's dependencies to these. A transaction that has
    /// not written yet keeps one merged set for all its queries; past
    /// the pending-key bound it is coarsened to "everything", so the set
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
        self.entities.extend(other.entities.iter().copied());
        self.bound();
    }
    /// Register these dependencies on an optimistic transaction.
    pub fn note(&self, tx: &Txn<'_>) {
        if self.all {
            tx.note_read_all();
            return;
        }
        for id in &self.entities {
            tx.note_read_entity(*id);
        }
        if self.definitions {
            tx.note_entity_definitions();
        }
        for attr in &self.attributes {
            if let Err(error) = tx.note_read_attribute(attr)
                && !matches!(error, kala_core::Error::UnknownAttribute(_))
            {
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
