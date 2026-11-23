//! Opening a world: replaying a log into the in-memory tables, with the
//! same validation a live write gets.

use super::*;

/// Replay state spanning one transaction after the genesis, for the rules a
/// single entry can't check on its own.
#[derive(Debug, Default)]
pub(super) struct ReplayTxn {
    /// Whether the transaction has any entry before its `Commit`.
    has_entries: bool,
    pub(super) migration_expected: std::collections::VecDeque<EntryBody>,
    /// Edges the transaction defined whose triple it hasn't (yet) asserted.
    unasserted_edges: HashMap<Triple, EdgeId>,
    /// The keys (`E` key without `^tx`) of the transaction's facts,
    /// `DELETED` markers and revised facts so far.
    keys: HashSet<Vec<u8>>,
    /// `(of, rev)` of the `DELETED` marker that is the previous entry, if it
    /// is one: a `Revised` right after it shares its rev (a moved key).
    prev_deleted: Option<(EntryId, u32)>,
}

impl ReplayTxn {
    /// Records that the transaction writes an entry at `fact`'s key; `Err`
    /// if it already wrote one there (a live transaction stores at most one
    /// entry per key).
    fn claim_key(&mut self, fact: &FactDef) -> Result<(), String> {
        let mut k = key::e_value_prefix(fact.entity, fact.attr, fact.key_value());
        key::push_seek_tick(&mut k, fact.tick);
        if self.keys.insert(k) {
            Ok(())
        } else {
            Err(format!(
                "a second entry at one key at {:?} in one transaction",
                fact.tick
            ))
        }
    }
}

/// Replay: `rev` must be the next revision of `chain` (base entry `of`).
fn check_next_rev(chain: Chain, of: EntryId, rev: u32) -> Result<(), ReplayFault> {
    match chain.rev.checked_add(1) {
        Some(expected) if expected == rev => Ok(()),
        Some(expected) => Err(ReplayFault::RevisionSkip {
            of,
            expected,
            found: rev,
        }),
        None => Err(ReplayFault::Invalid(format!(
            "revision {rev} of entry#{}, which already has the most revisions",
            of.0
        ))),
    }
}

/// Why replay refused one entry: no live writer could have written it.
/// [`World::open`] turns any of them into [`Error::Corrupt`]; doctor reports
/// each as its own finding and replays on without the entry.
#[derive(Debug)]
pub(crate) enum ReplayFault {
    /// The entry uses an entity or edge ID that no earlier entry defines.
    DanglingId(EntityId),
    /// The entry uses an attribute ID the schema doesn't define.
    DanglingAttr(AttrId),
    /// A correction that names a missing or non-fact entry, isn't at its
    /// chain's current key, or corrects a deleted chain.
    BadCorrection(String),
    /// A correction whose revision isn't its chain's next one.
    RevisionSkip {
        /// The chain's base entry.
        of: EntryId,
        /// The chain's next revision.
        expected: u32,
        /// The revision the entry carries.
        found: u32,
    },
    /// A write a live writer refuses with this error (a value of the wrong
    /// type, a retracted event, an invalid value, ...).
    Refused(Error),
    /// Any other replay rule.
    Invalid(String),
}

impl ReplayFault {
    /// The fault of a fact a live write refuses with `e`.
    fn from_write(e: Error) -> ReplayFault {
        match e {
            Error::UnknownEntity(id) => ReplayFault::DanglingId(id),
            other => ReplayFault::Refused(other),
        }
    }
}

impl std::fmt::Display for ReplayFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayFault::DanglingId(id) => write!(f, "undefined ID {id:?}"),
            ReplayFault::DanglingAttr(attr) => write!(f, "unknown attribute {attr:?}"),
            ReplayFault::BadCorrection(why) | ReplayFault::Invalid(why) => f.write_str(why),
            ReplayFault::RevisionSkip {
                of,
                expected,
                found,
            } => write!(
                f,
                "revision {found} of entry#{} where {expected} was next",
                of.0
            ),
            ReplayFault::Refused(e) => write!(f, "{e}"),
        }
    }
}

