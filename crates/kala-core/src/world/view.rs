//! [`View`]: every read, at a pinned canon.

use super::*;

/// A read-only view of a [`World`] pinned to one canon tx, from
/// [`World::at_canon`]: it reads only facts written by transactions `<=`
/// that tx, so it never sees half a transaction or anything committed later.
///
/// Entity existence is pinned too: an entity or edge whose `DEFINE` is in a
/// transaction after the view's canon doesn't exist there, so reading it
/// (as a subject of [`View::as_of`], [`View::as_of_value`],
/// [`View::as_of_detail`], [`View::history`] or [`View::neighbors`]) is
/// [`Error::UnknownEntity`], exactly as for an ID the world never defined.
/// At or after its defining tx it reads normally (with no facts until some
/// are written). Scans ([`View::attr_scan`], [`View::entities_of_type`])
/// simply leave such IDs out.
#[derive(Clone, Copy, Debug)]
pub struct View<'w> {
    pub(super) world: &'w World,
    /// The newest tx visible; never past the world's latest tx.
    pub(super) canon: TxId,
    /// Read the latest-revision indexes (`canon` is the latest tx) instead
    /// of the main ones with the canon skip.
    pub(super) latest: bool,
    /// Where reads count the index keys they examine ([`View::metered`]).
    pub(super) work: Work<'w>,
}

impl<'w> View<'w> {
    /// The world this view reads.
    pub fn world(&self) -> &'w World {
        self.world
    }

    /// This view (same world, canon and indexes), with every read adding
    /// the index keys it examines to `meter` (see [`WorkMeter`]). Successful
    /// answers and key counts are unchanged. The returned [`MeteredView`]
    /// also makes latest-tick and lazy entity reads fallible.
    pub fn metered<'m>(&self, meter: &'m WorkMeter<'m>) -> MeteredView<'m>
    where
        'w: 'm,
    {
        MeteredView {
            view: View {
                world: self.world,
                canon: self.canon,
                latest: self.latest,
                work: Work(Some(meter)),
            },
        }
    }

    /// Every entity of node type `node_type` defined in this canon (its
    /// `DEFINE` in a transaction `<=` the view's canon), ascending by
    /// sequence number (so by [`EntityId`]): `NodeScan` by type. Never an
    /// entity of another type, never an edge; an entity with no facts is
    /// listed. Node type names are exact (case-sensitive).
    ///
    /// Answered from the ID tables, not the facts: finding the canon's
    /// entities is one binary search, then the iterator yields each ID
    /// lazily. Fails with [`Error::UnknownNodeType`] for a type the schema
    /// doesn't declare; a declared type with no entities is empty.
    pub fn entities_of_type(
        &self,
        node_type: &str,
    ) -> Result<impl Iterator<Item = EntityId> + use<'w>, Error> {
        let world = self.world;
        let type_id = *world
            .catalog
            .node_type_by_name
            .get(node_type)
            .ok_or_else(|| Error::UnknownNodeType(node_type.to_owned()))?;
        let canon = self.canon;
        if world.catalog.node_origins[usize::from(type_id.0)][0].0 > canon {
            return Err(Error::UnknownNodeType(node_type.into()));
        }
        let defined_in = &world.catalog.node_types[usize::from(type_id.0)].defined_in;
        // Entities are defined in tx order, so those visible at the canon
        // are a prefix of the type's sequences.
        let visible = defined_in.partition_point(|&tx| tx <= canon) as u64;
        Ok((1..=visible).map(move |seq| {
            EntityId::new(type_id, seq).expect("invariant: assigned entity sequences fit an ID")
        }))
    }
}

