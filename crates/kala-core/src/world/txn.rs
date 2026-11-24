//! The write path: validating a fact into a definition, judging it against
//! what holds at its tick, and [`Txn`], which stages and commits writes.

use super::*;

/// One entry a transaction staged, in write order: the order of its log
/// entries.
#[derive(Debug)]
pub(super) enum Staged {
    /// A fact (assert or retract).
    Fact(FactDef),
    /// A `DELETED` marker: revision `rev` of the chain of base entry `of`,
    /// at the key of `fact`, the chain's current fact.
    Deleted {
        of: EntryId,
        rev: u32,
        fact: FactDef,
    },
    /// A revised fact: revision `rev` of the chain of base entry `of`, its
    /// new current fact.
    Revised {
        of: EntryId,
        rev: u32,
        fact: FactDef,
    },
}

impl Target {
    /// The revision the next correction of the chain carries.
    fn next_rev(&self) -> Result<u32, Error> {
        self.chain
            .rev
            .checked_add(1)
            .ok_or(Error::TooManyRevisions(self.base))
    }
}

impl Staged {
    /// The key the entry is stored at and what it says there.
    pub(super) fn key_and_mark(&self) -> (&FactDef, Mark) {
        match self {
            Staged::Fact(fact) | Staged::Revised { fact, .. } => (fact, Mark::from(fact.op)),
            Staged::Deleted { fact, .. } => (fact, Mark::Deleted),
        }
    }
}

/// What a correction acts on (from [`World::correction_target`]).
#[derive(Debug)]
pub(super) struct Target {
    /// The chain's base entry.
    base: EntryId,
    chain: Chain,
    /// The chain's current fact, as stored.
    pub(super) fact: FactDef,
    /// Its value, decoded.
    pub(super) value: Value,
}

/// A fact as it was written, before validation: what [`World::prepare_fact`]
/// turns into a [`FactDef`].
#[derive(Clone, Copy, Debug)]
pub(super) struct FactRequest<'a> {
    /// The subject as written (a bidirectional pair isn't normalised yet).
    pub(super) entity: EntityId,
    /// The attribute's name.
    pub(super) attr: &'a str,
    pub(super) value: &'a Value,
    pub(super) tick: Tick,
    pub(super) op: Op,
    /// The precision of the tick: a payload the fact carries, never judged.
    pub(super) precision: Precision,
    /// Whether the fact may target a deprecated attribute.
    pub(super) deprecated: DeprecatedWrites,
}

/// Outcome of judging a valid write against the stored facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// The write changes something: store it.
    Store,
    /// The write changes nothing ("0 changed").
    Noop,
}

/// One `(entity, attr, side)` check a commit touches, with the values
/// (encoded, ascending, deduplicated) whose intervals it may change there.
#[derive(Debug)]
pub(super) struct TouchedSide {
    side: (EntityId, AttrId, Side),
    values: Vec<Vec<u8>>,
}

/// The result of committing a transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct CommitResult {
    /// Whether this call appended a transaction, including entity-only commits.
    pub committed: bool,
    /// Schema changes, in operation order.
    pub schema: Vec<crate::schema::SchemaChange>,
    /// The transaction's number. If nothing was written (every write was a
    /// no-op and no entity was created), nothing is appended and this is
    /// the unchanged latest tx.
    pub tx: TxId,
    /// Fact, `DELETED` and revised entries, including migration copies.
    /// Entity creation, edge interning and schema actions do not count.
    pub changed: usize,
    /// Reported schema actions, excluding no-ops. A migration reports its
    /// rename, deprecation, declaration and copy action.
    pub declared: usize,
    /// IDs of the fact, `DELETED` and `Revised` entries the transaction appended, in
    /// log order (`DEFINE`s and the `Commit` are not listed): one per stored
    /// entry. Empty if nothing was appended.
    pub entries: Vec<EntryId>,
    /// Edges newly interned by the transaction, in interning order (the
    /// order of their `DEFINE` entries). Re-asserting an already interned
    /// triple interns nothing.
    pub edges: Vec<EdgeId>,
    /// The cardinality overlaps the transaction introduced and resolved:
    /// its net effect, so an overlap made and resolved inside the
    /// transaction isn't reported (see [`Warnings`]). Empty if nothing was
    /// appended.
    pub warnings: Warnings,
}

/// What holds a value's exact tick, from [`Txn::entry_at_tick`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TickOccupant {
    /// A committed log entry. Its ID is permanent, so it may be named in a
    /// `correct`.
    Committed {
        /// The entry (a base fact or its current revision).
        entry: EntryId,
        /// What it says at that tick.
        op: Op,
    },
    /// An entry this transaction staged and has not committed. It has no
    /// entry ID yet: if the transaction aborts, the ID it would have got goes
    /// to an unrelated later entry, so nothing may name it.
    Staged {
        /// Its position among the transaction's staged entries: the count
        /// [`Txn::staged_entry_count`] returned just before it was staged.
        position: usize,
        /// What it says at that tick.
        op: Op,
    },
}

/// A write transaction on a [`World`], started with [`World::begin`].
///
/// Writes are judged as they are made, against the committed facts *and*
/// the transaction's own earlier writes, then staged; [`Txn::commit`] makes
/// them all visible under one new tx. Dropping a `Txn` without committing
/// discards everything it staged, including created entities: their IDs and
/// handles are free again.
///
/// The first write that fails returns its own error and poisons the
/// transaction: every later write returns [`Error::TransactionAborted`], and
/// so does [`Txn::commit`], which then applies nothing. A no-op is not a
/// failure.
#[derive(Debug)]
pub struct Txn<'w> {
    pub(super) world: &'w mut World,
    /// The world's latest tick when the transaction began: the tick of a
    /// write with no tick of its own and no transaction default.
    base_tick: Tick,
    default_tick: Option<Tick>,
    /// The tx this transaction commits as. Fixed at begin: the `Txn` holds
    /// the world mutably, so no other commit can take it first.
    pub(super) tx: TxId,
    /// Staged entries (facts, `DELETED` markers, revised facts), keyed exactly as they
    /// will be in the `E` index (with `tx`), so judging seeks the committed
    /// and staged entries the same way.
    pub(super) staged: Index,
    /// The staged entries in write order: the order of their log entries.
    pub(super) entries: Vec<Staged>,
    /// Entities created and edges interned by this transaction.
    pub(super) ids: StagedIds,
    /// Set by the first failed write; the transaction then applies nothing.
    poisoned: bool,
    pub(super) schema_before: Option<SchemaSnapshot>,
    pub(super) schema_changes: Vec<SchemaChange>,
    pub(super) order: Vec<OrderedEntry>,
}