/// The schema declared by the genesis transaction's entries (without its
/// Commit): node type and attribute `DEFINE`s only, each kind numbered
/// 0, 1, 2, ... in log order, as [`World::create`] writes them.
fn genesis_schema(entries: &[LogEntry]) -> Result<Schema, Error> {
    let mut schema = Schema::new();
    let (mut node_types, mut attrs) = (0usize, 0usize);
    for entry in entries {
        let corrupt = |why: String| Error::Corrupt(format!("genesis entry#{}: {why}", entry.id.0));
        match &entry.body {
            EntryBody::DefineNodeType { id, name } => {
                if usize::from(id.0) != node_types {
                    return Err(corrupt(format!("node type {id:?} out of sequence")));
                }
                node_types += 1;
                schema = schema.node_type(name);
            }
            EntryBody::DefineAttr { id, def } => {
                if def.inferred || def.deprecated {
                    return Err(corrupt(
                        "genesis attributes must be declared and writable".into(),
                    ));
                }
                if usize::try_from(id.0).ok() != Some(attrs) {
                    return Err(corrupt(format!("attribute {id:?} out of sequence")));
                }
                attrs += 1;
                schema = schema.attribute(def.clone());
            }
            EntryBody::Schema(SchemaEdit::Setting { key, value }) => {
                schema = schema.setting(key, value);
            }
            _ => {
                return Err(corrupt(
                    "not a genesis schema definition or world setting".into(),
                ));
            }
        }
    }
    Ok(schema)
}

impl World {
    /// Creates a new world backed by a new log file at `path`.
    ///
    /// The schema is validated exactly as by [`World::in_memory`] before the
    /// file system is touched. Then the file is created (never overwritten:
    /// an existing path fails with [`Error::Io`] of kind `AlreadyExists`),
    /// the header and the schema are written as the genesis transaction
    /// tx#0, and the file and its directory are fsynced. Like an in-memory
    /// world, the new world's [`latest_tx`](World::latest_tx) is `TxId(0)`.
    ///
    /// The world holds an exclusive advisory lock on the file for its whole
    /// lifetime (taken right after creating it, before writing), so a
    /// [`World::open`] of the same file meanwhile fails with
    /// [`Error::WorldLocked`]. Advisory means it keeps out other Kala
    /// worlds, not other programs.
    ///
    /// Every later commit is appended to the file and fsynced before it
    /// becomes visible. If an append fails, the commit fails with
    /// [`Error::Io`], the world is left as it was, and every later write
    /// fails with [`Error::Io`] too. If only the fsync failed, the
    /// transaction's bytes are first cut off the file again (best effort),
    /// so the failed commit doesn't reappear on reopen.
    pub fn create(path: &Path, schema: Schema) -> Result<World, Error> {
        let (mut world, genesis) = World::with_genesis(schema)?;
        world.journal.attach_log(LogWriter::create(path, &genesis)?);
        Ok(world)
    }

