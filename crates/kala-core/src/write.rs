//! Language-neutral write results. Formatting and serialization belong to adapters.
use crate::{
    CommitResult, EdgeId, EntityId, EntryId, IdMap, Precision, SchemaChange, Tick, TickMoved, TxId,
    Warnings,
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
    /// For conditional creates: whether this operation created the entity.
    pub created: Option<bool>,
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
    /// Provisional IDs the commit renumbered because other transactions
    /// were committed while this one ran ([`CommitResult::ids`]); `ops`
    /// already carry the final IDs. Empty when nothing moved.
    pub ids: IdMap,
    /// The warning that tick-less writes stayed on the latest tick the
    /// transaction began with while a newer commit moved it
    /// ([`CommitResult::latest_tick_moved`]). It displays as the line to
    /// print.
    pub latest_tick_moved: Option<TickMoved>,
}
/// A staged form. Entry IDs become final only after the caller commits.
#[derive(Clone, Debug)]
pub struct StagedWrite {
    pub ops: Vec<WriteOperation>,
    pub entry_offset: usize,
    /// For each of `ops`, the native operations it made: the range of
    /// [`Txn::operation_count`](crate::Txn::operation_count) it covers.
    /// With it, a commit that had to re-apply the transaction over newer
    /// commits can correct each op's counts ([`StagedWrite::complete`]),
    /// and a [`Conflict`](crate::Conflict)'s `operation` can be traced to
    /// the op that read. Empty (no correction) if the stager doesn't track
    /// it.
    pub spans: Vec<std::ops::Range<usize>>,
}
impl StagedWrite {
    /// Attach committed entry IDs to this form's operations. Transaction totals
    /// describe the entire commit, including other forms staged by the caller.
    ///
    /// If the commit re-applied the transaction over newer commits
    /// ([`CommitResult::finalized`]), each operation reports what was
    /// committed, not what was staged: its counts, a created entity's final
    /// ID, whether a conditional create still created, and no precision for
    /// a write that became "0 changed".
    pub fn complete(mut self, result: &CommitResult) -> WriteReport {
        if let Some(finalized) = &result.finalized
            && self.spans.len() == self.ops.len()
        {
            let made = |span: &std::ops::Range<usize>| {
                finalized
                    .ops
                    .get(span.clone())
                    .or_else(|| finalized.ops.get(span.start.min(finalized.ops.len())..))
                    .unwrap_or_default()
            };
            if let Some(first) = self.spans.first() {
                self.entry_offset = made(&(0..first.start)).iter().map(|op| op.changed).sum();
            }
            for (op, span) in self.ops.iter_mut().zip(&self.spans) {
                let made = made(span);
                op.changed = made.iter().map(|m| m.changed).sum();
                op.declared = made.iter().map(|m| m.declared).sum();
                op.entity = op.entity.map(|id| result.ids.resolve(id));
                if op.created.is_some() {
                    op.created = made.iter().rev().find_map(|m| m.created).or(op.created);
                }
                if matches!(op.kind, "assert" | "retract" | "set") {
                    op.precision = made.iter().find_map(|m| m.precision);
                }
            }
        }
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
            ids: result.ids.clone(),
            latest_tick_moved: result.latest_tick_moved,
        }
    }
}