impl View<'_> {
    /// The canon tx this view reads at.
    pub fn canon(&self) -> TxId {
        self.canon
    }

    /// Whether the entity or edge `id` was defined at this view's canon (its
    /// `DEFINE` is in a transaction `<=` the canon). False for an ID the
    /// world never defined. Constant time (one lookup in the ID tables).
    pub fn is_defined(&self, id: EntityId) -> bool {
        self.id_visible(id)
    }

    /// [`View::is_defined`].
    fn id_visible(&self, id: EntityId) -> bool {
        if let Some(edge) = id.as_edge() {
            return self.edge_visible(edge);
        }
        if id.is_edge() {
            return false;
        }
        id.seq()
            .checked_sub(1)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| {
                self.world
                    .catalog
                    .node_types
                    .get(usize::from(id.node_type().0))?
                    .defined_in
                    .get(i)
            })
            .is_some_and(|&tx| tx <= self.canon)
    }

    /// [`World::resolve`] for a read at this canon: a known attribute, and
    /// `entity` defined at the canon (else [`Error::UnknownEntity`]).
    pub(super) fn resolve(
        &self,
        entity: EntityId,
        attr: &str,
    ) -> Result<(AttrId, &AttrDef), Error> {
        let resolved = self.world.resolve(&StagedIds::default(), entity, attr)?;
        if !self.id_visible(entity) {
            return Err(Error::UnknownEntity(entity));
        }
        Ok((resolved.0, self.require_definition(resolved.0)?))
    }

    /// Whether the edge with ID `id` was defined at this view's canon (its
    /// `DEFINE` is in a transaction `<=` the canon).
    fn edge_visible(&self, id: EdgeId) -> bool {
        id.seq()
            .checked_sub(1)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| self.world.catalog.edges_defined_in.get(i))
            .is_some_and(|&tx| tx <= self.canon)
    }

    /// The triple `(source, attribute name, target)` of edge `id`, as
    /// [`World::edge`], but `None` if the edge's `DEFINE` is in a
    /// transaction after this view's canon.
    pub fn edge(&self, id: EdgeId) -> Option<(EntityId, String, EntityId)> {
        self.edge_visible(id).then(|| self.world.edge(id))?
    }

    /// The edge interned for `(source, attr, target)`, as [`World::edge_id`]
    /// (a bidirectional pair in either order), but `None` if the edge's
    /// `DEFINE` is in a transaction after this view's canon.
    pub fn edge_id(&self, source: EntityId, attr: &str, target: EntityId) -> Option<EdgeId> {
        self.world
            .edge_id(source, attr, target)
            .filter(|&id| self.edge_visible(id))
    }

    /// The latest tick at this canon: the highest tick of any fact (assert
    /// or retract) visible here, or `Tick(0)` if there is none. A fact that
    /// a correction visible here deleted (or moved away) doesn't count; one
    /// deleted only by a later correction does. At the latest canon this is
    /// [`World::latest_tick`].
    ///
    /// Cost: at the latest canon, one lookup in the world's per-tick counts.
    /// At an older canon, a walk over the whole `E` index (every stored
    /// entry), so linear in the size of the world: fine for milestone 0's
    /// in-memory worlds, not for a hot path.
    pub fn latest_tick(&self) -> Tick {
        if self.latest {
            return self.world.latest_tick();
        }
        let canon = self.canon;
        let mut best: Option<Tick> = None;
        // The key (history and tick) whose current entry at the canon was
        // already found: its older entries are skipped.
        let mut done: Option<&[u8]> = None;
        // Unbounded meters retain the old bulk-count fast path.
        let work = if self.work.0.is_some_and(|m| m.limit.is_none()) {
            self.work.add(self.world.indexes.e_index.len() as u64);
            Work::NONE
        } else {
            self.work
        };
        for (k, mark) in self.world.indexes.e_index.iter() {
            if !work.add(1) {
                break;
            }
            let at_key = key::without_tx(k);
            if done == Some(at_key) {
                continue;
            }
            // Within one key, entries are newest tx first, so the first
            // with `tx <= canon` is the key's current entry at the canon.
            let Some((tick, tx)) = key::e_key_tick_and_tx(k) else {
                continue;
            };
            if tx > canon {
                continue;
            }
            done = Some(at_key);
            if mark.op().is_some() && best.is_none_or(|b| tick > b) {
                best = Some(tick);
            }
        }
        best.unwrap_or(Tick(0))
    }

    /// Whether value `v` of `entity.attr` holds at tick `t` in this canon.
    ///
    /// Takes the latest op with tick `<= t` for that `(entity, attr, v)`
    /// among the visible transactions (ticks whose current entry is a
    /// `DELETED` marker skipped): it holds if that op is an assert, and not
    /// if it is a retract or there is none. Fails with
    /// [`Error::UnknownAttribute`], [`Error::UnknownEntity`],
    /// [`Error::WrongValueType`] for a value of another type than the
    /// attribute's, or [`Error::Value`] for NaN.
    pub fn as_of_value(
        &self,
        entity: EntityId,
        attr: &str,
        v: &Value,
        t: Tick,
    ) -> Result<bool, Error> {
        let world = self.world;
        let (attr_id, def) = self.resolve(entity, attr)?;
        check_type(attr, def, v)?;
        // A bidirectional pair is stored once, normalised: look it up there.
        let mut prefix = match v {
            Value::Ref(target) if world.is_bidirectional(attr_id) => {
                let (source, target) = world.stored_pair(attr_id, entity, *target);
                key::e_value_prefix(source, attr_id, &Value::Ref(target).encode()?)
            }
            _ => {
                let encoded = v.encode()?;
                // An unindexed value never stored has no history.
                let Some(part) = world.committed_key_part(attr_id, &encoded) else {
                    return Ok(false);
                };
                key::e_value_prefix(entity, attr_id, &part)
            }
        };
        Ok(self.holds_e(&mut prefix, t))
    }

    /// Whether the `E` history `value_prefix` (`E|e|a|v|`) holds at `t` in
    /// this canon: one seek in `E_latest` at the latest canon, else in the
    /// main `E` index with the canon skip. Reuses `value_prefix` as the seek
    /// key buffer; it is restored before returning.
    fn holds_e(&self, value_prefix: &mut Vec<u8>, t: Tick) -> bool {
        if self.latest {
            holds_latest(&self.world.indexes.e_latest, value_prefix, t, self.work)
        } else {
            holds(
                &[&self.world.indexes.e_index],
                value_prefix,
                t,
                self.canon,
                self.work,
            )
        }
    }

    /// Every value of `entity.attr` that holds at tick `t` in this canon,
    /// sorted by value encoding (the value's natural order within its type;
    /// for `ref`, entity ID order).
    ///
    /// For a bidirectional attribute these are `entity`'s partners from
    /// either side (each pair is stored once, under its smaller ID), each
    /// once, so both ends of a pair agree.
    ///
    /// Cost grows with the number of distinct values the attribute has ever
    /// had: one seek per value (plus skipped newer-tx entries at an older
    /// canon). Fails with [`Error::UnknownAttribute`] or
    /// [`Error::UnknownEntity`].
    pub fn as_of(&self, entity: EntityId, attr: &str, t: Tick) -> Result<Vec<Value>, Error> {
        let (attr_id, _) = self.resolve(entity, attr)?;
        Ok(self.values_holding(entity, attr_id, t))
    }

    /// The body of [`View::as_of`] for a resolved attribute: every value of
    /// `entity.attr` holding at `t`, sorted by value encoding (bidirectional
    /// partners from either side, each once). Shared with
    /// [`View::attr_scan`], so the two can't disagree.
    fn values_holding(&self, entity: EntityId, attr_id: AttrId, t: Tick) -> Vec<Value> {
        let world = self.world;
        if world.is_bidirectional(attr_id) {
            // Ref encoding order is entity ID order, so sorting the IDs
            // sorts by value encoding.
            let mut partners = Vec::new();
            self.for_each_partner_holding(entity, attr_id, true, true, t, |p| partners.push(p));
            partners.sort_unstable();
            partners.dedup();
            return partners.into_iter().map(Value::Ref).collect();
        }
        let mut out = Vec::new();
        self.for_each_e_value_holding(entity, attr_id, t, |value| {
            // An unresolvable key (only by tampering) is skipped.
            if let Some(v) = world.decode_e_part(attr_id, value) {
                out.push(v);
            }
        });
        if self.work.finish(Ok(())).is_err() {
            return Vec::new();
        }
        if world.keys_by_ref(attr_id) {
            // Value references sort by hash, not by value: restore the
            // value-encoding order every other attribute has by key order
            // (`Ord for Value` is that order).
            out.sort_unstable();
        }
        out
    }

    /// Every `(entity, value)` pair of `attr` that holds at tick `t` in this
    /// canon: the pairs whose latest op with tick `<= t` is an assert, with
    /// corrections honoured exactly as in [`View::as_of`] (it is the same
    /// code, run for every ID). Entities include edges (edge properties).
    /// Sorted ascending by `(entity, value)` ([`Value`]'s order), no
    /// duplicates.
    ///
    /// - A bidirectional attribute lists each pair in both orientations,
    ///   `(a, b)` and `(b, a)`; a self pair once.
    /// - An inverse name (`child_of` for `parent_of`) lists the base
    ///   attribute's pairs swapped, `(child, parent)`, read through `R`.
    /// - Unindexed string attributes return their full values.
    ///
    /// Cost: one `E` (or `R`) prefix seek per defined entity and edge ID,
    /// plus one seek per distinct value each has ever had for `attr`: linear
    /// in the world's ID count even for a rare attribute. Fine for
    /// milestone 0's small worlds; an attribute-first index would make it
    /// proportional to the attribute's own histories.
    ///
    /// Fails with [`Error::UnknownAttribute`] for a name that is neither an
    /// attribute nor an inverse name.
    pub fn attr_scan(&self, attr: &str, t: Tick) -> Result<Vec<(EntityId, Value)>, Error> {
        self.resolve_attribute(attr)?;
        let world = self.world;
        let mut out = Vec::new();
        if let Some(&attr_id) = world.catalog.attr_by_name.get(attr) {
            self.require_definition(attr_id)?;
            // IDs ascend and each entity's values come sorted, so `out` is
            // sorted and duplicate-free as built.
            for id in world.defined_ids() {
                if !self.work.id() {
                    break;
                }
                for v in self.values_holding(id, attr_id, t) {
                    out.push((id, v));
                }
            }
            return Ok(out);
        }
        let &attr_id = world
            .catalog
            .inverse_by_name
            .get(attr)
            .ok_or_else(|| Error::UnknownAttribute(attr.to_owned()))?;
        self.require_definition(attr_id)?;
        if self.require_definition(attr_id)?.inverse.as_deref() != Some(attr) {
            return Err(Error::UnknownAttribute(attr.into()));
        }
        // Inverse names belong to directed attributes only, so each stored
        // pair `source -> id` is read once, from `R|id|attr|`.
        for id in world.defined_ids() {
            if !self.work.id() {
                break;
            }
            let start = out.len();
            self.for_each_partner_holding(id, attr_id, false, true, t, |source| {
                out.push((id, Value::Ref(source)));
            });
            out[start..].sort_unstable();
        }
        Ok(out)
    }

    /// [`View::as_of`] with detail: for every value of `entity.attr` that
    /// holds at tick `t` in this canon, the [`Change`] that makes it hold,
    /// i.e. its latest op with tick `<= t` (an assert), with that op's tick
    /// and [`Precision`]. Sorted by value encoding, so the values are
    /// exactly [`View::as_of`]'s, in the same order; for a bidirectional
    /// attribute each partner once, from either side.
    ///
    /// Same cost as [`View::as_of`]: one seek per value the attribute has
    /// ever had. Fails with [`Error::UnknownAttribute`] or
    /// [`Error::UnknownEntity`].
    pub fn as_of_detail(
        &self,
        entity: EntityId,
        attr: &str,
        t: Tick,
    ) -> Result<Vec<Change>, Error> {
        let world = self.world;
        let (attr_id, _) = self.resolve(entity, attr)?;
        // (value encoding, change), sorted by the encoding at the end.
        let mut out: Vec<(Vec<u8>, Change)> = Vec::new();
        self.for_each_e_history(entity, attr_id, |p| {
            let Some((k, Op::Assert)) =
                self.entry_at(&world.indexes.e_index, &world.indexes.e_latest, p, t)
            else {
                return;
            };
            // An unresolvable or mistyped key (only by tampering) is skipped.
            let Some((bytes, value)) = world.decode_e_value(attr_id, &p[key::E_ATTR_PREFIX_LEN..])
            else {
                return;
            };
            let change = Change {
                tick: tick_after(k, p.len()),
                op: Op::Assert,
                value,
                precision: self.precision_at(k),
            };
            out.push((bytes.to_vec(), change));
        });
        if world.is_bidirectional(attr_id) {
            // The pairs stored under a smaller partner, through their R
            // mirrors; a self pair was found on the E side.
            let prefix = key::r_attr_prefix(entity, attr_id);
            let source_end = |_: &[u8]| key::R_SOURCE_PREFIX_LEN;
            let mut on_history = |p: &mut Vec<u8>| {
                let Some((k, Op::Assert)) =
                    self.entry_at(&world.indexes.r_index, &world.indexes.r_latest, p, t)
                else {
                    return;
                };
                let Some(source) = key::r_source(p).filter(|s| *s != entity) else {
                    return;
                };
                let tick = tick_after(k, p.len());
                // A latest-revision key has no tx (and the latest path
                // ignores it); a main R key ends in its entry's.
                let tx = key::e_key_tx(k).filter(|_| !self.latest);
                let value = Value::Ref(source);
                let bytes = value
                    .encode()
                    .expect("invariant: a ref value always encodes");
                let change = Change {
                    tick,
                    op: Op::Assert,
                    value,
                    precision: self.r_precision(
                        source,
                        attr_id,
                        entity,
                        tick,
                        tx.unwrap_or(TxId(0)),
                    ),
                };
                out.push((bytes, change));
            };
            if self.latest {
                for_each_history(
                    &world.indexes.r_latest,
                    &prefix,
                    source_end,
                    self.work,
                    &mut on_history,
                );
            } else {
                for_each_history(
                    &world.indexes.r_index,
                    &prefix,
                    source_end,
                    self.work,
                    &mut on_history,
                );
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.dedup_by(|a, b| a.0 == b.0);
        Ok(out.into_iter().map(|(_, c)| c).collect())
    }

    /// The key and op of the current entry of the latest tick `<= t` in
    /// the history `value_prefix` (`E|e|a|v|` or `R|t|a|s|`) of the index
    /// this view reads: `latest` at the latest canon, else `main` with the
    /// canon skip (`DELETED` ticks skipped). Reuses `value_prefix` as the
    /// seek key buffer; it is restored before returning.
    fn entry_at<'i>(
        &self,
        main: &'i Index,
        latest: &'i LatestIndex,
        value_prefix: &mut Vec<u8>,
        t: Tick,
    ) -> Option<(&'i [u8], Op)> {
        if self.latest {
            return latest_seek(latest, value_prefix, t, self.work);
        }
        let len = value_prefix.len();
        key::push_seek_tick(value_prefix, t);
        let found = seek_entry(
            &[main],
            value_prefix,
            &value_prefix[..len],
            self.canon,
            self.work,
        );
        value_prefix.truncate(len);
        found
    }

    /// Every entity for which `attr = v` holds at tick `t` in this canon,
    /// ascending by [`EntityId`], each once: "the entity named Edwin".
    ///
    /// Holding is judged as in [`View::as_of_value`]: the latest op with
    /// tick `<= t` for `(e, attr, v)` among the visible transactions
    /// (`DELETED` ticks skipped) is an assert, so an event holds from its
    /// tick on. An edge property finds the edge's entity ID.
    ///
    /// A non-ref attribute is answered from the `V` index (`V_latest` at
    /// the latest canon, the main `V` with the canon skip at an older one):
    /// one seek per entity that ever had `attr = v`. A `ref` attribute is
    /// answered from `R`, whatever its `indexed` flag: for `Ref(x)`, every
    /// source `s` of `(s, attr, x)`, and for a bidirectional attribute the
    /// partners of `x` on either side.
    ///
    /// Fails with [`Error::UnknownAttribute`] (an inverse name included),
    /// [`Error::NotIndexed`] for a non-ref attribute declared unindexed,
    /// [`Error::WrongValueType`], or [`Error::Value`] for NaN. `-0.0` finds
    /// `0.0`, as it is stored. A string too long to be indexed, or a `Ref`
    /// to an undefined entity, can't have been written, so it finds nothing.
    pub fn lookup(&self, attr: &str, v: &Value, t: Tick) -> Result<Vec<EntityId>, Error> {
        let world = self.world;
        let attr_id = *world
            .catalog
            .attr_by_name
            .get(attr)
            .ok_or_else(|| Error::UnknownAttribute(attr.to_owned()))?;
        let def = self.require_definition(attr_id)?;
        if def.value_type != ValueType::Ref && !def.indexed {
            return Err(Error::NotIndexed(attr.to_owned()));
        }
        check_type(attr, def, v)?;
        // Encoding rejects NaN and normalises -0.0, exactly as a write does.
        let encoded = v.encode()?;
        let mut out = Vec::new();
        match v {
            Value::Ref(x) => {
                if !world.is_defined_staged(&StagedIds::default(), *x) {
                    return Ok(out);
                }
                let both = world.is_bidirectional(attr_id);
                self.for_each_partner_holding(*x, attr_id, both, true, t, |s| out.push(s));
                // Two sides (and a self pair, stored under both) may repeat.
                out.sort_unstable();
                out.dedup();
            }
            _ => {
                if v.check_indexed().is_err() {
                    return Ok(out);
                }
                let prefix = key::v_value_prefix(attr_id, &encoded);
                // V keys are fixed-width from the value on: `V|a|v|e|` is
                // the history of one entity, in entity order.
                let entity_end = |k: &[u8]| (prefix.len() + key::V_ENTITY_LEN).min(k.len());
                self.for_each_value_holder(
                    &world.indexes.v_index,
                    &world.indexes.v_latest,
                    &prefix,
                    entity_end,
                    t,
                    |e| out.push(e),
                );
            }
        }
        Ok(out)
    }

    /// Every entity (or edge) with ANY value of the string attribute `attr`
    /// holding at tick `t` in this canon whose [`fold_for_search`] equals
    /// `fold_for_search(text)`: a case- and normalisation-insensitive
    /// [`View::lookup`] ("edwin" finds "Edwin" and "EDWIN"). Ascending by
    /// [`EntityId`], each once.
    ///
    /// Each original value keeps its own history, judged latest-wins as in
    /// [`View::lookup`]: an entity is found if at least one of its values
    /// that fold equal holds at `t`. Answered from `VFold_latest` at the
    /// latest canon and from the main `VFold` with the canon skip at an
    /// older one: one seek per `(original value, entity)` that ever folded
    /// to the text.
    ///
    /// Fails with [`Error::UnknownAttribute`] (an inverse name included),
    /// then [`Error::NotIndexed`] for an attribute declared unindexed (of
    /// any type), then [`Error::WrongValueType`] for a non-string
    /// attribute. The text has no length limit: a fold no stored value has
    /// finds nothing.
    pub fn lookup_ci(&self, attr: &str, text: &str, t: Tick) -> Result<Vec<EntityId>, Error> {
        let world = self.world;
        let attr_id = *world
            .catalog
            .attr_by_name
            .get(attr)
            .ok_or_else(|| Error::UnknownAttribute(attr.to_owned()))?;
        let def = self.require_definition(attr_id)?;
        if !def.indexed {
            return Err(Error::NotIndexed(attr.to_owned()));
        }
        if def.value_type != ValueType::Str {
            return Err(Error::WrongValueType {
                attr: attr.to_owned(),
                expected: def.value_type,
                got: ValueType::Str,
            });
        }
        let fold = Value::Str(fold_for_search(text))
            .encode()
            .expect("invariant: encoding a string never fails");
        // The fold's encoding is self-delimiting, so `F|a|fold|` never
        // matches a longer fold it is a byte prefix of.
        let prefix = key::vfold_prefix(attr_id, &fold);
        let fold_prefix_len = prefix.len();
        let history_end = |k: &[u8]| key::vfold_history_end(fold_prefix_len, k);
        let mut out = Vec::new();
        self.for_each_value_holder(
            &world.indexes.vfold_index,
            &world.indexes.vfold_latest,
            &prefix,
            history_end,
            t,
            |e| out.push(e),
        );
        // Keys run by original value first, so one entity may hold several
        // values that fold equal, and entities aren't in order across them.
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Calls `f`, in key order, with the entity of every history under
    /// `value_prefix` in a value index (`main` and its latest-revision copy
    /// `latest`) that holds at `t` in this canon. `history_end(key)` is the
    /// length of a key's `…|entity|` history prefix, whose last 8 bytes are
    /// the entity. Shared by every value index layout.
    fn for_each_value_holder(
        &self,
        main: &Index,
        latest: &LatestIndex,
        value_prefix: &[u8],
        history_end: impl Fn(&[u8]) -> usize,
        t: Tick,
        mut f: impl FnMut(EntityId),
    ) {
        let f = |p: &[u8]| {
            f(key::v_prefix_entity(p)
                .expect("invariant: a value index history prefix ends in its entity"))
        };
        let work = self.work;
        if self.latest {
            let holds_at = |p: &mut Vec<u8>| holds_latest(latest, p, t, work);
            for_each_holding(latest, value_prefix, history_end, work, holds_at, f);
        } else {
            let holds_at = |p: &mut Vec<u8>| holds(&[main], p, t, self.canon, work);
            for_each_holding(main, value_prefix, history_end, work, holds_at, f);
        }
    }

    /// Calls `f` with every neighbor `x` of `entity` through the `ref`
    /// attribute `attr` whose pair holds at `t` in this canon: targets of
    /// `E|entity|attr|` if `out`, sources of `R|entity|attr|` if `inn`.
    /// Unsorted across the two sides, and a self pair (stored under both)
    /// is reported by each side read.
    fn for_each_partner_holding(
        &self,
        entity: EntityId,
        attr: AttrId,
        out: bool,
        inn: bool,
        t: Tick,
        mut f: impl FnMut(EntityId),
    ) {
        if out {
            let world = self.world;
            self.for_each_e_value_holding(entity, attr, t, |value| {
                // An unresolvable key, or one holding a plain value on this
                // ref attribute (only by tampering), is skipped.
                if let Some(Value::Ref(target)) = world.decode_e_part(attr, value) {
                    f(target);
                }
            });
        }
        if inn {
            let prefix = key::r_attr_prefix(entity, attr);
            // Both R layouts are fixed-width up to and including the source.
            let source_end = |_: &[u8]| key::R_SOURCE_PREFIX_LEN;
            let f =
                |p: &[u8]| f(key::r_source(p).expect("invariant: R index keys are fixed-width"));
            let world = self.world;
            let work = self.work;
            if self.latest {
                let index = &world.indexes.r_latest;
                let holds_at = |p: &mut Vec<u8>| holds_latest(index, p, t, work);
                for_each_holding(index, &prefix, source_end, work, holds_at, f);
            } else {
                let index = &world.indexes.r_index;
                let holds_at = |p: &mut Vec<u8>| holds(&[index], p, t, self.canon, work);
                for_each_holding(index, &prefix, source_end, work, holds_at, f);
            }
        }
    }

    /// Calls `f` with the encoded value of every value of `entity.attr`
    /// that holds at `t` in this canon, in value-encoding order: the
    /// distinct-value walk under `E|entity|attr|`.
    pub(super) fn for_each_e_value_holding(
        &self,
        entity: EntityId,
        attr: AttrId,
        t: Tick,
        mut f: impl FnMut(&[u8]),
    ) {
        self.for_each_e_history(entity, attr, |p| {
            if self.holds_e(p, t) {
                f(&p[key::E_ATTR_PREFIX_LEN..]);
            }
        });
    }

    /// Calls `f` with the history prefix `E|entity|attr|value` of every
    /// value `entity.attr` has ever had in the index this view reads, in
    /// key order (value-encoding order, value references aside).
    fn for_each_e_history(&self, entity: EntityId, attr: AttrId, f: impl FnMut(&mut Vec<u8>)) {
        let prefix = key::e_attr_prefix(entity, attr);
        let world = self.world;
        // Keys in both E indexes are only ever built by `key::e_fact_key`
        // (the latest one without its `^tx`) from a successful
        // `Value::encode`, so they always parse.
        if self.latest {
            let value_end = |k: &[u8]| {
                key::E_ATTR_PREFIX_LEN
                    + key::latest_e_value_and_tick(k)
                        .expect("invariant: E_latest keys are E keys without their ^tx")
                        .0
                        .len()
            };
            for_each_history(&world.indexes.e_latest, &prefix, value_end, self.work, f);
        } else {
            let value_end = |k: &[u8]| {
                key::E_ATTR_PREFIX_LEN
                    + key::e_value_bytes(k)
                        .expect("invariant: E index keys are built only by key::e_fact_key")
                        .len()
            };
            for_each_history(&world.indexes.e_index, &prefix, value_end, self.work, f);
        }
    }

    /// The current entry of every `E` key under `attr_prefix` (`E|e|a|`) in
    /// this canon, `DELETED` ones left out, as `(tick, value encoding, op,
    /// precision)` in key order.
    fn e_current<'v>(&'v self, attr_prefix: &'v [u8]) -> Vec<(Tick, &'v [u8], Op, Precision)> {
        let world = self.world;
        if self.latest {
            under(&world.indexes.e_latest, attr_prefix, self.work)
                .map(|(k, op)| {
                    let (value, tick) = key::latest_e_value_and_tick(k)
                        .expect("invariant: E_latest keys are E keys without their ^tx");
                    (tick, value, *op, self.precision_at(k))
                })
                .collect()
        } else {
            current_entries_metered(&world.indexes.e_index, attr_prefix, self.canon, self.work)
                .map(|(k, op)| {
                    let (value, tick) = key::e_value_and_tick(k)
                        .expect("invariant: E index keys are built only by key::e_fact_key");
                    (tick, value, op, self.precision_at(k))
                })
                .collect()
        }
    }

    /// The precision of the fact stored at `E` key `k` of the index this
    /// view reads (`E_latest` keys at the latest canon, main `E` keys
    /// otherwise): [`Precision::Exact`] unless a vague one is recorded.
    fn precision_at(&self, k: &[u8]) -> Precision {
        let precisions = if self.latest {
            &self.world.indexes.e_latest_precision
        } else {
            &self.world.indexes.e_precision
        };
        precisions.get(k).copied().unwrap_or_default()
    }

    /// The precision of the fact an `R` entry mirrors: the `E` entry of
    /// `(source, attr, Ref(target))` at `tick` (and `tx`, for a main-index
    /// read). Builds that `E` key only if the world holds any vague tick.
    fn r_precision(
        &self,
        source: EntityId,
        attr: AttrId,
        target: EntityId,
        tick: Tick,
        tx: TxId,
    ) -> Precision {
        let world = self.world;
        if world.indexes.e_precision.is_empty() {
            return Precision::Exact;
        }
        let value = Value::Ref(target)
            .encode()
            .expect("invariant: a ref value always encodes");
        let k = key::e_fact_key(source, attr, &value, tick, tx);
        if self.latest {
            self.precision_at(key::without_tx(&k))
        } else {
            self.precision_at(&k)
        }
    }

    /// The current entry of every `R` key under `R|target|attr|` in this
    /// canon, `DELETED` ones left out, as `(source, tick, op, precision)` in
    /// key order.
    fn r_current(&self, target: EntityId, attr: AttrId) -> Vec<(EntityId, Tick, Op, Precision)> {
        let world = self.world;
        let attr_prefix = key::r_attr_prefix(target, attr);
        if self.latest {
            under(&world.indexes.r_latest, &attr_prefix, self.work)
                .map(|(k, op)| {
                    let (source, tick) = key::latest_r_source_and_tick(k)
                        .expect("invariant: R_latest keys are R keys without their ^tx");
                    // The latest path ignores the tx.
                    let precision = self.r_precision(source, attr, target, tick, TxId(0));
                    (source, tick, *op, precision)
                })
                .collect()
        } else {
            current_entries_metered(&world.indexes.r_index, &attr_prefix, self.canon, self.work)
                .map(|(k, op)| {
                    let (_, _, source, tick, tx) =
                        key::decode_r_key(k).expect("invariant: R index keys are fixed-width");
                    let precision = self.r_precision(source, attr, target, tick, tx);
                    (source, tick, op, precision)
                })
                .collect()
        }
    }

    /// The neighbors of `entity` through the `ref` attribute `attr` at tick
    /// `t` in this canon, as `(neighbor, edge)` pairs sorted ascending,
    /// without duplicates.
    ///
    /// [`Direction::Out`]: `entity` is the source; every target `x` with
    /// `(entity, attr, x)` holding at `t`. [`Direction::In`]: `entity` is
    /// the target; every source `x` with `(x, attr, entity)` holding.
    /// [`Direction::Both`]: the union (a pair linked both ways appears once
    /// per edge). A pair holds iff its latest op with tick `<= t` among the
    /// visible transactions (deleted ticks skipped) is an assert, so an event holds from its tick
    /// on. `attr` may be an inverse name (`child_of` for `parent_of`): it
    /// reads the base attribute with `Out` and `In` swapped. For a
    /// bidirectional attribute every direction returns the partners from
    /// either side (each pair is stored once, normalised to `(min, max)`).
    ///
    /// Cost, per direction read, grows with the number of distinct
    /// neighbors the entity has *ever* had through `attr`: one seek each.
    /// Fails with [`Error::UnknownAttribute`] (neither an attribute nor an
    /// inverse name), [`Error::UnknownEntity`] (undefined entity or edge) or
    /// [`Error::NotARef`] (not a `ref` attribute).
    pub fn neighbors(
        &self,
        entity: EntityId,
        attr: &str,
        dir: Direction,
        t: Tick,
    ) -> Result<Vec<(EntityId, EdgeId)>, Error> {
        let world = self.world;
        self.resolve_attribute(attr)?;
        let (attr_id, dir) = world.resolve_neighbor_attr(attr, dir)?;
        self.require_definition(attr_id)?;
        if !self.id_visible(entity) {
            return Err(Error::UnknownEntity(entity));
        }
        if self.require_definition(attr_id)?.value_type != ValueType::Ref {
            return Err(Error::NotARef(attr.to_owned()));
        }
        // A bidirectional pair is stored once, under its normalised
        // (min, max) order, so every direction reads both sides.
        let (read_out, read_in) = if world.is_bidirectional(attr_id) {
            (true, true)
        } else {
            (dir.reads_out(), dir.reads_in())
        };
        // Every stored ref fact's triple has an edge: a stored assert
        // interns it before (or with) the fact, and a stored retract needs a
        // stored assert. An edge defined after this canon can't show up
        // here, since its facts are newer too, so the latest table serves.
        // `edge_of` finds a bidirectional pair in either order; a directed
        // one is asked with `entity` on the side it was read from.
        let mut out = Vec::new();
        if read_out {
            self.for_each_partner_holding(entity, attr_id, true, false, t, |x| {
                out.push((x, edge_or_panic(world, attr_id, entity, x)));
            });
        }
        if read_in {
            self.for_each_partner_holding(entity, attr_id, false, true, t, |x| {
                out.push((x, edge_or_panic(world, attr_id, x, entity)));
            });
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// The change points of `entity.attr` in this canon: for every key
    /// `(value, tick)`, its current entry (the one written by the newest tx
    /// `<=` the canon) if that is an assert or retract, ordered by tick
    /// ascending, then (same tick) by value encoding ascending. A key whose
    /// current entry is a `DELETED` marker is left out; older canon, before
    /// the correction, still lists it.
    ///
    /// Only effective writes are stored, so no-ops and failed writes never
    /// appear. A change point is where something changed, not every tick
    /// where a value holds. For a bidirectional attribute the changes of
    /// every pair `entity` is in are listed, from either side, with the
    /// partner as value; a self pair's changes are listed once. Cost is one
    /// scan of `E|e|a|` (and, bidirectional, of `R|e|a|`): every entry the
    /// attribute has ever had. Fails with [`Error::UnknownAttribute`] or
    /// [`Error::UnknownEntity`].
    pub fn history(&self, entity: EntityId, attr: &str) -> Result<Vec<Change>, Error> {
        let (attr_id, _) = self.resolve(entity, attr)?;
        Ok(self.changes(entity, attr_id))
    }

    /// The change points of `attr` across every entity and edge ID defined
    /// at this canon: for each ID, ascending, exactly what
    /// [`View::history`] returns for it, in the same order (by tick, then
    /// value), as `(id, change)` rows. IDs with no history under `attr`,
    /// and IDs defined after the canon, contribute nothing. Corrections and
    /// precision are honoured as in [`View::history`] (same code, run per
    /// ID).
    ///
    /// An inverse name (`child_of` for `parent_of`) reads the base
    /// attribute through `R`: for each target, the changes of the pairs
    /// pointing at it, with `Ref(source)` as value (as [`View::attr_scan`]
    /// lists `(child, parent)`).
    ///
    /// Cost: one `E` (or `R`) prefix seek per defined ID (plus one `R` seek
    /// for a bidirectional attribute), and a walk over each ID's own keys:
    /// linear in the world's ID count even for a rare attribute, like
    /// [`View::attr_scan`]. Fails with [`Error::UnknownAttribute`] for a
    /// name that is neither an attribute nor an inverse name.
    pub fn history_scan(&self, attr: &str) -> Result<Vec<(EntityId, Change)>, Error> {
        self.resolve_attribute(attr)?;
        let world = self.world;
        let (attr_id, inverse) = match world.catalog.attr_by_name.get(attr) {
            Some(&id) => (id, false),
            None => (
                *world
                    .catalog
                    .inverse_by_name
                    .get(attr)
                    .ok_or_else(|| Error::UnknownAttribute(attr.to_owned()))?,
                true,
            ),
        };
        let def = self.require_definition(attr_id)?;
        if inverse && def.inverse.as_deref() != Some(attr) {
            return Err(Error::UnknownAttribute(attr.into()));
        }
        let mut out = Vec::new();
        for id in world.defined_ids() {
            if !self.work.id() {
                break;
            }
            if !self.id_visible(id) {
                continue;
            }
            let changes = if inverse {
                self.reverse_changes(id, attr_id)
            } else {
                self.changes(id, attr_id)
            };
            out.extend(changes.into_iter().map(|c| (id, c)));
        }
        Ok(out)
    }

    /// The ticks where `entity.attr` holds `value` in this canon, as an
    /// [`IntervalSet`]: [`IntervalSet::from_changes`] of the
    /// [`View::history`] changes whose value is `value`. For a
    /// bidirectional attribute the pair is read from either side.
    ///
    /// Cost: one [`View::history`] read. Fails with
    /// [`Error::UnknownAttribute`] or [`Error::UnknownEntity`], as
    /// [`View::history`] does.
    pub fn holds_during(
        &self,
        entity: EntityId,
        attr: &str,
        value: &Value,
    ) -> Result<IntervalSet, Error> {
        let (attr_id, _) = self.resolve(entity, attr)?;
        let points: Vec<(Tick, Op)> = self
            .changes(entity, attr_id)
            .into_iter()
            .filter(|c| &c.value == value)
            .map(|c| (c.tick, c.op))
            .collect();
        Ok(IntervalSet::from_changes(&points))
    }

    /// [`View::history`] of a resolved attribute.
    pub(super) fn changes(&self, entity: EntityId, attr_id: AttrId) -> Vec<Change> {
        let world = self.world;
        let attr_prefix = key::e_attr_prefix(entity, attr_id);
        // The scan yields value-ascending, then newest tick first; collect
        // the value bytes (borrowed from E keys) and reorder by
        // (tick, value bytes).
        // Value references are resolved to the full encodings they name, so
        // the order below is by value, whatever the references are.
        // A key whose value part can't be resolved or decoded (only by
        // tampering) is skipped; doctor reports it.
        // (value encoding, change): sorted by (tick, encoding) at the end.
        let mut points: Vec<(Cow<'_, [u8]>, Change)> = self
            .e_current(&attr_prefix)
            .into_iter()
            .filter_map(|(tick, part, op, precision)| {
                let (bytes, value) = world.decode_e_value(attr_id, part)?;
                let change = Change {
                    tick,
                    op,
                    value,
                    precision,
                };
                Some((Cow::Borrowed(bytes), change))
            })
            .collect();
        if world.is_bidirectional(attr_id) {
            // The pairs stored under a smaller partner: their R mirrors.
            // A self pair's R mirror is the E entry already collected.
            for (source, tick, op, precision) in self.r_current(entity, attr_id) {
                if source != entity {
                    let value = Value::Ref(source);
                    let bytes = value
                        .encode()
                        .expect("invariant: a ref value always encodes");
                    let change = Change {
                        tick,
                        op,
                        value,
                        precision,
                    };
                    points.push((Cow::Owned(bytes), change));
                }
            }
        }
        if self.work.finish(Ok(())).is_err() {
            return Vec::new();
        }
        // (tick, value) is unique among the points: only each key's
        // current entry is listed, and a bidirectional pair is stored under
        // one side only (the self pair's R side is skipped).
        points.sort_unstable_by(|a, b| (a.1.tick, &a.0).cmp(&(b.1.tick, &b.0)));
        points.into_iter().map(|(_, change)| change).collect()
    }

    /// The change points of the sources pointing at `target` through the
    /// `ref` attribute `attr` (`R|target|attr|`) in this canon, with
    /// `Ref(source)` as value, ordered like [`View::history`]: by tick, then
    /// source ID (= value encoding).
    fn reverse_changes(&self, target: EntityId, attr: AttrId) -> Vec<Change> {
        let mut points = self.r_current(target, attr);
        points.sort_unstable_by_key(|&(source, tick, ..)| (tick, source));
        points
            .into_iter()
            .map(|(source, tick, op, precision)| Change {
                tick,
                op,
                value: Value::Ref(source),
                precision,
            })
            .collect()
    }

    /// Every cardinality overlap of `attr` in this canon, over the whole
    /// story timeline (see [`crate::overlap`]): two values that hold at the
    /// same time on a side labelled cardinality one.
    ///
    /// For a cardinality-one **state** attribute, the [`Side::Forward`]
    /// overlaps of every entity (and edge) among its values as
    /// [`View::history`] lists them (for a bidirectional attribute, the
    /// partners from either side, so both ends of a pair agree); for a
    /// directed `ref` state attribute with a reverse cardinality of one,
    /// the [`Side::Reverse`] overlaps of every target among its sources.
    /// Cardinality many, no reverse label and event attributes have none.
    /// Touching intervals and gaps are never reported.
    ///
    /// Sorted by `(entity, side, a, b, during.from)`, Forward before
    /// Reverse. Cost: one history read per defined entity and side checked.
    /// Fails with [`Error::UnknownAttribute`].
    pub fn overlaps(&self, attr: &str) -> Result<Vec<Overlap>, Error> {
        let world = self.world;
        let attr_id = *world
            .catalog
            .attr_by_name
            .get(attr)
            .ok_or_else(|| Error::UnknownAttribute(attr.to_owned()))?;
        self.require_definition(attr_id)?;
        let mut out = Vec::new();
        if !self.is_checked(attr_id, Side::Forward) && !self.is_checked(attr_id, Side::Reverse) {
            return Ok(out);
        }
        for entity in world.defined_ids() {
            if !self.work.id() {
                break;
            }
            for side in [Side::Forward, Side::Reverse] {
                out.extend(self.overlaps_of(entity, attr_id, side));
            }
        }
        Ok(out)
    }

    /// Whether `side` of `attr` is labelled cardinality one on a state
    /// attribute, i.e. has overlaps to check.
    fn is_checked(&self, attr: AttrId, side: Side) -> bool {
        let Some(def) = self.definition(attr) else {
            return false;
        };
        def.kind == Kind::State
            && match side {
                Side::Forward => def.cardinality == Cardinality::One,
                // Only a directed ref attribute can carry this label
                // (checked at world creation and replay).
                Side::Reverse => def.reverse_cardinality == Some(Cardinality::One),
            }
    }

    /// The overlaps of one `(entity, attr, side)` in this canon, sorted by
    /// `(a, b, during.from)`; empty if that side isn't checked (see
    /// [`View::overlaps`]). The per-entity step of [`View::overlaps`], for
    /// callers that only need the entities a write touched.
    pub(crate) fn overlaps_of(&self, entity: EntityId, attr: AttrId, side: Side) -> Vec<Overlap> {
        self.overlaps_of_touching(entity, attr, side, None)
    }

    /// [`View::overlaps_of`] restricted, when `touched` is given, to the
    /// overlaps in which one value is among `touched` (value encodings,
    /// ascending and deduplicated), in the same order.
    pub(super) fn overlaps_of_touching(
        &self,
        entity: EntityId,
        attr: AttrId,
        side: Side,
        touched: Option<&[Vec<u8>]>,
    ) -> Vec<Overlap> {
        if !self.is_checked(attr, side) {
            return Vec::new();
        }
        let changes = match side {
            Side::Forward => self.changes(entity, attr),
            Side::Reverse => self.reverse_changes(entity, attr),
        };
        if changes.is_empty() {
            return Vec::new();
        }
        let name = &self.world.catalog.attributes[attr.0 as usize].name;
        let per_value = value_intervals(&changes);
        let rows = match touched {
            None => find_overlaps(&per_value),
            Some(values) => find_overlaps_touching(&per_value, values),
        };
        rows.into_iter()
            .map(|(a, b, during)| Overlap {
                entity,
                attr: name.clone(),
                side,
                a,
                b,
                during,
            })
            .collect()
    }

    /// The entries visible in this canon that store something at a key
    /// (facts, `Revised` facts and `DELETED` markers), in log order, with
    /// their tx and key.
    fn visible_keyed_entries(&self) -> impl Iterator<Item = (&EntryInfo, FactDef)> + '_ {
        let canon = self.canon;
        let world = self.world;
        world
            .journal
            .entries()
            .iter()
            .take_while(move |e| e.tx <= canon)
            .filter_map(|e| {
                let (entity, attr, value, tick, op) = match &e.body {
                    EntryBody::Deleted {
                        entity,
                        attr,
                        value,
                        tick,
                        ..
                    } => (*entity, *attr, value, *tick, Op::Assert),
                    body => {
                        let (entity, attr, value, tick, op, _) = as_fact(body)?;
                        (entity, attr, value, tick, op)
                    }
                };
                // Kept entries were applied, so their values encode.
                Some((
                    e,
                    world.stored_fact_def(entity, attr, value, tick, op).ok()?,
                ))
            })
    }

    /// Every cardinality overlap in this canon ([`View::overlaps`] of every
    /// attribute, in schema order), each with the newest tx `<=` canon of an
    /// entry touching its `(entity, attr, side)` as [`World::touched_sides`]
    /// judges a write: doctor's `since` tag.
    pub(crate) fn tagged_overlaps(&self) -> Vec<(TxId, Overlap)> {
        let world = self.world;
        let mut touched: HashMap<(EntityId, AttrId, Side), TxId> = HashMap::new();
        for (e, fact) in self.visible_keyed_entries() {
            for side in world.touched_sides(std::iter::once(&fact)) {
                let tx = touched.entry(side).or_insert(e.tx);
                *tx = (*tx).max(e.tx);
            }
        }
        let mut out = Vec::new();
        for (i, _) in world.catalog.attributes.iter().enumerate() {
            let attr = AttrId(i as u32);
            let Some(def) = self.definition(attr) else {
                continue;
            };
            let overlaps = self
                .overlaps(&def.name)
                .expect("invariant: every schema attribute is known by its name");
            out.extend(overlaps.into_iter().map(|o| {
                // Every overlap's side has entries, so it was touched.
                let tx = touched
                    .get(&(o.entity, attr, o.side))
                    .copied()
                    .unwrap_or(self.canon);
                let history = &world.catalog.attribute_history[attr.0 as usize];
                let schema_tx = history
                    .iter()
                    .rev()
                    .find(|(tx, _)| *tx <= self.canon)
                    .map_or(TxId(0), |(tx, _)| *tx);
                (tx.max(schema_tx), o)
            }));
        }
        out
    }

    /// Every property assert on an edge whose current entry in this canon
    /// is an assert at a tick where the edge's own triple doesn't hold in
    /// this canon, in entry order, as `(tag, entry, edge, attr name, tick)`.
    /// `entry` is the key's current entry (the base fact or its last
    /// `Revised`); `tag` is the newer of its tx and the newest tx `<=` canon
    /// of an entry on the edge's triple: doctor's `since` tag. Property
    /// retracts are never reported: cleaning up at the edge's end is normal.
    pub(crate) fn unasserted_edge_properties(&self) -> Vec<(TxId, EntryId, EdgeId, String, Tick)> {
        // Per key: its current entry (the last one in log order); per
        // history `E|e|a|v`: the newest tx writing to it.
        type Current = (EntryId, TxId, Mark, EdgeId, FactDef);
        let mut current: HashMap<(Vec<u8>, Tick), Current> = HashMap::new();
        let mut history_tx: HashMap<Vec<u8>, TxId> = HashMap::new();
        for (e, fact) in self.visible_keyed_entries() {
            let prefix = key::e_value_prefix(fact.entity, fact.attr, &fact.value);
            // An edge's own triple may have an edge as its source.
            let tx = history_tx.entry(prefix.clone()).or_insert(e.tx);
            *tx = (*tx).max(e.tx);
            let Some(edge) = fact.entity.as_edge() else {
                continue;
            };
            let mark = match e.body {
                EntryBody::Deleted { .. } => Mark::Deleted,
                _ => Mark::from(fact.op),
            };
            current.insert((prefix, fact.tick), (e.id, e.tx, mark, edge, fact));
        }
        let mut out: Vec<_> = current
            .into_values()
            .filter(|(_, _, mark, _, _)| *mark == Mark::Assert)
            .filter_map(|(entry, tx, _, edge, fact)| {
                let i = usize::try_from(edge.seq().checked_sub(1)?).ok()?;
                let &(source, attr, target) = self
                    .world
                    .catalog
                    .edges
                    .get(i)
                    .expect("invariant: a kept entry's edge subject is defined");
                let value = Value::Ref(target)
                    .encode()
                    .expect("invariant: a ref value always encodes");
                let mut triple = key::e_value_prefix(source, attr, &value);
                if self.holds_e(&mut triple, fact.tick) {
                    return None;
                }
                let tag = history_tx.get(&triple).map_or(tx, |t| tx.max(*t));
                let name = self.world.catalog.attributes[fact.attr.0 as usize]
                    .name
                    .clone();
                Some((tag, entry, edge, name, fact.tick))
            })
            .collect();
        out.sort_unstable_by_key(|f| f.1);
        out
    }
}