    /// Opens the world backed by the existing log file at `path` by
    /// replaying its committed transactions: the schema from the genesis
    /// transaction tx#0, then every later transaction's entity `DEFINE`s
    /// and facts, in log order. The rebuilt world answers every read, at
    /// every canon, exactly as the world that wrote the log did, and later
    /// commits are appended to the same file. A clean log is not modified.
    ///
    /// Fails with [`Error::Io`] if the file can't be opened (`NotFound` if
    /// it doesn't exist; it is never created; `InvalidInput` for anything
    /// but a regular file, such as a directory, FIFO or device, refused
    /// before any read), [`Error::BadMagic`] or
    /// [`Error::UnsupportedVersion`] for a foreign header, or
    /// [`Error::Corrupt`] if the log has no committed genesis transaction or
    /// a committed transaction that a live write could not have produced
    /// (an ID used before its `DEFINE`, a value of the wrong type, a no-op
    /// fact, an edge `DEFINE` with no assert of its triple in its
    /// transaction, a transaction with nothing but its `Commit`, two entries
    /// at one key in one transaction, a `DELETED` or `Revised` entry that
    /// doesn't point at an earlier base fact or skips a revision, a
    /// `DELETED` marker not at its chain's current key or of an already
    /// deleted chain, a `Revised` that changes nothing at the current key,
    /// sits at another key without directly following its rev's `DELETED`
    /// marker, or lands on a key with a current entry, ...).
    ///
    /// The file is locked exclusively (an advisory lock, held for the
    /// world's lifetime) before it is read. If another world holds the lock
    /// this fails with [`Error::WorldLocked`] and the file is untouched: no
    /// cut, no quarantine.
    ///
    /// A torn tail (a last transaction cut short by a crash mid-append, or a
    /// zero-filled tail) or a damaged one (a last transaction whose bytes
    /// are all there but fail its body checksum, with no valid begin record
    /// after it; exact rules in [`crate::log`]) is cut off the file and
    /// fsynced once the rest has replayed cleanly; the world then continues
    /// entry IDs and tx from the last committed transaction. Before the cut,
    /// the cut bytes are saved and fsynced to `<path>.torn-<valid_len>`
    /// (with `-1`, `-2`, ... added if that exists); if saving them fails,
    /// this fails with [`Error::Io`] and the log is not cut.
    /// [`World::recovery`] reports what was cut, why and where it was saved.
    /// Any other damage, in any transaction, is [`Error::Corrupt`] instead,
    /// with the file untouched.
    pub fn open(path: &Path) -> Result<World, Error> {
        Self::open_with(path, true)
    }

    /// Like [`World::open`], but a *damaged* last transaction (see
    /// [`CutCause::Damaged`]) is not cut: the open fails with
    /// [`Error::RecoveryRequired`] and the file is left untouched (no cut,
    /// no quarantine file). The check runs under the writer lock, so no
    /// other open can change the tail in between. Use it where a damaged
    /// tail must be reviewed before it is cut (it may hold an acknowledged
    /// write); then [`World::open`] and [`World::acknowledge_recovery`].
    /// Clean logs and torn tails open exactly as with [`World::open`].
    pub fn open_refusing_damaged(path: &Path) -> Result<World, Error> {
        Self::open_with(path, false)
    }

    fn open_with(path: &Path, cut_damaged: bool) -> Result<World, Error> {
        let (mut world, writer, recovery) = LogWriter::open(path, World::replay, cut_damaged)?;
        world.journal.attach_log(writer);
        world.gate = WriteGate::after_recovery(recovery);
        Ok(world)
    }

    /// Opens the world in the existing log file at `path` for reading only:
    /// a snapshot of its committed transactions at the moment of the call,
    /// replayed exactly as [`World::open`] replays them, so every read
    /// answers as a `World::open` of the same bytes would. Commits appended
    /// to the file afterwards (by a writer world) are not seen; open again
    /// to see them.
    ///
    /// Takes no lock, so it succeeds while another world (from
    /// [`World::open`] or [`World::create`]) holds the file, and it never
    /// keeps a writer out. It never modifies the file system: a torn or
    /// damaged tail is left in the file and only left out in memory, and
    /// [`World::recovery`] reports what `World::open` would cut
    /// (`bytes_cut`, `cause`) with `quarantine: None`.
    ///
    /// Every write (create, assert, retract, correct, a [`Txn`] op or
    /// commit, a no-op included) fails with [`Error::ReadOnly`] and appends
    /// nothing; [`World::acknowledge_recovery`] has no effect on that.
    ///
    /// Fails as [`World::open`] does short of locking and cutting:
    /// [`Error::Io`] if the file can't be read (`NotFound` if it doesn't
    /// exist; it is never created; `InvalidInput` for anything but a
    /// regular file, such as a directory, FIFO or device, refused before
    /// any read), [`Error::BadMagic`] or
    /// [`Error::UnsupportedVersion`] for a foreign header, or
    /// [`Error::Corrupt`] for any damage other than a torn or damaged tail.
    pub fn open_read_only(path: &Path) -> Result<World, Error> {
        let (mut world, recovery) = LogWriter::read_committed(path, World::replay)?;
        world.gate = WriteGate::read_only(recovery);
        Ok(world)
    }