impl Txn<'_> {
    /// Find a handle of the requested type, or stage a new entity. The returned
    /// boolean says whether a definition was staged. Type mismatches poison the
    /// transaction just like ordinary duplicate-handle errors.
    pub fn find_or_create_entity(
        &mut self,
        node_type: &str,
        handle: &str,
    ) -> Result<(EntityId, bool), Error> {
        self.guard(|tx| {
            let expected = *tx
                .world
                .catalog
                .node_type_by_name
                .get(node_type)
                .ok_or_else(|| Error::UnknownNodeType(node_type.to_owned()))?;
            if let Some(id) = tx.entity_by_handle(handle) {
                if id.node_type() != expected {
                    return Err(Error::HandleTypeMismatch {
                        handle: handle.to_owned(),
                        node_type: tx.world.catalog.node_types[usize::from(id.node_type().0)]
                            .name
                            .clone(),
                    });
                }
                return Ok((id, false));
            }
            tx.create_entity(node_type, Some(handle))
                .map(|id| (id, true))
        })
    }

    /// Values held at a tick, including earlier staged operations. This is a
    /// read only: language-level replacement still writes explicit primitives.
    /// Distinct-value walks and seeks charge `meter`; stop before retaining more
    /// than `limit` values or `max_bytes` encoded bytes (at most 16 MiB).
    pub fn values_at(
        &self,
        entity: EntityId,
        attr: &str,
        tick: Tick,
        limit: usize,
        max_bytes: usize,
        meter: &WorkMeter<'_>,
    ) -> Result<Vec<Value>, Error> {
        if self.poisoned {
            return Err(Error::TransactionAborted);
        }
        let (attr_id, def) = self.world.catalog.resolve(&self.ids, entity, attr)?;
        let work = Work(Some(meter));
        let layers = [&self.world.indexes.e_index, &self.staged];
        let mut values = BTreeMap::new();
        let mut bytes = 0usize;
        let mut error = None;
        let mut keep = |encoding: &[u8]| {
            if error.is_some() || values.contains_key(encoding) {
                return;
            }
            if values.len() >= limit
                || bytes.saturating_add(encoding.len()) > max_bytes.min(crate::MAX_ENTRY_BYTES)
            {
                error = Some(Error::WorkInterrupted {
                    reason: WorkInterruptReason::Limit,
                });
                return;
            }
            match Value::decode(encoding) {
                Ok(value) => {
                    bytes += encoding.len();
                    values.insert(encoding.to_vec(), value);
                }
                Err(e) => error = Some(Error::Value(e)),
            }
        };
        let mut read = |prefix: &mut Vec<u8>| {
            if !holds(&layers, prefix, tick, TxId(u64::MAX), work) {
                return;
            }
            let part = &prefix[key::E_ATTR_PREFIX_LEN..];
            let encoding = if def.value_type == ValueType::Str && !def.indexed {
                key::ValueRef::from_key_bytes(part).and_then(|r| {
                    self.ids
                        .values
                        .resolve(r)
                        .or_else(|| self.world.indexes.values.resolve(r))
                })
            } else {
                Some(part)
            };
            if let Some(encoding) = encoding {
                keep(encoding);
            }
        };
        let prefix = key::e_attr_prefix(entity, attr_id);
        for index in layers {
            index::for_each_history(
                index,
                &prefix,
                |k| key::E_ATTR_PREFIX_LEN + key::e_value_bytes(k).map_or(0, |v| v.len()),
                work,
                &mut read,
            );
        }
        if self.world.catalog.is_bidirectional(attr_id) {
            // Committed reverse histories identify partners. Judge their E keys
            // against both layers so staged retracts/corrections are visible.
            let mut reverse = |source: EntityId| {
                let mut prefix = key::e_value_prefix(
                    source,
                    attr_id,
                    &Value::Ref(entity).encode().expect("ref encoding"),
                );
                if holds(&layers, &mut prefix, tick, TxId(u64::MAX), work) {
                    keep(&Value::Ref(source).encode().expect("ref encoding"));
                }
            };
            index::for_each_history(
                &self.world.indexes.r_index,
                &key::r_attr_prefix(entity, attr_id),
                |_| key::R_SOURCE_PREFIX_LEN,
                work,
                |p| {
                    if let Some(source) = key::r_source(p) {
                        reverse(source);
                    }
                },
            );
            for entry in &self.entries {
                if !work.add(1) {
                    break;
                }
                let (fact, _) = entry.key_and_mark();
                if fact.attr == attr_id && fact.target == Some(entity) {
                    reverse(fact.entity);
                }
            }
        }
        meter.check()?;
        if let Some(error) = error {
            return Err(error);
        }
        Ok(values.into_values().collect())
    }

    /// The entry currently occupying a value's exact tick: one this transaction
    /// staged earlier, else a committed one. Used to explain same-tick errors.
    /// A staged entry is reported by position, never by a provisional entry ID.
    pub fn entry_at_tick(
        &self,
        entity: EntityId,
        attr: &str,
        value: &Value,
        tick: Tick,
        meter: &WorkMeter<'_>,
    ) -> Result<Option<TickOccupant>, Error> {
        let fact = self.world.prepare_fact(
            &self.ids,
            FactRequest {
                entity,
                attr,
                value,
                tick,
                op: Op::Assert,
                precision: Precision::Exact,
                deprecated: DeprecatedWrites::Correction,
            },
        )?;
        let work = Work(Some(meter));
        let mut prefix = key::e_value_prefix(fact.entity, fact.attr, fact.key_value());
        key::push_seek_tick(&mut prefix, tick);
        if !work.add(1) {
            meter.check()?;
        }
        if self
            .staged
            .scan_from(&prefix)
            .next()
            .is_some_and(|(k, _)| k.starts_with(&prefix))
        {
            for (position, entry) in self.entries.iter().enumerate().rev() {
                if !work.add(1) {
                    meter.check()?;
                }
                let (f, mark) = entry.key_and_mark();
                if (f.entity, f.attr, &f.value, f.tick)
                    == (fact.entity, fact.attr, &fact.value, tick)
                {
                    return Ok(mark.op().map(|op| TickOccupant::Staged { position, op }));
                }
            }
        }
        if !work.add(1) {
            meter.check()?;
        }
        let Some((key, mark)) = self
            .world
            .indexes
            .e_index
            .scan_from(&prefix)
            .next()
            .filter(|(k, _)| k.starts_with(&prefix))
        else {
            return Ok(None);
        };
        if mark.op().is_none() {
            return Ok(None);
        }
        let Some(tx) = key::e_key_tx(key) else {
            return Ok(None);
        };
        let entries = self.world.journal.entries();
        // Locate the owning transaction without walking unrelated later history.
        let start = entries.partition_point(|e| e.tx < tx);
        let end = entries.partition_point(|e| e.tx <= tx);
        for entry in entries[start..end].iter().rev() {
            if !work.add(1) {
                meter.check()?;
            }
            let (e, a, v, t, op) = match &entry.body {
                EntryBody::Fact {
                    entity,
                    attr,
                    value,
                    tick,
                    op,
                    ..
                }
                | EntryBody::Revised {
                    entity,
                    attr,
                    value,
                    tick,
                    op,
                    ..
                } => (*entity, *attr, value, *tick, Some(*op)),
                EntryBody::Deleted {
                    entity,
                    attr,
                    value,
                    tick,
                    ..
                } => (*entity, *attr, value, *tick, None),
                _ => continue,
            };
            let matches_value = match fact.target {
                Some(target) => v == &Value::Ref(target),
                None => v == value,
            };
            if e == fact.entity && a == fact.attr && t == tick && matches_value {
                return Ok(op.map(|op| TickOccupant::Committed {
                    entry: entry.id,
                    op,
                }));
            }
        }
        Ok(None)
    }

    /// Resolve an optional tick using the transaction's stable defaults.
    pub fn write_tick(&self, tick: Option<Tick>) -> Tick {
        tick.or(self.default_tick).unwrap_or(self.base_tick)
    }
    /// Number of primitive entries staged so far, for language report offsets.
    pub fn staged_entry_count(&self) -> usize {
        self.entries.len()
    }
    /// Poison the transaction after an adapter error; partial forms cannot commit.
    pub fn abort(&mut self) {
        self.poisoned = true;
    }
    /// Current committed correction target (staged corrections of it are refused).
    pub fn current_change(&self, entry: EntryId) -> Result<Change, Error> {
        self.world.current_change(entry)
    }

    /// Sets the tick for this transaction's writes that don't give their
    /// own (`AT TICK` for the transaction). It wins over the world's latest
    /// tick; a write's own tick wins over it.
    pub fn set_default_tick(&mut self, t: Tick) {
        self.default_tick = Some(t);
    }

    /// Stages a new entity of the given node type, with an optional handle,
    /// and returns its ID.
    ///
    /// The ID and handle are assigned as by [`World::create_entity`],
    /// counting this transaction's earlier creates: sequences continue from
    /// them and handles must differ from them. Later writes in this
    /// transaction can use the entity as subject or `ref` value. It exists
    /// for everyone else only after [`Txn::commit`]. Fails (poisoning the
    /// transaction) with [`Error::UnknownNodeType`],
    /// [`Error::InvalidHandle`], [`Error::DuplicateHandle`] or
    /// [`Error::Id`].
    pub fn create_entity(
        &mut self,
        node_type: &str,
        handle: Option<&str>,
    ) -> Result<EntityId, Error> {
        self.guard(|txn| {
            let def = txn
                .world
                .catalog
                .prepare_entity(&txn.ids, node_type, handle)?;
            let id = def.id;
            txn.order.push(OrderedEntry::Entity(txn.ids.defs.len()));
            txn.ids.push(def);
            Ok(id)
        })
    }

    /// Runs one write: refused if the transaction is already poisoned or
    /// the world refuses writes (failed log), and poisons it if the write
    /// fails.
    pub(super) fn guard<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        if self.poisoned {
            return Err(Error::TransactionAborted);
        }
        let res = self.world.writable().and_then(|()| f(self));
        self.poisoned = res.is_err();
        res
    }

    /// Stages an assert of `v` for `entity.attr` from `tick` on.
    ///
    /// The tick is `tick`, else the transaction default, else the world's
    /// latest tick captured at [`World::begin`]; the result reports it.
    /// Validated and judged like [`World::assert`], with the transaction's
    /// own earlier writes counted as stored. Nothing is visible until
    /// [`Txn::commit`].
    pub fn assert(
        &mut self,
        entity: EntityId,
        attr: &str,
        v: Value,
        tick: Option<Tick>,
    ) -> Result<WriteResult, Error> {
        self.write(entity, attr, &v, tick, Op::Assert, Precision::Exact)
    }

    /// [`Txn::assert`] with the tick's precision: staged with `p`, judged
    /// exactly like [`Txn::assert`] (see [`World::assert_with_precision`]).
    pub fn assert_with_precision(
        &mut self,
        entity: EntityId,
        attr: &str,
        v: Value,
        tick: Option<Tick>,
        p: Precision,
    ) -> Result<WriteResult, Error> {
        self.write(entity, attr, &v, tick, Op::Assert, p)
    }

    /// [`Txn::retract`] with the tick's precision: staged with `p`, judged
    /// exactly like [`Txn::retract`].
    pub fn retract_with_precision(
        &mut self,
        entity: EntityId,
        attr: &str,
        v: Value,
        tick: Option<Tick>,
        p: Precision,
    ) -> Result<WriteResult, Error> {
        self.write(entity, attr, &v, tick, Op::Retract, p)
    }

    /// Stages a retract of `v` for `entity.attr` from `tick` on.
    ///
    /// Ticks default as for [`Txn::assert`]. Validated and judged like
    /// [`World::retract`], with the transaction's own earlier writes
    /// counted as stored.
    pub fn retract(
        &mut self,
        entity: EntityId,
        attr: &str,
        v: Value,
        tick: Option<Tick>,
    ) -> Result<WriteResult, Error> {
        self.write(entity, attr, &v, tick, Op::Retract, Precision::Exact)
    }

    /// The edge interned for `(source, attr, target)`, as
    /// [`World::edge_id`] but also seeing edges interned earlier in this
    /// transaction.
    pub fn edge_id(&self, source: EntityId, attr: &str, target: EntityId) -> Option<EdgeId> {
        let attr = *self.world.catalog.attr_by_name.get(attr)?;
        self.world.catalog.edge_of(&self.ids, source, attr, target)
    }

    /// Resolves the tick, validates, judges against committed + staged
    /// facts, and stages the fact unless it is a no-op.
    fn write(
        &mut self,
        entity: EntityId,
        attr: &str,
        v: &Value,
        tick: Option<Tick>,
        op: Op,
        precision: Precision,
    ) -> Result<WriteResult, Error> {
        self.guard(|txn| {
            let tick = tick.or(txn.default_tick).unwrap_or(txn.base_tick);
            let fact = txn.world.prepare_fact(
                &txn.ids,
                FactRequest {
                    entity,
                    attr,
                    value: v,
                    tick,
                    op,
                    // A payload only: judged below exactly as an exact fact.
                    precision,
                    deprecated: DeprecatedWrites::Refuse,
                },
            )?;
            check_entry_size(attr, &fact)?;
            if judge(&txn.world.indexes.e_index, &txn.staged, &fact, entity, attr)? == Verdict::Noop
            {
                // Find-then-act: nothing to change, so nothing is staged.
                return Ok(WriteResult {
                    changed: 0,
                    tick,
                    entries: Vec::new(),
                    edges: Vec::new(),
                    warnings: Warnings::default(),
                    schema: Vec::new(),
                });
            }
            // A stored retract never needs an edge: it requires a stored
            // assert, which interned it.
            txn.intern_edge(&fact)?;
            txn.stage(Staged::Fact(fact));
            Ok(WriteResult {
                changed: 1,
                tick,
                entries: Vec::new(),
                edges: Vec::new(),
                warnings: Warnings::default(),
                schema: Vec::new(),
            })
        })
    }

    /// Stages a correction of the committed fact entry `entry`: see
    /// [`World::correct`]. Nothing is visible until [`Txn::commit`], whose
    /// result lists the correction's entry IDs (this result's `entries` is
    /// empty).
    ///
    /// Fails (poisoning the transaction) as [`World::correct`] does, judged
    /// against the committed world plus this transaction's staged entries,
    /// and with [`Error::TxnKeyConflict`] if this transaction already staged
    /// an entry at the corrected key (e.g. it corrected the same chain
    /// before, even if this correction would be a no-op) or at a replaced
    /// fact's new key.
    pub fn correct(&mut self, entry: EntryId, c: Correction) -> Result<WriteResult, Error> {
        self.guard(|txn| {
            let target = txn.world.correction_target(entry)?;
            let attr = txn.world.catalog.attributes[target.fact.attr.0 as usize]
                .name
                .clone();
            txn.check_correctable(&target, &attr)?;
            let (value, tick, op, precision) = match c {
                Correction::Delete => return txn.stage_deletion(target),
                Correction::Replace {
                    value,
                    tick,
                    op,
                    precision,
                } => (value, tick, op, precision),
            };
            // The new fact is the current one with the given fields
            // replaced, validated like a write. The subject is the current
            // fact's (stored) entity; a bidirectional pair is renormalised,
            // which may swap it with the value.
            let current = &target.fact;
            let new = txn.world.prepare_fact(
                &txn.ids,
                FactRequest {
                    entity: current.entity,
                    attr: &attr,
                    value: value.as_ref().unwrap_or(&target.value),
                    tick: tick.unwrap_or(current.tick),
                    op: op.unwrap_or(current.op),
                    // Kept unless given, also when the key moves.
                    precision: precision.unwrap_or(current.precision),
                    deprecated: DeprecatedWrites::Correction,
                },
            )?;
            check_entry_size(&attr, &new)?;
            let same_key = (new.entity, &new.value, new.tick)
                == (current.entity, &current.value, current.tick);
            if same_key {
                txn.revise_in_place(target, new)
            } else {
                txn.move_fact(target, new, attr)
            }
        })
    }

    /// The checks every correction of `target` (a chain's current fact, of
    /// the attribute named `attr`) passes before anything is staged.
    fn check_correctable(&self, target: &Target, attr: &str) -> Result<(), Error> {
        let current = &target.fact;
        // Judged against the committed chain: if this transaction already
        // corrected it, that state is stale, so refuse before anything
        // else (a "no-op" against stale state could be a real change).
        if has_key(&self.staged, current) {
            return Err(Error::TxnKeyConflict {
                entity: current.entity,
                attr: attr.to_owned(),
                tick: current.tick,
            });
        }
        // Every correction entry carries the current value (a `DELETED`
        // marker) or a value derived from it, so the current value must
        // fit a correction entry before anything is staged. A value that
        // fit only as a plain fact (a crafted log) is refused here with
        // the typed error, not later by the log writer.
        check_entry_size(attr, current)
    }

    /// [`Correction::Delete`]: stages a `DELETED` marker at the key of the
    /// chain's current fact.
    fn stage_deletion(&mut self, target: Target) -> Result<WriteResult, Error> {
        let tick = target.fact.tick;
        self.stage(Staged::Deleted {
            of: target.base,
            rev: target.next_rev()?,
            fact: target.fact,
        });
        Ok(written(1, tick))
    }

    /// A replace that keeps the key: stages `new` as the chain's `Revised`
    /// entry, unless nothing changed.
    fn revise_in_place(&mut self, target: Target, new: FactDef) -> Result<WriteResult, Error> {
        let tick = new.tick;
        if (new.op, new.precision) == (target.fact.op, target.fact.precision) {
            // Nothing changed: find-then-act, nothing staged.
            return Ok(written(0, tick));
        }
        // The chain's own entry is the current one at its key, so
        // changing the op or the precision there can't conflict
        // with anything else.
        self.intern_edge(&new)?;
        self.stage(Staged::Revised {
            of: target.base,
            rev: target.next_rev()?,
            fact: new,
        });
        Ok(written(1, tick))
    }

    /// A replace that moves the key (tick or value): stages a `DELETED`
    /// marker at the old key and, unless the new key already says the same,
    /// the `Revised` fact `new` at the new one. `attr` names the attribute
    /// for the errors.
    fn move_fact(
        &mut self,
        target: Target,
        new: FactDef,
        attr: String,
    ) -> Result<WriteResult, Error> {
        let tick = new.tick;
        let of = target.base;
        // A moved key: the same-tick rule at the new key, judged at the
        // latest canon plus the staged entries. The hold-based no-op
        // rules don't apply: this changes what canon says, not the story.
        let layers = [&self.world.indexes.e_index, &self.staged];
        let mut prefix = key::e_value_prefix(new.entity, new.attr, new.key_value());
        let there = op_at_tick(&layers, &mut prefix, new.tick, TxId(u64::MAX));
        match there {
            Some(stored) if stored != new.op => Err(Error::SameTickConflict {
                entity: new.entity,
                attr,
                tick,
            }),
            Some(_) => {
                // The new key already says this: only the old key goes.
                self.stage(Staged::Deleted {
                    of,
                    rev: target.next_rev()?,
                    fact: target.fact,
                });
                Ok(written(1, tick))
            }
            None => {
                if has_key(&self.staged, &new) {
                    return Err(Error::TxnKeyConflict {
                        entity: new.entity,
                        attr,
                        tick,
                    });
                }
                let rev = target.next_rev()?;
                self.intern_edge(&new)?;
                self.stage(Staged::Deleted {
                    of,
                    rev,
                    fact: target.fact,
                });
                self.stage(Staged::Revised { of, rev, fact: new });
                Ok(written(2, tick))
            }
        }
    }

    /// Stages the edge a stored assert of the `ref` fact `fact` interns, if
    /// its triple has none yet (committed or staged). Retracts and non-ref
    /// facts intern nothing.
    fn intern_edge(&mut self, fact: &FactDef) -> Result<(), Error> {
        if let (Some(target), Op::Assert) = (fact.target, fact.op)
            && let Some(edge) =
                self.world
                    .catalog
                    .prepare_edge(&self.ids, fact.entity, fact.attr, target)?
        {
            self.order.push(OrderedEntry::Edge(self.ids.edges.len()));
            self.ids.push_edge(edge);
        }
        Ok(())
    }

    /// Stages one entry: its `E` key in the staged index (so later writes
    /// in the transaction are judged against it) and the entry itself, in
    /// write order.
    pub(super) fn stage(&mut self, entry: Staged) {
        let (fact, mark) = entry.key_and_mark();
        // A value new to the world is held here until the commit applies it.
        if let Some(r) = fact.value_ref()
            && self.world.indexes.values.get(&fact.value).is_none()
        {
            self.ids.values.insert(&fact.value, r);
        }
        let k = key::e_fact_key(fact.entity, fact.attr, fact.key_value(), fact.tick, self.tx);
        self.staged.insert(k, mark);
        self.order.push(OrderedEntry::Fact(self.entries.len()));
        self.entries.push(entry);
    }

    /// Makes every staged entity, write and correction visible as one new
    /// transaction and returns its tx and the number of entries stored.
    ///
    /// With nothing staged (only no-ops), nothing is appended and the tx
    /// doesn't advance; a transaction that only creates entities does
    /// advance it. Fails with [`Error::TransactionAborted`], applying
    /// nothing, if a write in the transaction failed.
    ///
    /// In a file-backed world entries are appended in operation order, with
    /// every ID defined before use and the `COMMIT` last, in one write and
    /// fsynced, and only then applied in memory. If that fails, the commit
    /// returns [`Error::Io`], nothing is applied, and the world refuses
    /// further writes.
    pub fn commit(self) -> Result<CommitResult, Error> {
        self.commit_with(true)
    }

    /// [`Txn::commit`] without the write warnings: the returned
    /// [`CommitResult::warnings`] is always empty. Everything stored (log
    /// bytes, entry and edge IDs, tx numbering) is identical to `commit`.
    ///
    /// For the fixture loader, which discards warnings: computing them
    /// rescans each touched cardinality-one side's whole history and is
    /// quadratic in the overlapping values one transaction touches
    /// (security review H1/H2).
    pub fn commit_quiet(self) -> Result<CommitResult, Error> {
        self.commit_with(false)
    }

    /// The body of [`Txn::commit`]; `warn` selects whether the overlap
    /// warnings diff is computed. Nothing else depends on it.
    fn commit_with(mut self, warn: bool) -> Result<CommitResult, Error> {
        if self.poisoned {
            return Err(Error::TransactionAborted);
        }
        self.world.accepts_writes()?;
        if self.order.is_empty() {
            return Ok(CommitResult {
                committed: false,
                tx: self.world.latest_tx(),
                changed: 0,
                declared: 0,
                entries: Vec::new(),
                edges: Vec::new(),
                warnings: Warnings::default(),
                schema: Vec::new(),
            });
        }
        let encoded = self.encode_ordered()?;
        let scope = self.warning_scope(warn);
        // The staged schema edits were applied provisionally; the world is
        // the committed one again until the log has the transaction.
        if let Some(before) = self.schema_before.take() {
            before.restore(&mut self.world.catalog);
        }
        let before = OverlapsBefore::capture(scope, self.world);
        self.world.journal.append(&encoded)?;
        let entries = self.apply(&encoded);
        self.world.journal.set_latest_tx(self.tx);
        self.world.journal.record(encoded.entries, self.tx);
        let warnings = before.warnings(self.world);
        Ok(CommitResult {
            committed: true,
            tx: self.tx,
            changed: self.entries.len(),
            declared: self.schema_changes.len(),
            entries,
            edges: self.ids.edges.iter().map(|d| d.id).collect(),
            warnings,
            schema: std::mem::take(&mut self.schema_changes),
        })
    }

    /// Which overlap checks this commit's warnings compare before and
    /// after. Read while the transaction's schema edits are still applied.
    fn warning_scope(&self, warn: bool) -> WarningScope {
        if !warn {
            return WarningScope::Off;
        }
        // Only a schema edit that can change overlaps on sides no staged fact
        // touches (a relabel, rename or migration) needs the whole-world scan;
        // new declarations, node types, deprecations and settings keep the
        // touched-sides diff, which costs what the transaction wrote.
        let schema_changed = self.schema_before.as_ref().is_some_and(|before| {
            before.changes_overlaps(&self.world.catalog)
                || self
                    .order
                    .iter()
                    .any(|e| matches!(e, OrderedEntry::Schema(SchemaEdit::Migration { .. })))
        });
        if schema_changed {
            return WarningScope::World;
        }
        // Touched sides are named and oriented with this transaction's schema
        // applied, so facts on an attribute it declares can be sorted; that
        // schema is the one `after` reads.
        WarningScope::Sides(
            self.world
                .catalog
                .touched_side_values(self.entries.iter().map(|e| e.key_and_mark().0)),
        )
    }

    /// Applies every staged operation to the world in operation order,
    /// after the log has them as `encoded` (one logged entry per
    /// operation). Returns the IDs of the fact and correction entries, in
    /// log order.
    fn apply(&mut self, encoded: &EncodedTxn) -> Vec<EntryId> {
        let mut entries = Vec::new();
        let mut migrating = false;
        for (ordered, logged) in self.order.iter().zip(&encoded.entries) {
            match ordered {
                OrderedEntry::Schema(SchemaEdit::Migration { .. }) => {
                    migrating = true;
                }
                OrderedEntry::Schema(edit) => {
                    self.world
                        .apply_schema_edit(edit, self.tx, migrating)
                        .expect("invariant: staged schema edit was validated in operation order");
                }
                OrderedEntry::Entity(i) => {
                    let d = &self.ids.defs[*i];
                    self.world.catalog.apply_entity(
                        EntityDef {
                            id: d.id,
                            handle: d.handle.clone(),
                        },
                        self.tx,
                    );
                }
                OrderedEntry::Edge(i) => {
                    let d = &self.ids.edges[*i];
                    self.world.catalog.apply_edge(
                        EdgeDef {
                            id: d.id,
                            source: d.source,
                            attr: d.attr,
                            target: d.target,
                        },
                        self.tx,
                    );
                }
                OrderedEntry::Fact(i) => {
                    self.world
                        .apply_staged(&self.entries[*i], logged.id, self.tx);
                    entries.push(logged.id);
                }
            }
        }
        entries
    }
}

