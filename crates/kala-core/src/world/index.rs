//! The indexes: their key-value types, how a committed entry is written
//! into them ([`World::index_entry`]) and the seeks every read is built from.

use super::*;

/// An `E`-layout index with its value references resolved, and the stored
/// keys that couldn't be (see [`World::resolved_e_index`]).
pub(crate) type ResolvedIndex<'i, V> = (Cow<'i, OrderedMap<V>>, Vec<Vec<u8>>);

/// An `E`- or `R`-layout index: full key -> what the entry says. Used for
/// the committed indexes and for a transaction's staged entries, so one seek
/// routine serves both.
pub(crate) type Index = OrderedMap<Mark>;

/// A latest-revision index (`E_latest` / `R_latest`): an `E`- or `R`-layout
/// key without its `^tx` (`E|e|a|v|^tick`, `R|t|a|s|^tick`) -> the op of
/// that key's current entry at the latest canon. A key whose current entry
/// is a `DELETED` marker is absent, so the first key at or after `^T` in a
/// history is the answer at `T`: one seek, nothing to skip.
pub(crate) type LatestIndex = OrderedMap<Op>;

/// The tick precisions of an `E`-layout index (main keys with `^tx`, or
/// latest-revision keys without), by the same key as the entry they belong
/// to. Sparse: only facts whose precision isn't [`Precision::Exact`] have a
/// row, so a world that never writes a vague tick holds none. Precision is
/// a payload, never part of a key: the indexes themselves are the same
/// whatever the precisions are.
pub(crate) type Precisions = OrderedMap<Precision>;

/// A decoded copy of the stored indexes, from [`World::index_dump`].
/// Rows are in key order (within one key, newest tx first), `DELETED`
/// markers included.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct IndexDump {
    /// `E` rows: `(entity, attr, value, tick, tx, mark)`.
    pub e: Vec<(EntityId, AttrId, Value, Tick, TxId, Mark)>,
    /// `R` rows: `(target, attr, source, tick, tx, mark)`.
    pub r: Vec<(EntityId, AttrId, EntityId, Tick, TxId, Mark)>,
    /// `V` rows: `(attr, value, entity, tick, tx, mark)`.
    pub v: Vec<(AttrId, Value, EntityId, Tick, TxId, Mark)>,
    /// `VFold` rows: `(attr, folded, original, entity, tick, tx, mark)`.
    pub v_fold: Vec<(AttrId, String, Value, EntityId, Tick, TxId, Mark)>,
}

/// Borrowed views of a world's stored indexes (see [`World::indexes`]).
pub(crate) struct StoredIndexes<'w> {
    pub(crate) e: &'w Index,
    pub(crate) r: &'w Index,
    pub(crate) v: &'w Index,
    pub(crate) e_latest: &'w LatestIndex,
    pub(crate) r_latest: &'w LatestIndex,
    pub(crate) e_precision: &'w Precisions,
    pub(crate) e_latest_precision: &'w Precisions,
    pub(crate) v_latest: &'w LatestIndex,
    pub(crate) vfold: &'w Index,
    pub(crate) vfold_latest: &'w LatestIndex,
}

/// Borrows an owned key bound as a slice bound, for [`OrderedKv::scan`].
fn as_slice(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match b {
        Bound::Included(k) => Bound::Included(k),
        Bound::Excluded(k) => Bound::Excluded(k),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// The distinct-history walk behind `AsOf(entity, attr, T)` and
/// `Neighbors`, for any index whose keys are `attr_prefix | x | ^tick`,
/// optionally followed by `| ^tx` (`E|e|a|value|...`, `R|t|a|source|...`,
/// main or latest-revision). Calls `f` with the history prefix
/// `attr_prefix | x` of every `x` for which `holds_at(prefix)` (the seek at
/// the read's tick and canon) is true, in key order.
///
/// `history_end(key)` is the length of a key's history prefix. Each round
/// finds the next distinct history, seeks it, then jumps past all of its
/// keys, so the cost is one seek per distinct `x` ever stored.
pub(super) fn for_each_holding<M: OrderedKv>(
    index: &M,
    attr_prefix: &[u8],
    history_end: impl Fn(&[u8]) -> usize,
    work: Work<'_>,
    mut holds_at: impl FnMut(&mut Vec<u8>) -> bool,
    mut f: impl FnMut(&[u8]),
) {
    for_each_history(index, attr_prefix, history_end, work, |prefix| {
        if holds_at(prefix) {
            f(prefix);
        }
    });
}

/// The walk of [`for_each_holding`] without the seek: calls `f` with the
/// history prefix of every distinct history under `attr_prefix`, in key
/// order (`f` may use it as a seek buffer, restoring it before returning).
/// Charges `work` one key per round (the key that ends the walk included).
pub(super) fn for_each_history<M: OrderedKv>(
    index: &M,
    attr_prefix: &[u8],
    history_end: impl Fn(&[u8]) -> usize,
    work: Work<'_>,
    mut f: impl FnMut(&mut Vec<u8>),
) {
    let mut from = Bound::Included(attr_prefix.to_vec());
    let mut prefix = Vec::new();
    while let Some((k, _)) = index.scan(as_slice(&from)).next() {
        if !work.add(1) {
            break;
        }
        if !k.starts_with(attr_prefix) {
            break;
        }
        prefix.clear();
        prefix.extend_from_slice(&k[..history_end(k)]);
        from = Bound::Excluded(key::past_value(&prefix));
        f(&mut prefix);
    }
}

/// The tick of index key `k` whose history prefix is `prefix_len` bytes
/// long: the `^tick` right after it (main and latest-revision keys alike).
pub(super) fn tick_after(k: &[u8], prefix_len: usize) -> Tick {
    let bytes = k
        .get(prefix_len..prefix_len + 8)
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .expect("invariant: a history's keys carry a ^tick after its prefix");
    Tick::decode_inverted(bytes)
}

/// Every `(key, op)` of a latest-revision index under `prefix`, in key
/// order. Charges `work` one per key examined.
pub(super) fn under<'i, M: OrderedKv<Value = Op>>(
    index: &'i M,
    prefix: &'i [u8],
    work: Work<'i>,
) -> impl Iterator<Item = (&'i [u8], &'i Op)> + 'i {
    index
        .scan_from(prefix)
        .take_while(move |_| work.add(1))
        .take_while(move |(k, _)| k.starts_with(prefix))
}