    /// A detached, writable in-memory copy of the committed state, for previews.
    /// Writes to this copy cannot touch this world's log or acquire its lock.
    /// Replays the kept entries with the same validation as opening a world;
    /// costs O(log size) memory and replay work. Recovery tails are not copied.
    pub fn preview_copy(&self) -> Result<World, Error> {
        World::replay_with_hash(&self.journal.log_entries(), self.indexes.content_hash)
    }

    /// Rebuilds a world from the entries of a log's committed transactions.
    fn replay(entries: &[LogEntry]) -> Result<World, Error> {
        World::replay_with_hash(entries, fnv1a64)
    }

    /// [`World::replay`] with value references hashed by `hash`.
    pub(super) fn replay_with_hash(
        entries: &[LogEntry],
        hash: ContentHash,
    ) -> Result<World, Error> {
        // Split off the genesis transaction: everything up to the first
        // Commit. `read_log`'s scan guarantees the first Commit is tx#0.
        let genesis_end = entries
            .iter()
            .position(|e| matches!(e.body, EntryBody::Commit { .. }))
            .ok_or_else(|| Error::Corrupt("no committed genesis transaction".to_owned()))?;
        let mut world = World::from_genesis(&entries[..=genesis_end])?;
        world.set_rebuild_hash(hash);
        let mut txn = ReplayTxn::default();
        for entry in &entries[genesis_end + 1..] {
            world.replay_entry(entry, &mut txn)?;
        }
        Ok(world)
    }

    /// A world holding just the genesis transaction `genesis` (its schema
    /// `DEFINE`s, then its `Commit`), as a log has it. Fails with
    /// [`Error::Corrupt`] if those entries don't declare a valid schema.
    pub(crate) fn from_genesis(genesis: &[LogEntry]) -> Result<World, Error> {
        let defines = genesis.split_last().map_or(&[][..], |(_, d)| d);
        let mut world = World::in_memory(genesis_schema(defines)?)
            .map_err(|e| Error::Corrupt(format!("genesis schema: {e}")))?;
        // Keep the genesis entries as the file has them, not as this build
        // would re-encode them: `entry` reports what the log says. Kept
        // as given, not through `record_one`: `World::open`'s scan has
        // already refused entry IDs that don't start at 1, but doctor's
        // survey reports them (`EntryIdGap`) and rebuilds anyway, and a
        // rebuild copes with ascending IDs that have gaps (see
        // `Journal::kept_entry`).
        world.journal.clear_entries();
        for e in genesis {
            world.journal.keep(e, TxId(0));
        }
        Ok(world)
    }

    /// Applies one entry of a transaction after the genesis. Facts are
    /// keyed at the tx of the `Commit` ending their transaction; since the
    /// tx isn't known until then, they're keyed at `latest_tx + 1`, which
    /// the scan guarantees is the Commit's tx. `txn` tracks the rules that
    /// span the current transaction and is reset by its `Commit`.
    pub(super) fn replay_entry(
        &mut self,
        entry: &LogEntry,
        txn: &mut ReplayTxn,
    ) -> Result<(), Error> {
        // The scan guarantees the transaction's Commit has tx `next_tx`.
        let tx = self.next_tx();
        self.apply_entry(entry, txn)
            .map_err(|fault| Error::Corrupt(format!("entry#{}: {fault}", entry.id.0)))?;
        self.journal.record_one(entry.clone(), tx);
        Ok(())
    }