/// Which overlap checks a commit's write warnings compare before and after
/// it is applied.
#[derive(Debug)]
pub(super) enum WarningScope {
    /// No warnings ([`Txn::commit_quiet`]).
    Off,
    /// The sides the transaction's entries touch, with the values they
    /// change there: costs what the transaction wrote.
    Sides(Vec<TouchedSide>),
    /// Every attribute's overlaps: a schema edit may have changed overlaps
    /// on sides no staged fact touches.
    World,
}

type WarningKey = (EntityId, AttrId, u8);
type WarningIntervals = Vec<(Value, Vec<Interval>)>;
pub(super) type WarningSnapshot = BTreeMap<WarningKey, WarningIntervals>;
impl WarningScope {
    pub(super) fn intervals(&self, world: &World) -> WarningSnapshot {
        let view = world.at_canon(world.latest_tx());
        let mut snapshot = BTreeMap::new();
        let mut capture = |entity, attr, side| {
            let intervals = view.warning_intervals(entity, attr, side);
            // An overlap needs two values, so a side with fewer forms no pair
            // in this snapshot and the before/after diff is the same without
            // it (a side that grows from one value to two is compared against
            // an empty "before", where the single value had no pair either).
            // Not keeping it matters for the world scope, which would
            // otherwise hold every entity's decoded values (large unindexed
            // strings included) twice.
            if intervals.len() >= 2 {
                snapshot.insert((entity, attr, u8::from(side == Side::Reverse)), intervals);
            }
        };
        match self {
            Self::Off => {}
            Self::Sides(sides) => {
                for touched in sides {
                    let (e, a, s) = touched.side;
                    capture(e, a, s);
                }
            }
            Self::World => {
                for entity in world.catalog.defined_ids() {
                    for (i, def) in world.catalog.attributes.iter().enumerate() {
                        if def.kind != Kind::State {
                            continue;
                        }
                        let attr = AttrId(i as u32);
                        if def.cardinality == Cardinality::One {
                            capture(entity, attr, Side::Forward);
                        }
                        if def.reverse_cardinality == Some(Cardinality::One) {
                            capture(entity, attr, Side::Reverse);
                        }
                    }
                }
            }
        }
        snapshot
    }
}