/// Makes `mark` the current entry of latest-revision key `at_key`: a fact
/// is stored (replacing the previous current entry), a `DELETED` marker
/// empties the key.
fn set_latest(index: &mut impl OrderedKv<Value = Op>, at_key: &[u8], mark: Mark) {
    match mark.op() {
        Some(op) => index.set(at_key, op),
        None => {
            index.remove(at_key);
        }
    }
}

/// One seek in a latest-revision index: whether the first key at or after
/// `value_prefix | ^t` within `value_prefix` (`E|e|a|v|` or `R|t|a|s|`),
/// i.e. the current entry of the latest tick `<= t`, is an assert. Deleted
/// keys aren't stored there and no entry is newer than the latest canon,
/// so there is nothing to skip. Reuses `value_prefix` as the seek key
/// buffer; it is restored before returning.
pub(super) fn holds_latest(
    index: &impl OrderedKv<Value = Op>,
    value_prefix: &mut Vec<u8>,
    t: Tick,
    work: Work<'_>,
) -> bool {
    latest_seek(index, value_prefix, t, work).map(|(_, op)| op) == Some(Op::Assert)
}

/// The seek of [`holds_latest`]: the key and op of the current entry of the
/// latest tick `<= t` under `value_prefix`, if any.
pub(super) fn latest_seek<'i, M: OrderedKv<Value = Op>>(
    index: &'i M,
    value_prefix: &mut Vec<u8>,
    t: Tick,
    work: Work<'_>,
) -> Option<(&'i [u8], Op)> {
    if !work.add(1) {
        return None;
    }
    let len = value_prefix.len();
    key::push_seek_tick(value_prefix, t);
    let found = index
        .scan_from(value_prefix)
        .next()
        .filter(|(k, _)| k.starts_with(&value_prefix[..len]))
        .map(|(k, op)| (k, *op));
    value_prefix.truncate(len);
    found
}

