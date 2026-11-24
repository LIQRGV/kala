//! Language-neutral write results. Formatting and serialization belong to adapters.
use crate::{
    CommitResult, EdgeId, EntityId, EntryId, Precision, SchemaChange, Tick, TxId, Warnings,
};

/// One source operation and the primitive entries it wrote.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteOperation {
    pub position: usize,
    pub kind: &'static str,
    pub tick: Tick,
    pub precision: Option<Precision>,
    pub changed: usize,
    pub declared: usize,
    pub entries: Vec<EntryId>,
    pub entity: Option<EntityId>,
}
/// A committed transaction, independent of any language or output format.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteReport {
    pub tx: Option<TxId>,
    pub changed: usize,
    pub declared: usize,
    pub entries: Vec<EntryId>,
    pub edges: Vec<EdgeId>,
    pub schema: Vec<SchemaChange>,
    pub warnings: Warnings,
    pub ops: Vec<WriteOperation>,
}
/// A staged form. Entry IDs become final only after the caller commits.
#[derive(Clone, Debug)]
pub struct StagedWrite {
    pub ops: Vec<WriteOperation>,
    pub entry_offset: usize,
}
impl StagedWrite {
    /// Attach committed entry IDs to this form's operations. Transaction totals
    /// describe the entire commit, including other forms staged by the caller.
    pub fn complete(mut self, result: &CommitResult) -> WriteReport {
        let mut ids = result.entries.iter().skip(self.entry_offset);
        for op in &mut self.ops {
            op.entries = ids.by_ref().take(op.changed).copied().collect();
        }
        WriteReport {
            tx: result.committed.then_some(result.tx),
            changed: result.changed,
            declared: result.declared,
            entries: result.entries.clone(),
            edges: result.edges.clone(),
            schema: result.schema.clone(),
            warnings: result.warnings.clone(),
            ops: self.ops,
        }
    }
}