/// Keep value intervals, never overlap pairs. Snapshot memory is linear in
/// history; the only overlap allocations are the capped result lists.
struct OverlapsBefore {
    scope: WarningScope,
    intervals: WarningSnapshot,
}
impl OverlapsBefore {
    fn capture(scope: WarningScope, world: &World) -> Self {
        Self {
            intervals: scope.intervals(world),
            scope,
        }
    }
    fn warnings(self, world: &World) -> Warnings {
        let after = self.scope.intervals(world);
        let touched: BTreeMap<_, _> = match &self.scope {
            WarningScope::Sides(sides) => sides
                .iter()
                .map(|t| {
                    let (e, a, s) = t.side;
                    ((e, a, u8::from(s == Side::Reverse)), t.values.as_slice())
                })
                .collect(),
            _ => BTreeMap::new(),
        };
        let mut keys: Vec<_> = self.intervals.keys().chain(after.keys()).copied().collect();
        keys.sort_unstable();
        keys.dedup();
        // Names are the latest namespace even for resolved warnings after rename.
        keys.sort_by(|a, b| {
            (a.0, world.attr_name(a.1), a.2).cmp(&(b.0, world.attr_name(b.1), b.2))
        });
        let mut warnings = Warnings::default();
        for key @ (entity, attr, side) in keys {
            let old = self
                .intervals
                .get(&key)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let new = after.get(&key).map(Vec::as_slice).unwrap_or_default();
            crate::overlap::diff_intervals(
                old,
                new,
                touched.get(&key).copied(),
                |introduced, a, b, during| {
                    warnings.push(introduced, || Overlap {
                        entity,
                        attr: world.attr_name(attr).unwrap_or_default().to_owned(),
                        side: if side == 0 {
                            Side::Forward
                        } else {
                            Side::Reverse
                        },
                        a: a.clone(),
                        b: b.clone(),
                        during,
                    });
                },
            );
        }
        warnings
    }
}