/// [`current_entries_metered`], unmetered (white-box tests).
#[cfg(test)]
pub(super) fn current_entries<'i>(
    index: &'i Index,
    prefix: &'i [u8],
    canon: TxId,
) -> impl Iterator<Item = (&'i [u8], Op)> + 'i {
    current_entries_metered(index, prefix, canon, Work::NONE)
}

/// The current entry (newest tx `<= canon`) of every key under `prefix` in
/// `index`, as `(full key, op)` in key order; keys whose current entry is a
/// `DELETED` marker are left out. One pass over every entry under `prefix`:
/// the entries of one key are adjacent, newest tx first, so the first
/// visible one of each run is that key's current entry. Charges `work` one
/// per entry examined (the one that ends the walk included).
pub(super) fn current_entries_metered<'i, M: OrderedKv<Value = Mark>>(
    index: &'i M,
    prefix: &'i [u8],
    canon: TxId,
    work: Work<'i>,
) -> impl Iterator<Item = (&'i [u8], Op)> + 'i {
    let mut last_key: Option<&'i [u8]> = None;
    index
        .scan_from(prefix)
        .take_while(move |_| work.add(1))
        .take_while(move |(k, _)| k.starts_with(prefix))
        .filter(move |(k, _)| key::e_key_tx(k).is_some_and(|tx| tx <= canon))
        .filter(move |(k, _)| {
            let at_key = key::without_tx(k);
            let first = last_key != Some(at_key);
            last_key = Some(at_key);
            first
        })
        .filter_map(|(k, mark)| Some((k, mark.op()?)))
}

/// Decodes a stored `E` key, value included: `(entity, attr, value, tick,
/// tx)`, or `None` if the bytes aren't a well-formed `E` key holding a
/// canonical value encoding. Never panics, whatever the bytes: doctor and
/// [`World::index_dump`] read tampered indexes through it.
pub(crate) fn decode_e_row(k: &[u8]) -> Option<(EntityId, AttrId, Value, Tick, TxId)> {
    let (entity, attr, value, tick, tx) = key::decode_e_key(k).ok()?;
    Some((entity, attr, Value::decode(value).ok()?, tick, tx))
}

/// Decodes a stored `V` key, value included: `(attr, value, entity, tick,
/// tx)`, or `None` if the bytes aren't a well-formed `V` key holding a
/// canonical value encoding. Never panics, whatever the bytes.
pub(crate) fn decode_v_row(k: &[u8]) -> Option<(AttrId, Value, EntityId, Tick, TxId)> {
    let (attr, value, entity, tick, tx) = key::decode_v_key(k).ok()?;
    Some((attr, Value::decode(value).ok()?, entity, tick, tx))
}

/// Decodes a stored `VFold` key, values included: `(attr, folded,
/// original, entity, tick, tx)`, or `None` if the bytes aren't a
/// well-formed `VFold` key holding a canonical string fold and a canonical
/// original value. Never panics, whatever the bytes.
pub(crate) fn decode_vfold_row(k: &[u8]) -> Option<(AttrId, String, Value, EntityId, Tick, TxId)> {
    let (attr, fold, original, entity, tick, tx) = key::decode_vfold_key(k).ok()?;
    let Value::Str(folded) = Value::decode(fold).ok()? else {
        return None;
    };
    Some((
        attr,
        folded,
        Value::decode(original).ok()?,
        entity,
        tick,
        tx,
    ))
}

/// Whether the `E`-layout `index` holds any entry (any tx) at `fact`'s key
/// `E|e|a|v|^tick`.
pub(super) fn has_key(index: &impl OrderedKv<Value = Mark>, fact: &FactDef) -> bool {
    let mut at_key = key::e_value_prefix(fact.entity, fact.attr, fact.key_value());
    key::push_seek_tick(&mut at_key, fact.tick);
    index
        .scan_from(&at_key)
        .next()
        .is_some_and(|(k, _)| k.starts_with(&at_key))
}

/// One seek: the op of the current entry (newest tx `<= canon`) under
/// `value_prefix` (`E|e|a|v|`) at exactly tick `t`, if any; `None` if there
/// is none or it is a `DELETED` marker. Reuses `value_prefix` as the seek
/// key buffer; it is restored before returning.
pub(super) fn op_at_tick<M: OrderedKv<Value = Mark>>(
    layers: &[&M],
    value_prefix: &mut Vec<u8>,
    t: Tick,
    canon: TxId,
) -> Option<Op> {
    let len = value_prefix.len();
    key::push_seek_tick(value_prefix, t);
    // The prefix to match includes `^t`: only entries at exactly `t`.
    // Only the write path asks this: unmetered.
    let found = seek(layers, value_prefix, value_prefix, canon, Work::NONE);
    value_prefix.truncate(len);
    found
}

/// One seek: whether the current entry (newest tx `<= canon`) of the latest
/// tick `<= t` under `value_prefix` (`E|e|a|v|`) is an assert, ticks whose
/// current entry is a `DELETED` marker skipped. Reuses `value_prefix` as
/// the seek key buffer; it is restored before returning.
pub(super) fn holds<M: OrderedKv<Value = Mark>>(
    layers: &[&M],
    value_prefix: &mut Vec<u8>,
    t: Tick,
    canon: TxId,
    work: Work<'_>,
) -> bool {
    let len = value_prefix.len();
    key::push_seek_tick(value_prefix, t);
    let found = seek(layers, value_prefix, &value_prefix[..len], canon, work);
    value_prefix.truncate(len);
    found == Some(Op::Assert)
}

/// The op of the current entry of the first tick at or after `from` under
/// `prefix`, reading all `layers` as if they were one index and only
/// entries written by a tx `<= canon`; ticks whose current entry is a
/// `DELETED` marker are skipped whole.
///
/// Within one `E|e|a|v|` (or `R|t|a|s|`) history the smallest key is the
/// latest entry (newest tick first, then newest tx), i.e. the current entry
/// of the newest tick. That holds across layers because a transaction's
/// staged keys carry its own tx, newer than any committed one. Keys order
/// by `^tick` before `^tx`, so a newer-tx entry can sit before the answer at
/// any tick (a later prequel or retract); those are skipped one by one. A
/// current `DELETED` marker empties its whole tick: older-tx entries at the
/// same tick are not current, so the walk jumps past the tick instead of
/// falling through to them. At the latest canon with no corrections nothing
/// is skipped: still one seek.
fn seek<M: OrderedKv<Value = Mark>>(
    layers: &[&M],
    from: &[u8],
    prefix: &[u8],
    canon: TxId,
    work: Work<'_>,
) -> Option<Op> {
    seek_entry(layers, from, prefix, canon, work).map(|(_, op)| op)
}

/// [`seek`], also returning the key of the entry found. Charges `work`
/// one per key examined: the one landed on, every newer-tx or deleted
/// entry stepped over, and the one past the prefix that ends a miss.
pub(super) fn seek_entry<'i, M: OrderedKv<Value = Mark>>(
    layers: &[&'i M],
    from: &[u8],
    prefix: &[u8],
    canon: TxId,
    work: Work<'_>,
) -> Option<(&'i [u8], Op)> {
    // Set once a deleted tick was skipped: seek exclusively past it.
    let mut past: Option<Vec<u8>> = None;
    loop {
        let lower = match &past {
            None => Bound::Included(from),
            Some(p) => Bound::Excluded(&p[..]),
        };
        let found = layers
            .iter()
            .filter_map(|index| {
                index
                    .scan(lower)
                    .take_while(|_| work.add(1))
                    .take_while(|(k, _)| k.starts_with(prefix))
                    .find(|(k, _)| key::e_key_tx(k).is_some_and(|tx| tx <= canon))
            })
            .min_by(|(a, _), (b, _)| a.cmp(b));
        let (k, mark) = found?;
        match mark.op() {
            Some(op) => return Some((k, op)),
            None => past = Some(key::past_tick(k)),
        }
    }
}

/// Every index over a world's stored facts, with the tables derived
/// alongside them (the value table, tick counts and planner statistics).
/// All of it is a copy of the log: written only by [`World::index_entry`],
/// on commit and replay, and rebuilt from the log on open.
#[derive(Debug)]
pub(super) struct Indexes {
    /// Forward index `E|entity|attr|value|^tick|^tx -> mark` (layout in
    /// [`crate::key`]): every stored fact and `DELETED` marker. With
    /// `r_index`, the in-memory copy of the log's entries (milestone 0);
    /// both are rebuilt from the log on open.
    pub(super) e_index: Index,
    /// Reverse index `R|target|attr|source|^tick|^tx -> mark`: one entry
    /// per stored entry at a `ref` key, written by [`World::index_entry`]
    /// together with its `E` entry. Read by [`View::neighbors`] for
    /// [`Direction::In`].
    pub(super) r_index: Index,
    /// `E_latest`: every `E` key's current entry at the latest canon, keys
    /// whose current entry is a `DELETED` marker left out (see
    /// [`LatestIndex`]). Derived from `e_index`; written only by
    /// [`World::index_entry`]. Serves every read at the latest canon.
    /// Crate-visible so doctor can compare it against a rebuild.
    pub(super) e_latest: LatestIndex,
    /// `R_latest`: the same for `r_index`.
    pub(super) r_latest: LatestIndex,
    /// The precision of every `E` entry that is a fact with a vague tick,
    /// by its `E` key (see [`Precisions`]): the payload half of an `E` row.
    /// Written only by [`World::index_entry`], derived and rebuilt with `E`;
    /// doctor compares it as part of `E`.
    pub(super) e_precision: Precisions,
    /// The same for `E_latest`: the precision of each key's current fact at
    /// the latest canon, if vague. Compared by doctor as part of `E_latest`.
    pub(super) e_latest_precision: Precisions,
    /// Value index `V|attr|value|entity|^tick|^tx -> mark`: a copy of every
    /// `E` entry of an indexed, non-ref attribute (see
    /// [`World::value_indexed`]), keyed by value so "every entity with
    /// `attr = v`" is a prefix walk. Written only by [`World::index_entry`]
    /// next to its `E` entry, so it is derived and rebuilt with `E`.
    pub(super) v_index: Index,
    /// `V_latest`: the same for `v_index` as `E_latest` is for `e_index`.
    /// Serves [`View::lookup`] at the latest canon.
    pub(super) v_latest: LatestIndex,
    /// Folded value index `F|attr|fold|original|entity|^tick|^tx -> mark`
    /// (layout in [`crate::key`]): a copy of every `V` entry of a string
    /// attribute, keyed by the value's [`fold_for_search`] form, so "every
    /// entity named edwin, in any case" is a prefix walk. Written only by
    /// [`World::index_entry`] next to its `V` entry; derived and rebuilt
    /// with `E`.
    pub(super) vfold_index: Index,
    /// `VFold_latest`: the same for `vfold_index` as `E_latest` is for
    /// `e_index`. Serves [`View::lookup_ci`] at the latest canon.
    pub(super) vfold_latest: LatestIndex,
    /// Every unindexed string value an `E` key refers to, by reference and
    /// by content (see [`crate::value_table`]). Derived like the indexes:
    /// filled only by [`World::index_entry`] on commit and replay.
    pub(super) values: ValueTable,
    /// The content hash of the value references ([`fnv1a64`]; a test hook
    /// may replace it, see `World::set_content_hash`).
    pub(super) content_hash: ContentHash,
    /// How many keys hold a fact (assert or retract) at the latest canon,
    /// per tick: a key whose current entry is a `DELETED` marker isn't
    /// counted. The last tick is the world's latest tick. Derived from the
    /// indexes; maintained by [`World::index_entry`] on commit and replay.
    pub(super) tick_counts: BTreeMap<Tick, u64>,
    /// The planner's per-attribute estimates behind [`World::attr_stats`].
    /// Derived from the log like the indexes (never written to it):
    /// maintained by [`World::index_entry`] on commit and replay.
    pub(super) attr_counts: HashMap<AttrId, AttrCounts>,
}

impl Indexes {
    /// Empty indexes whose value references use `content_hash`.
    pub(super) fn new(content_hash: ContentHash) -> Self {
        Indexes {
            e_index: OrderedMap::new(),
            r_index: OrderedMap::new(),
            e_latest: OrderedMap::new(),
            r_latest: OrderedMap::new(),
            e_precision: OrderedMap::new(),
            e_latest_precision: OrderedMap::new(),
            v_index: OrderedMap::new(),
            v_latest: OrderedMap::new(),
            vfold_index: OrderedMap::new(),
            vfold_latest: OrderedMap::new(),
            values: ValueTable::default(),
            content_hash,
            tick_counts: BTreeMap::new(),
            attr_counts: HashMap::new(),
        }
    }

    /// The stored index maps, borrowed for doctor's comparison.
    fn stored(&self) -> StoredIndexes<'_> {
        StoredIndexes {
            e: &self.e_index,
            r: &self.r_index,
            v: &self.v_index,
            e_latest: &self.e_latest,
            r_latest: &self.r_latest,
            e_precision: &self.e_precision,
            e_latest_precision: &self.e_latest_precision,
            v_latest: &self.v_latest,
            vfold: &self.vfold_index,
            vfold_latest: &self.vfold_latest,
        }
    }

    /// Stores one entry at the key of `fact` in every index it belongs to
    /// (see [`World::index_entry`], the only caller). `value_indexed` says
    /// whether the fact's attribute is mirrored in `V`, which the schema
    /// decides, not the indexes.
    fn insert(&mut self, fact: &FactDef, mark: Mark, tx: TxId, value_indexed: bool) {
        // A new unindexed value enters the table with the reference its
        // keys were built with (see `World::value_ref`).
        if let Some(r) = fact.value_ref() {
            self.values.insert(&fact.value, r);
        }
        let k = key::e_fact_key(fact.entity, fact.attr, fact.key_value(), fact.tick, tx);
        // `tx` is newer than every stored entry, so this entry becomes the
        // key's current one at the latest canon: `E_latest` holds the
        // previous current entry, if it was a fact.
        let at_key = key::without_tx(&k);
        let was_fact = self.e_latest.contains_key(at_key);
        set_latest(&mut self.e_latest, at_key, mark);
        // The precision rides along with the entry (sparse: vague ticks
        // only). The key's current entry replaces the previous one's at
        // the latest canon, so a DELETED marker or an exact fact clears it.
        let vague = mark.op().is_some() && fact.precision != Precision::Exact;
        if vague {
            self.e_precision.insert(k.clone(), fact.precision);
            self.e_latest_precision.set(at_key, fact.precision);
        } else {
            self.e_latest_precision.remove(at_key);
        }
        match (was_fact, mark.op().is_some()) {
            (false, true) => *self.tick_counts.entry(fact.tick).or_insert(0) += 1,
            (true, false) => {
                if let Some(n) = self.tick_counts.get_mut(&fact.tick) {
                    *n -= 1;
                    if *n == 0 {
                        self.tick_counts.remove(&fact.tick);
                    }
                }
            }
            _ => {}
        }
        if mark.op().is_some() {
            // A fact or revised entry (DELETED markers add nothing), in the
            // stored orientation `fact` already has.
            self.attr_counts
                .entry(fact.attr)
                .or_default()
                .add(fact.entity, fact.key_value());
        }
        self.e_index.insert(k, mark);
        if let Some(target) = fact.target {
            let k = key::r_fact_key(target, fact.attr, fact.entity, fact.tick, tx);
            set_latest(&mut self.r_latest, key::without_tx(&k), mark);
            self.r_index.insert(k, mark);
        }
        if value_indexed {
            let k = key::v_fact_key(fact.attr, &fact.value, fact.entity, fact.tick, tx);
            set_latest(&mut self.v_latest, key::without_tx(&k), mark);
            self.v_index.insert(k, mark);
            // A string attribute's entry is mirrored once more under its
            // fold. `fact.value` comes from `Value::encode`, so it decodes;
            // anything but a string (never stored for a string attribute)
            // has no fold.
            if let Ok(Value::Str(text)) = Value::decode(&fact.value) {
                let fold = Value::Str(fold_for_search(&text))
                    .encode()
                    .expect("invariant: encoding a string never fails");
                let k =
                    key::vfold_fact_key(fact.attr, &fold, &fact.value, fact.entity, fact.tick, tx);
                set_latest(&mut self.vfold_latest, key::without_tx(&k), mark);
                self.vfold_index.insert(k, mark);
            }
        }
    }
}