    /// Doctor's replay of one whole transaction `tx` (after the genesis):
    /// like [`World::replay_entry`] for each entry, but an entry that fails
    /// is passed to `refused` with its [`ReplayFault`], left out (not
    /// applied, not kept) and the replay goes on. The transaction counts as
    /// committed at `tx` whatever happened to its entries, so the next one
    /// replays at its own tx even after a skipped transaction (a gap).
    ///
    /// Entries kept this way may have ID gaps; `Journal::kept_entry` copes.
    /// Only a doctor rebuild, never a live world, takes this path.
    pub(crate) fn replay_txn_collecting(
        &mut self,
        tx: TxId,
        entries: &[LogEntry],
        mut refused: impl FnMut(EntryId, ReplayFault),
    ) {
        if tx <= self.latest_tx() {
            // A scan only hands over ascending txs; refuse rather than rewind.
            if let Some(first) = entries.first() {
                refused(
                    first.id,
                    ReplayFault::Invalid(format!("{tx:?} after {:?}", self.latest_tx())),
                );
            }
            return;
        }
        // Facts are keyed at `next_tx()`: make that `tx`.
        self.journal.set_latest_tx(TxId(tx.0 - 1));
        let mut txn = ReplayTxn::default();
        for entry in entries {
            match self.apply_entry(entry, &mut txn) {
                Ok(()) => self.journal.keep(entry, tx),
                Err(fault) => refused(entry.id, fault),
            }
        }
        // Also when its Commit was refused: the transaction is over.
        self.journal.set_latest_tx(tx);
    }

    /// Every kept entry grouped by transaction, in log order: what doctor
    /// replays for an in-memory world (a file-backed one is re-read from
    /// disk instead).
    pub(crate) fn kept_txns(&self) -> Vec<SurveyedTxn> {
        self.journal.kept_txns()
    }