/// Refuses a live write of `fact` with [`Error::EntryTooLarge`] if a
/// `Revised` entry carrying its value would exceed
/// [`MAX_ENTRY_BYTES`](log::MAX_ENTRY_BYTES): every value is judged by the
/// largest entry it may ever need, so any accepted value can later be
/// deleted or replaced in place. Checked before anything is staged, so the
/// log never gets a frame a reader refuses. Replay doesn't check: its
/// entries already fit their frames.
pub(super) fn check_entry_size(attr: &str, fact: &FactDef) -> Result<(), Error> {
    let len = log::revised_payload_len(fact.value.len());
    if len > log::MAX_ENTRY_BYTES {
        return Err(Error::EntryTooLarge {
            attr: attr.to_owned(),
            len,
            max: log::MAX_ENTRY_BYTES,
        });
    }
    Ok(())
}

fn written(changed: usize, tick: Tick) -> WriteResult {
    WriteResult {
        changed,
        tick,
        entries: Vec::new(),
        edges: Vec::new(),
        warnings: Warnings::default(),
        schema: Vec::new(),
    }
}

/// Judges a validated fact against the committed `E` index plus a
/// transaction's `staged` entries, at the fact's own tick (never the latest
/// tick, so out-of-order writes are kept). A key whose current entry is a
/// `DELETED` marker holds nothing.
///
/// First the same-tick rule: if the opposite op is current for the same
/// `(entity, attr, value)` at exactly this tick, the write fails with
/// [`Error::SameTickConflict`] (tick order can't decide between them, and
/// letting tx order decide would break "truth follows tick order"). Then
/// the no-op rule: an assert of a value that already holds there, or a
/// retract of one that doesn't, changes nothing. Last, a write that would be
/// stored at a key the transaction already staged an entry at (only a
/// `DELETED` marker can get there) fails with [`Error::TxnKeyConflict`]: a
/// transaction stores at most one entry per key. `written` (the subject as
/// the caller wrote it, before any bidirectional normalisation) and `attr`
/// (the attribute's name) are for the errors.
fn judge(
    committed: &Index,
    staged: &Index,
    fact: &FactDef,
    written: EntityId,
    attr: &str,
) -> Result<Verdict, Error> {
    let layers = [committed, staged];
    let layers = &layers[..];
    let mut prefix = key::e_value_prefix(fact.entity, fact.attr, fact.key_value());
    // Writes are judged at the latest canon plus the transaction's own
    // staged facts, so every entry is visible.
    let all = TxId(u64::MAX);
    if let Some(stored) = op_at_tick(layers, &mut prefix, fact.tick, all)
        && stored != fact.op
    {
        return Err(Error::SameTickConflict {
            entity: written,
            attr: attr.to_owned(),
            tick: fact.tick,
        });
    }
    let holds = holds(layers, &mut prefix, fact.tick, all, Work::NONE);
    let noop = match fact.op {
        Op::Assert => holds,
        Op::Retract => !holds,
    };
    if noop {
        return Ok(Verdict::Noop);
    }
    if has_key(staged, fact) {
        return Err(Error::TxnKeyConflict {
            entity: fact.entity,
            attr: attr.to_owned(),
            tick: fact.tick,
        });
    }
    Ok(Verdict::Store)
}