impl World {
    /// The content hash this world's value references use: doctor rebuilds
    /// a world with the same one, so the two assign the same references.
    pub(crate) fn content_hash(&self) -> ContentHash {
        self.indexes.content_hash
    }

    /// Sets the content hash of a world being rebuilt, before any fact is
    /// applied (a world holding just its genesis).
    pub(crate) fn set_rebuild_hash(&mut self, hash: ContentHash) {
        debug_assert!(
            self.indexes.values.is_empty(),
            "invariant: the content hash is set before any value is referenced"
        );
        self.indexes.content_hash = hash;
    }

    /// Replaces the content hash of this world's value references with `f`
    /// and rebuilds its indexes and value table from its kept entries, as a
    /// replay with that hash would: a test hook, so a forced-collision world
    /// (`|_| 0`) can be checked to answer exactly like a normal one.
    /// [`crate::doctor`] rebuilds this world with `f` too. Only this World
    /// object changes: nothing is logged, and a reopen uses the real hash.
    /// Not part of the supported API.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn set_content_hash(&mut self, f: fn(&[u8]) -> u64) {
        let rebuilt = World::replay_with_hash(&self.journal.log_entries(), f)
            .expect("invariant: a world's kept entries replay cleanly");
        // The rebuild holds the same schema, IDs and entries; what is kept
        // from this World is what isn't derived from the log: the log file
        // and the write gate.
        let log = self.journal.detach_log();
        let gate = std::mem::take(&mut self.gate);
        *self = rebuilt;
        if let Some(log) = log {
            self.journal.attach_log(log);
        }
        self.gate = gate;
    }

    /// The stored indexes, for doctor to compare against a rebuild.
    pub(crate) fn indexes(&self) -> StoredIndexes<'_> {
        self.indexes.stored()
    }

    /// Stores one entry of transaction `tx` at the key of `fact` in the
    /// indexes: `mark` (the fact's op, or [`Mark::Deleted`] for a `DELETED`
    /// marker at that key) under its `E` key and, for a `ref` key, its `R`
    /// mirror, for an indexed non-ref attribute its `V` mirror (and for a
    /// string attribute its `VFold` mirror). Commit and
    /// replay both write the indexes only through here, so the two always
    /// hold the same entries, and the latest-revision indexes and the
    /// latest tick follow the keys' current entries.
    pub(super) fn index_entry(&mut self, fact: &FactDef, mark: Mark, tx: TxId) {
        let value_indexed = self.value_indexed(fact.attr);
        self.indexes.insert(fact, mark, tx, value_indexed);
    }

    /// Whether `attr`'s entries are mirrored in `V`: it is declared
    /// indexed and isn't a `ref` attribute (`R` is the value index of a
    /// `ref` attribute).
    fn value_indexed(&self, attr: AttrId) -> bool {
        let def = &self.catalog.attributes[attr.0 as usize];
        def.indexed && def.value_type != ValueType::Ref
    }

    /// Whether `attr`'s `E` keys carry value references: it is an
    /// unindexed string attribute.
    pub(super) fn keys_by_ref(&self, attr: AttrId) -> bool {
        self.catalog
            .attributes
            .get(attr.0 as usize)
            .is_some_and(|def| !def.indexed && def.value_type == ValueType::Str)
    }

    /// The full value encoding an `E`-key value part of attribute `attr`
    /// stands for: the part itself, or for an unindexed string the value
    /// its reference names in the table. `None` if the part doesn't fit the
    /// attribute's layout (a reference on an attribute not keyed by
    /// reference, a plain encoding on one that is) or names a reference the
    /// table lacks: only reachable by tampering, so readers skip such keys
    /// (doctor reports them).
    fn e_part_encoding<'a>(&'a self, attr: AttrId, part: &'a [u8]) -> Option<&'a [u8]> {
        match (self.keys_by_ref(attr), ValueRef::from_key_bytes(part)) {
            (true, Some(r)) => self.indexes.values.resolve(r),
            (false, None) => Some(part),
            _ => None,
        }
    }

    /// The value an `E`-key value part of attribute `attr` stands for, or
    /// `None` if it can't be resolved or decoded (see
    /// [`World::e_part_encoding`]); readers skip such keys.
    pub(super) fn decode_e_part(&self, attr: AttrId, part: &[u8]) -> Option<Value> {
        self.decode_e_value(attr, part).map(|(_, v)| v)
    }

    /// The full value encoding and the value an `E`-key value part of
    /// attribute `attr` stands for, or `None` if it can't be resolved or
    /// decoded, or decodes to a value of another type than the attribute's
    /// (a plain value on a `ref` attribute, say). Every write and replay
    /// checks the type, so such a key is only reachable by tampering:
    /// readers skip it (doctor reports it).
    pub(super) fn decode_e_value<'a>(
        &'a self,
        attr: AttrId,
        part: &'a [u8],
    ) -> Option<(&'a [u8], Value)> {
        let bytes = self.e_part_encoding(attr, part)?;
        let value = Value::decode(bytes).ok()?;
        let def = self.catalog.attributes.get(attr.0 as usize)?;
        (value.value_type() == def.value_type).then_some((bytes, value))
    }

    /// The `E`-key value part of the committed value `v` (encoded as
    /// `encoding`) of `attr`: the encoding, or for an unindexed string its
    /// reference; `None` if no committed entry ever stored that unindexed
    /// value (so it has no history).
    pub(super) fn committed_key_part<'a>(
        &self,
        attr: AttrId,
        encoding: &'a [u8],
    ) -> Option<Cow<'a, [u8]>> {
        if !self.keys_by_ref(attr) {
            return Some(Cow::Borrowed(encoding));
        }
        let r = self.indexes.values.get(encoding)?;
        Some(Cow::Owned(r.to_key_bytes().to_vec()))
    }

    /// Decodes a stored `E` key with its value resolved through this
    /// world's value table: `(entity, attr, value, tick, tx)`, or `None` if
    /// the bytes aren't a well-formed `E` key or refer to no value. Never
    /// panics, whatever the bytes.
    fn decode_e_row_resolved(&self, k: &[u8]) -> Option<(EntityId, AttrId, Value, Tick, TxId)> {
        let (entity, attr, part, tick, tx) = key::decode_e_key(k).ok()?;
        let (_, value) = self.decode_e_value(attr, part)?;
        Some((entity, attr, value, tick, tx))
    }

    /// A copy of an `E`-layout index (`main`: full keys, else
    /// latest-revision keys) with every value reference replaced by the
    /// full value encoding it names, for doctor to compare rows with values
    /// resolved: two worlds that agree on every row agree on these keys,
    /// whatever references they assigned. The second list holds the stored
    /// keys that can't be resolved: a reference on an attribute that isn't
    /// an unindexed string, an unindexed string not keyed by reference, or a
    /// reference the table lacks. Keys that don't parse as `E` keys at all
    /// are kept as they are (doctor reports them as it always has). Borrows
    /// the index when nothing needs resolving.
    pub(crate) fn resolved_e_index<'i, V: Clone>(
        &self,
        index: &'i OrderedMap<V>,
        main: bool,
    ) -> ResolvedIndex<'i, V> {
        let unindexed_strings = self
            .catalog
            .attributes
            .iter()
            .any(|d| !d.indexed && d.value_type == ValueType::Str);
        if self.indexes.values.is_empty() && !unindexed_strings {
            return (Cow::Borrowed(index), Vec::new());
        }
        let mut out = OrderedMap::new();
        let mut bad = Vec::new();
        for (k, v) in index.iter() {
            let split = if main {
                key::decode_e_key(k).map(|(e, a, part, ..)| (e, a, part))
            } else {
                key::decode_latest_e_key(k).map(|(e, a, part, _)| (e, a, part))
            };
            let Ok((_, attr, part)) = split else {
                out.insert(k.to_vec(), v.clone());
                continue;
            };
            let is_ref = ValueRef::from_key_bytes(part).is_some();
            if self.catalog.attributes.get(attr.0 as usize).is_none() && !is_ref {
                // An unknown attribute: no layout to check against.
                out.insert(k.to_vec(), v.clone());
                continue;
            }
            let resolved = self.e_part_encoding(attr, part);
            let Some(full) = resolved else {
                bad.push(k.to_vec());
                continue;
            };
            // The part sits right after `E|e|a|`; the suffix follows it.
            let start = key::E_ATTR_PREFIX_LEN;
            let mut canonical = Vec::with_capacity(k.len() - part.len() + full.len());
            canonical.extend_from_slice(&k[..start]);
            canonical.extend_from_slice(full);
            canonical.extend_from_slice(&k[start + part.len()..]);
            out.insert(canonical, v.clone());
        }
        (Cow::Owned(out), bad)
    }

    /// Every stored `E` and `R` entry, decoded, in key order: an inspection
    /// hook for tests. Reads the two stored indexes independently; it never
    /// derives one from the other. A row whose key doesn't decode (only
    /// reachable by tampering, see [`World::tamper_index`]) is skipped, with
    /// the same decoder doctor uses.
    #[doc(hidden)]
    pub fn index_dump(&self) -> IndexDump {
        let e = self
            .indexes
            .e_index
            .iter()
            .filter_map(|(k, mark)| {
                let (entity, attr, value, tick, tx) = self.decode_e_row_resolved(k)?;
                Some((entity, attr, value, tick, tx, *mark))
            })
            .collect();
        let r = self
            .indexes
            .r_index
            .iter()
            .filter_map(|(k, mark)| {
                let (target, attr, source, tick, tx) = key::decode_r_key(k).ok()?;
                Some((target, attr, source, tick, tx, *mark))
            })
            .collect();
        let v = self
            .indexes
            .v_index
            .iter()
            .filter_map(|(k, mark)| {
                let (attr, value, entity, tick, tx) = decode_v_row(k)?;
                Some((attr, value, entity, tick, tx, *mark))
            })
            .collect();
        let v_fold = self
            .indexes
            .vfold_index
            .iter()
            .filter_map(|(k, mark)| {
                let (attr, folded, original, entity, tick, tx) = decode_vfold_row(k)?;
                Some((attr, folded, original, entity, tick, tx, *mark))
            })
            .collect();
        IndexDump { e, r, v, v_fold }
    }

    /// Damages one stored index in memory, for doctor's tests: the
    /// indexes are copies of the log, and this is how a copy goes wrong
    /// without the log changing. Only that index changes (nothing else is
    /// kept in step), and nothing is logged. A row number past the end does
    /// nothing. Not part of the supported API.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn tamper_index(&mut self, t: Tamper) {
        fn remove_nth<V>(index: &mut OrderedMap<V>, nth: usize) {
            let key = index.keys().nth(nth).map(<[u8]>::to_vec);
            if let Some(k) = key {
                index.remove(&k);
            }
        }
        fn flip_nth<V: Copy>(index: &mut OrderedMap<V>, nth: usize, flip: fn(V) -> V) {
            let Some(k) = index.keys().nth(nth).map(<[u8]>::to_vec) else {
                return;
            };
            if let Some(v) = index.get(&k).copied() {
                index.set(&k, flip(v));
            }
        }
        fn flip_mark(m: Mark) -> Mark {
            match m {
                Mark::Assert => Mark::Retract,
                // A DELETED marker has no op; it becomes a fact.
                Mark::Retract | Mark::Deleted => Mark::Assert,
            }
        }
        fn flip_op(o: Op) -> Op {
            match o {
                Op::Assert => Op::Retract,
                Op::Retract => Op::Assert,
            }
        }
        match t {
            Tamper::InsertRawKey { index, key } => match index {
                IndexName::E => {
                    self.indexes.e_index.insert(key, Mark::Assert);
                }
                IndexName::R => {
                    self.indexes.r_index.insert(key, Mark::Assert);
                }
                IndexName::ELatest => {
                    self.indexes.e_latest.insert(key, Op::Assert);
                }
                IndexName::RLatest => {
                    self.indexes.r_latest.insert(key, Op::Assert);
                }
                IndexName::V => {
                    self.indexes.v_index.insert(key, Mark::Assert);
                }
                IndexName::VLatest => {
                    self.indexes.v_latest.insert(key, Op::Assert);
                }
                IndexName::VFold => {
                    self.indexes.vfold_index.insert(key, Mark::Assert);
                }
                IndexName::VFoldLatest => {
                    self.indexes.vfold_latest.insert(key, Op::Assert);
                }
            },
            Tamper::RemoveRow { index, nth } => match index {
                IndexName::E => remove_nth(&mut self.indexes.e_index, nth),
                IndexName::R => remove_nth(&mut self.indexes.r_index, nth),
                IndexName::ELatest => remove_nth(&mut self.indexes.e_latest, nth),
                IndexName::RLatest => remove_nth(&mut self.indexes.r_latest, nth),
                IndexName::V => remove_nth(&mut self.indexes.v_index, nth),
                IndexName::VLatest => remove_nth(&mut self.indexes.v_latest, nth),
                IndexName::VFold => remove_nth(&mut self.indexes.vfold_index, nth),
                IndexName::VFoldLatest => remove_nth(&mut self.indexes.vfold_latest, nth),
            },
            Tamper::FlipOp { index, nth } => match index {
                IndexName::E => flip_nth(&mut self.indexes.e_index, nth, flip_mark),
                IndexName::R => flip_nth(&mut self.indexes.r_index, nth, flip_mark),
                IndexName::ELatest => flip_nth(&mut self.indexes.e_latest, nth, flip_op),
                IndexName::RLatest => flip_nth(&mut self.indexes.r_latest, nth, flip_op),
                IndexName::V => flip_nth(&mut self.indexes.v_index, nth, flip_mark),
                IndexName::VLatest => flip_nth(&mut self.indexes.v_latest, nth, flip_op),
                IndexName::VFold => flip_nth(&mut self.indexes.vfold_index, nth, flip_mark),
                IndexName::VFoldLatest => flip_nth(&mut self.indexes.vfold_latest, nth, flip_op),
            },
        }
    }
}