    /// Validates and applies one replayed entry (see
    /// [`World::replay_entry`]).
    fn apply_entry(&mut self, entry: &LogEntry, txn: &mut ReplayTxn) -> Result<(), ReplayFault> {
        let corrupt = |why: &dyn std::fmt::Display| ReplayFault::Invalid(why.to_string());
        let bad_correction =
            |why: &dyn std::fmt::Display| ReplayFault::BadCorrection(why.to_string());
        let unknown_attr = |attr: &AttrId| ReplayFault::DanglingAttr(*attr);
        if !matches!(entry.body, EntryBody::Commit { .. }) {
            txn.has_entries = true;
        }
        let copied = if let Some(expected) = txn.migration_expected.pop_front() {
            if expected != entry.body {
                return Err(corrupt(&"migration copy does not match its source history"));
            }
            true
        } else {
            false
        };
        // Only the entry right after a DELETED marker may share its rev.
        let prev_deleted = txn.prev_deleted.take();
        match &entry.body {
            EntryBody::Schema(SchemaEdit::Migration {
                source,
                def,
                skip_unconvertible,
            }) => {
                txn.migration_expected = self
                    .migration_expected(*source, def, *skip_unconvertible)
                    .map_err(ReplayFault::Refused)?;
                Ok(())
            }
            EntryBody::Schema(edit) => self
                .apply_schema_edit(edit, self.next_tx(), copied)
                .map_err(ReplayFault::Refused),
            EntryBody::DefineNodeType { .. } | EntryBody::DefineAttr { .. } => Err(corrupt(
                &"genesis definition outside the genesis transaction; use a schema revision",
            )),
            EntryBody::DefineEntity { id, handle } => {
                // Validate as a live create would, then check the log
                // assigned exactly the ID a live create assigns.
                let name = self
                    .catalog
                    .node_types
                    .get(usize::from(id.node_type().0))
                    .filter(|_| !id.is_edge())
                    .map(|nt| nt.name.clone())
                    .ok_or_else(|| corrupt(&format!("entity {id:?} has an unknown node type")))?;
                let def = self
                    .catalog
                    .prepare_entity(&StagedIds::default(), &name, handle.as_deref())
                    .map_err(ReplayFault::Refused)?;
                if def.id != *id {
                    return Err(corrupt(&format!(
                        "entity {id:?} defined where {:?} was next",
                        def.id
                    )));
                }
                // Both replay paths key the transaction at `next_tx`.
                let tx = self.next_tx();
                self.catalog.apply_entity(def, tx);
                Ok(())
            }
            EntryBody::DefineEdge {
                id,
                source,
                attr,
                target,
            } => {
                // Exactly what a live interning writes: a ref attribute,
                // endpoints defined by earlier entries, a triple not yet
                // interned, and the next edge sequence.
                let def = self
                    .catalog
                    .attributes
                    .get(attr.0 as usize)
                    .ok_or_else(|| unknown_attr(attr))?;
                if def.value_type != ValueType::Ref {
                    return Err(corrupt(&format!(
                        "edge on non-ref attribute {:?}",
                        def.name
                    )));
                }
                for end in [source, target] {
                    if !self.catalog.is_defined_staged(&StagedIds::default(), *end) {
                        return Err(ReplayFault::DanglingId(*end));
                    }
                }
                // A bidirectional pair is interned once, normalised.
                if def.direction == Some(RefDirection::Bidirectional) && source > target {
                    return Err(corrupt(&"bidirectional edge with source > target"));
                }
                let triple = (*source, *attr, *target);
                if let Some(existing) = self.catalog.edge_by_triple.get(&triple) {
                    return Err(corrupt(&format!("triple already interned as {existing:?}")));
                }
                if id.seq() != self.catalog.last_edge_seq(&StagedIds::default()) + 1 {
                    return Err(corrupt(&format!("{id:?} out of sequence")));
                }
                if !copied {
                    txn.unasserted_edges.insert(triple, *id);
                }
                let tx = self.next_tx();
                self.catalog.apply_edge(
                    EdgeDef {
                        id: *id,
                        source: *source,
                        attr: *attr,
                        target: *target,
                    },
                    tx,
                );
                Ok(())
            }
            EntryBody::Fact {
                entity,
                attr,
                value,
                tick,
                op,
                precision,
            } => {
                let def = self
                    .catalog
                    .attributes
                    .get(attr.0 as usize)
                    .ok_or_else(|| unknown_attr(attr))?;
                // A live write stores a bidirectional fact normalised; checked
                // before `prepare_fact`, which would normalise it silently.
                if def.direction == Some(RefDirection::Bidirectional)
                    && let Value::Ref(target) = value
                    && entity > target
                {
                    return Err(corrupt(&"bidirectional fact with entity > value"));
                }
                // A migration copies edge properties of deprecated
                // attributes too; only entries matching the regenerated copy
                // sequence (`copied`) may write them.
                let deprecated = if copied {
                    DeprecatedWrites::MigrationCopy
                } else {
                    DeprecatedWrites::Refuse
                };
                let mut fact = self
                    .prepare_fact_as(
                        &StagedIds::default(),
                        *entity,
                        &def.name,
                        value,
                        *tick,
                        *op,
                        deprecated,
                    )
                    .map_err(ReplayFault::from_write)?;
                fact.precision = *precision;
                // A live write logs a ref fact's DefineEdge ahead of its
                // first stored assert, and a stored retract needs a stored
                // assert, so every stored ref fact's triple has an edge
                // defined by an earlier entry.
                if let Value::Ref(target) = value
                    && self
                        .catalog
                        .edge_of(&StagedIds::default(), fact.entity, fact.attr, *target)
                        .is_none()
                {
                    return Err(corrupt(&"ref fact with no edge defined before it"));
                }
                // A live write never stores a fact at a key whose current
                // entry has the opposite op: a same-tick conflict. (The same
                // op again is a no-op, caught below.) A key whose current
                // entry is a DELETED marker holds nothing. Judged at the
                // latest canon; the current transaction's entries are
                // already in.
                let mut prefix = key::e_value_prefix(fact.entity, fact.attr, fact.key_value());
                let all = TxId(u64::MAX);
                if let Some(stored) =
                    op_at_tick(&[&self.indexes.e_index], &mut prefix, fact.tick, all)
                    && stored != fact.op
                {
                    return Err(corrupt(&format!(
                        "a same-tick conflict: {:?} at {:?} where {stored:?} is current",
                        fact.op, fact.tick
                    )));
                }
                // A live write never stores a no-op: an assert of a value
                // that holds at its tick, or a retract of one that doesn't.
                // Judged like a live write, against everything before it
                // (the current transaction's earlier facts are already in).
                let held = holds(
                    &[&self.indexes.e_index],
                    &mut prefix,
                    fact.tick,
                    all,
                    Work::NONE,
                );
                if !copied && held == (fact.op == Op::Assert) {
                    return Err(corrupt(&format!(
                        "a no-op {:?} at {:?} (the value {} held there)",
                        fact.op,
                        fact.tick,
                        if held { "already" } else { "never" }
                    )));
                }
                // A live transaction stores at most one entry per key.
                // Claimed last, so a refused fact claims nothing.
                txn.claim_key(&fact).map_err(|why| corrupt(&why))?;
                if let (Some(target), Op::Assert) = (fact.target, fact.op) {
                    txn.unasserted_edges
                        .remove(&(fact.entity, fact.attr, target));
                }
                self.index_entry(&fact, Mark::from(fact.op), self.next_tx());
                Ok(())
            }
            EntryBody::Deleted {
                of,
                rev,
                entity,
                attr,
                value,
                tick,
            } => {
                let def = self
                    .catalog
                    .attributes
                    .get(attr.0 as usize)
                    .ok_or_else(|| unknown_attr(attr))?;
                if def.direction == Some(RefDirection::Bidirectional)
                    && let Value::Ref(target) = value
                    && entity > target
                {
                    return Err(bad_correction(
                        &"bidirectional DELETED marker with entity > value",
                    ));
                }
                let chain = self
                    .journal
                    .replay_chain(*of)
                    .map_err(|why| bad_correction(&why))?;
                let current = chain.current.ok_or_else(|| {
                    bad_correction(&format!("the chain of entry#{} is already deleted", of.0))
                })?;
                check_next_rev(chain, *of, *rev)?;
                // The marker sits at the key of the chain's current fact.
                // (Its op is unused: the marker is indexed as Deleted.)
                let fact = self
                    .stored_fact_def(*entity, *attr, value, *tick, Op::Assert)
                    .map_err(ReplayFault::Refused)?;
                if self.journal.op_if_at_key(current, &fact).is_none() {
                    return Err(bad_correction(&format!(
                        "DELETED marker not at the current key of entry#{}",
                        of.0
                    )));
                }
                txn.claim_key(&fact).map_err(|why| corrupt(&why))?;
                self.index_entry(&fact, Mark::Deleted, self.next_tx());
                self.journal.set_chain(*of, *rev, None);
                txn.prev_deleted = Some((*of, *rev));
                Ok(())
            }
            EntryBody::Revised {
                of,
                rev,
                entity,
                attr,
                value,
                tick,
                op,
                precision,
            } => {
                let def = self
                    .catalog
                    .attributes
                    .get(attr.0 as usize)
                    .ok_or_else(|| unknown_attr(attr))?;
                if def.direction == Some(RefDirection::Bidirectional)
                    && let Value::Ref(target) = value
                    && entity > target
                {
                    return Err(bad_correction(
                        &"bidirectional revised fact with entity > value",
                    ));
                }
                // Validated like a live write (type, defined IDs, string
                // length, RetractEvent); not judged by the hold-based no-op
                // rules, which don't apply to corrections.
                let mut fact = self
                    .prepare_fact_as(
                        &StagedIds::default(),
                        *entity,
                        &def.name,
                        value,
                        *tick,
                        *op,
                        DeprecatedWrites::Correction,
                    )
                    .map_err(ReplayFault::from_write)?;
                fact.precision = *precision;
                // A live correction interns the edge of a revised assert's
                // triple before it, as for a fact; a revised retract may be
                // the triple's only entry (no edge).
                if let (Some(target), Op::Assert) = (fact.target, fact.op)
                    && self
                        .catalog
                        .edge_of(&StagedIds::default(), fact.entity, fact.attr, target)
                        .is_none()
                {
                    return Err(corrupt(
                        &"revised ref assert with no edge defined before it",
                    ));
                }
                let chain = self
                    .journal
                    .replay_chain(*of)
                    .map_err(|why| bad_correction(&why))?;
                if prev_deleted == Some((*of, *rev)) {
                    // A moved key, right after the DELETED marker of the old
                    // one (which already took this rev). The same-tick rule
                    // at the new key: a live writer errs on the opposite op
                    // and writes no Revised for the same one, so any current
                    // entry there means no live writer wrote this.
                    let mut prefix = key::e_value_prefix(fact.entity, fact.attr, fact.key_value());
                    let all = TxId(u64::MAX);
                    if let Some(stored) =
                        op_at_tick(&[&self.indexes.e_index], &mut prefix, fact.tick, all)
                    {
                        return Err(corrupt(&format!(
                            "a moved revision onto {:?} where {stored:?} is current",
                            fact.tick
                        )));
                    }
                } else {
                    // Otherwise it revises the current fact in place: same
                    // key, its own next rev, another op or precision.
                    let current = chain.current.ok_or_else(|| {
                        bad_correction(&format!(
                            "a revision of the deleted chain of entry#{} not right after its \
                             DELETED marker",
                            of.0
                        ))
                    })?;
                    check_next_rev(chain, *of, *rev)?;
                    let Some((c_op, c_precision)) = self.journal.op_if_at_key(current, &fact)
                    else {
                        return Err(bad_correction(&format!(
                            "a revision of entry#{} at another key not right after its \
                             DELETED marker",
                            of.0
                        )));
                    };
                    if (fact.op, fact.precision) == (c_op, c_precision) {
                        return Err(corrupt(&format!(
                            "a revision of entry#{} that changes nothing",
                            of.0
                        )));
                    }
                }
                txn.claim_key(&fact).map_err(|why| corrupt(&why))?;
                // Only an accepted revised assert asserts its edge's triple
                // (as for a Fact): a refused one must leave the edge
                // unasserted, so doctor's collecting replay judges the
                // Commit like a fresh rebuild.
                if let (Some(target), Op::Assert) = (fact.target, fact.op) {
                    txn.unasserted_edges
                        .remove(&(fact.entity, fact.attr, target));
                }
                self.index_entry(&fact, Mark::from(fact.op), self.next_tx());
                self.journal.set_chain(*of, *rev, Some(entry.id));
                Ok(())
            }
            EntryBody::Commit { tx, .. } => {
                if *tx != self.next_tx() {
                    return Err(corrupt(&format!("{tx:?} out of sequence")));
                }
                // A live commit with nothing staged appends nothing.
                if !txn.has_entries {
                    return Err(corrupt(&"a transaction with nothing but its Commit"));
                }
                // A live write interns an edge only for a stored assert of
                // its triple in the same transaction.
                if let Some(id) = txn.unasserted_edges.values().min() {
                    return Err(corrupt(&format!(
                        "{id:?} defined in a transaction that doesn't assert its triple"
                    )));
                }
                *txn = ReplayTxn::default();
                self.journal.set_latest_tx(*tx);
                Ok(())
            }
        }
    }
}