impl World {
    /// Resolves the target of a correction of `entry` (a base `Fact` or any
    /// `Revised` entry of its chain): the chain's base entry, its state and
    /// its current fact (as stored, encoded). Changes nothing.
    pub(super) fn correction_target(&self, entry: EntryId) -> Result<Target, Error> {
        let info = self
            .journal
            .kept_entry(entry)
            .ok_or(Error::UnknownEntry(entry))?;
        let base = match info.body {
            EntryBody::Fact { .. } => entry,
            // A Revised entry always names its chain's base (replay checks it).
            EntryBody::Revised { of, .. } => of,
            _ => return Err(Error::NotAFact(entry)),
        };
        let chain = self.journal.chain(base);
        let current = chain.current.ok_or(Error::AlreadyDeleted(entry))?;
        let (entity, attr, value, tick, op, precision) = self.journal.current_fact(current);
        // A committed fact's value was encoded once before; it encodes.
        let mut fact = self.stored_fact_def(entity, attr, value, tick, op)?;
        fact.precision = precision;
        Ok(Target {
            base,
            chain,
            fact,
            value: value.clone(),
        })
    }

    /// Applies one staged entry of committed transaction `tx`, logged as
    /// entry `logged`: its index entries, and for a correction its chain's
    /// new state.
    fn apply_staged(&mut self, entry: &Staged, logged: EntryId, tx: TxId) {
        let (fact, mark) = entry.key_and_mark();
        self.index_entry(fact, mark, tx);
        match entry {
            Staged::Fact(_) => {}
            Staged::Deleted { of, rev, .. } => self.journal.set_chain(*of, *rev, None),
            Staged::Revised { of, rev, .. } => self.journal.set_chain(*of, *rev, Some(logged)),
        }
    }

    /// One auto-committed write: a transaction holding just this write.
    /// A no-op stages nothing, so its commit consumes no tx.
    pub(super) fn write(
        &mut self,
        entity: EntityId,
        attr: &str,
        v: &Value,
        tick: Option<Tick>,
        op: Op,
        precision: Precision,
    ) -> Result<WriteResult, Error> {
        let mut txn = self.begin();
        let mut res = txn.write(entity, attr, v, tick, op, precision)?;
        let commit = txn.commit()?;
        res.entries = commit.entries;
        res.edges = commit.edges;
        res.warnings = commit.warnings;
        Ok(res)
    }

    /// Validates a fact against the schema and the defined entities without
    /// changing the world.
    ///
    /// Entities staged by the writing transaction (`staged`) count as
    /// defined, both as the subject and as a `ref` value. A fact on a
    /// bidirectional attribute comes back normalised (see
    /// [`Catalog::stored_pair`]), so both spellings judge and store as one.
    ///
    /// The request names the deprecation rule: corrections may revise
    /// deprecated history; only a migration's own verified copy may append
    /// plain facts to deprecated attributes.
    pub(super) fn prepare_fact(
        &self,
        staged: &StagedIds,
        request: FactRequest<'_>,
    ) -> Result<FactDef, Error> {
        let FactRequest {
            entity,
            attr,
            value: v,
            tick,
            op,
            precision,
            deprecated,
        } = request;
        let (attr_id, def) = self.catalog.resolve(staged, entity, attr)?;
        if def.deprecated && deprecated == DeprecatedWrites::Refuse {
            return Err(Error::DeprecatedAttribute(attr.to_owned()));
        }
        check_type(attr, def, v)?;
        // Length check before encoding, so an oversized string isn't copied.
        if def.indexed {
            v.check_indexed()?;
        }
        let mut value = v.encode()?;
        if let Value::Ref(target) = v
            && !self.catalog.is_defined_staged(staged, *target)
        {
            return Err(Error::UnknownEntity(*target));
        }
        // Events are never retracted in story time, whatever is stored, so
        // this is a schema error: judged after input validation but before
        // the same-tick and no-op rules, which look at stored facts.
        if op == Op::Retract && def.kind == Kind::Event {
            return Err(Error::RetractEvent(attr.to_owned()));
        }
        let (entity, target) = match v {
            Value::Ref(target) => {
                let (source, target) = self.catalog.stored_pair(attr_id, entity, *target);
                if source != entity {
                    // Swapped: the value is now the caller's entity.
                    value = Value::Ref(target).encode()?;
                }
                (source, Some(target))
            }
            _ => (entity, None),
        };
        let vref = self.indexes.value_ref(staged, def, v, &value)?;
        Ok(FactDef {
            entity,
            attr: attr_id,
            value,
            tick,
            op,
            precision,
            target,
            vref,
        })
    }

    /// The [`FactDef`] (keys and op) of a stored entry, from its decoded
    /// fields: its value reference is the one the world's table holds.
    pub(super) fn stored_fact_def(
        &self,
        entity: EntityId,
        attr: AttrId,
        value: &Value,
        tick: Tick,
        op: Op,
    ) -> Result<FactDef, Error> {
        let encoded = value.encode()?;
        let vref = match self.catalog.attributes.get(attr.0 as usize) {
            Some(def) => self
                .indexes
                .value_ref(&StagedIds::default(), def, value, &encoded)?,
            None => None,
        };
        Ok(FactDef {
            entity,
            attr,
            value: encoded,
            tick,
            op,
            precision: Precision::Exact,
            target: match value {
                Value::Ref(target) => Some(*target),
                _ => None,
            },
            vref,
        })
    }
}

/// The overlap checks a commit touches: which `(entity, attr, side)` checks
/// a set of written keys can change. Decided by the schema alone.
impl Catalog {
    /// The `(entity, attr, side)` overlap checks the stored entries at the
    /// keys `facts` can change (write warnings): the fact's entity
    /// (Forward), for a `ref` fact its target (Reverse), and for a
    /// bidirectional one also the target's Forward side (its partners
    /// include the entity). A value-moving correction lists both its old
    /// and its new key, so both targets are covered. Sorted by `(entity,
    /// attr name, side)` with Forward first, deduplicated: the order the
    /// warnings are reported in. Cost: proportional to the entries, not the
    /// world.
    pub(super) fn touched_sides<'f>(
        &self,
        facts: impl Iterator<Item = &'f FactDef>,
    ) -> Vec<(EntityId, AttrId, Side)> {
        let mut touched = Vec::new();
        for f in facts {
            self.sides_of(f, |side, _| touched.push(side));
        }
        self.sort_sides(&mut touched, |s| *s);
        touched.dedup();
        touched
    }

    /// [`Catalog::touched_sides`], each side with the values the entries
    /// change on it (their encodings, ascending and deduplicated): on the
    /// entity's Forward side the fact's value, on the target's Reverse (and
    /// bidirectional Forward) side `Ref(entity)`. Only those values'
    /// intervals can differ before and after the commit.
    fn touched_side_values<'f>(
        &self,
        facts: impl Iterator<Item = &'f FactDef>,
    ) -> Vec<TouchedSide> {
        let mut pairs: Vec<((EntityId, AttrId, Side), Vec<u8>)> = Vec::new();
        for f in facts {
            self.sides_of(f, |side, value| pairs.push((side, value)));
        }
        self.sort_sides(&mut pairs, |p| p.0);
        let mut out: Vec<TouchedSide> = Vec::new();
        for (side, value) in pairs {
            match out.last_mut() {
                Some(last) if last.side == side => last.values.push(value),
                _ => out.push(TouchedSide {
                    side,
                    values: vec![value],
                }),
            }
        }
        for t in &mut out {
            t.values.sort_unstable();
            t.values.dedup();
        }
        out
    }

    /// Calls `push` with each `(entity, attr, side)` check the stored entry
    /// at `f`'s key can change and the value (encoded) it changes there.
    fn sides_of(&self, f: &FactDef, mut push: impl FnMut((EntityId, AttrId, Side), Vec<u8>)) {
        push((f.entity, f.attr, Side::Forward), f.value.clone());
        if let Some(target) = f.target {
            let source = || {
                Value::Ref(f.entity)
                    .encode()
                    .expect("invariant: a ref value always encodes")
            };
            push((target, f.attr, Side::Reverse), source());
            if self.is_bidirectional(f.attr) {
                push((target, f.attr, Side::Forward), source());
            }
        }
    }

    /// Sorts by `(entity, attr name, side)` with Forward first (stable, so
    /// equal sides keep their order): the order warnings are reported in.
    fn sort_sides<T>(&self, items: &mut [T], side: impl Fn(&T) -> (EntityId, AttrId, Side)) {
        let rank = |s: Side| match s {
            Side::Forward => 0u8,
            Side::Reverse => 1,
        };
        let name = |a: AttrId| self.attributes[a.0 as usize].name.as_str();
        items.sort_by(|x, y| {
            let (x, y) = (side(x), side(y));
            (x.0.raw(), name(x.1), rank(x.2)).cmp(&(y.0.raw(), name(y.1), rank(y.2)))
        });
    }
}

impl World {
    /// The current fact in an entry's correction chain, at the latest canon.
    /// Accepts the same entry IDs as [`World::correct`] and returns the same
    /// target errors (including a chain already deleted). This is a read only;
    /// it does not validate or stage a proposed replacement.
    pub fn current_change(&self, entry: EntryId) -> Result<Change, Error> {
        let target = self.correction_target(entry)?;
        Ok(Change {
            tick: target.fact.tick,
            op: target.fact.op,
            precision: target.fact.precision,
            value: target.value,
        })
    }

    /// Starts a write transaction.
    ///
    /// The world's [`latest tick`](World::latest_tick) is captured now: a
    /// write in the transaction with no tick of its own and no
    /// [`default`](Txn::set_default_tick) lands on it, so ticks written
    /// earlier in the same transaction don't move it. Nothing is visible
    /// until [`Txn::commit`]; dropping the transaction discards it.
    pub fn begin(&mut self) -> Txn<'_> {
        let base_tick = self.latest_tick();
        let tx = self.next_tx();
        Txn {
            world: self,
            base_tick,
            default_tick: None,
            tx,
            staged: OrderedMap::new(),
            entries: Vec::new(),
            ids: StagedIds::default(),
            poisoned: false,
            schema_before: None,
            schema_changes: Vec::new(),
            order: Vec::new(),
        }
    }
}
