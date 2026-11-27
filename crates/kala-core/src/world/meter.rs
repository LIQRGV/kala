//! Bounded reads: [`WorkMeter`] counts the index keys a read examines, and
//! [`MeteredView`] is a [`View`] whose reads charge one.

use super::*;

/// A counter of index work, for callers that budget reads (a query
/// executor's step budget): a [`View::metered`] view adds one per index key
/// its reads examine, i.e. every key a seek lands on or steps over and
/// every key a scan walks, the key that ends a prefix walk included.
///
/// The count is deterministic: it depends only on the indexes and the
/// reads, never on timing. Shared by reference, so a `View` stays `Copy`;
/// atomic, so a metered view stays `Send` and `Sync`.
#[derive(Debug, Default)]
pub struct WorkMeter<'a> {
    keys: AtomicU64,
    pub(super) limit: Option<AtomicU64>,
    stop: Option<&'a AtomicBool>,
    deadline: Option<std::time::Instant>,
    interrupted: AtomicU8,
    // ID walks also need a bound when their prefix seeks find no keys.
    // Kept separate to preserve historical index-key accounting.
    ids: AtomicU64,
    id_limit: Option<u64>,
}

impl<'a> WorkMeter<'a> {
    /// Stop reads at an absolute deadline, in addition to work/cancellation limits.
    pub fn with_deadline(mut self, deadline: Option<std::time::Instant>) -> Self {
        self.deadline = deadline;
        self
    }

    /// An unbounded meter at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stop a read after `max_keys` keys, or when `stop` is set.
    /// A limit-crossing key is counted but not processed. Interruption is
    /// sticky: reuse an interrupted meter only after [`Self::take`].
    pub fn bounded(max_keys: u64, stop: Option<&'a AtomicBool>) -> Self {
        Self {
            limit: Some(AtomicU64::new(max_keys)),
            id_limit: Some(max_keys),
            stop,
            ..Self::default()
        }
    }

    /// Update a bounded meter's key allowance before the next read. Does
    /// not reset counted work, clear an interruption or change the initial
    /// ID-walk allowance. Unbounded meters stay unbounded.
    pub fn set_max_keys(&self, max_keys: u64) {
        if let Some(limit) = &self.limit {
            limit.store(max_keys, AtomicOrdering::Relaxed);
        }
    }

    /// The index keys examined since the meter was made or last taken.
    pub fn keys(&self) -> u64 {
        self.keys.load(AtomicOrdering::Relaxed)
    }

    /// Return counted keys and reset this meter for the next read.
    pub fn take(&self) -> u64 {
        self.ids.store(0, AtomicOrdering::Relaxed);
        self.interrupted.store(0, AtomicOrdering::Relaxed);
        self.take_keys()
    }

    /// Take the index-key count while retaining cumulative ID-walk work
    /// and any interruption. Query callers use this between reads, so
    /// repeated empty-prefix scans cannot each spend a fresh ID allowance.
    pub fn take_keys(&self) -> u64 {
        self.keys.swap(0, AtomicOrdering::Relaxed)
    }

    /// Report a latched interruption or a currently set stop flag.
    pub fn check(&self) -> Result<(), Error> {
        let reason = self.interrupted.load(AtomicOrdering::Relaxed);
        let reason = if reason != 0 {
            reason
        } else if self
            .stop
            .is_some_and(|flag| flag.load(AtomicOrdering::Relaxed))
            || self
                .deadline
                .is_some_and(|at| std::time::Instant::now() >= at)
        {
            self.interrupted.store(2, AtomicOrdering::Relaxed);
            2
        } else {
            return Ok(());
        };
        Err(Error::WorkInterrupted {
            reason: if reason == 1 {
                WorkInterruptReason::Limit
            } else {
                WorkInterruptReason::Stopped
            },
        })
    }

    fn count(&self, count: &AtomicU64, n: u64, limit: Option<u64>) -> bool {
        if self.limit.is_none() && self.stop.is_none() && self.deadline.is_none() {
            count.fetch_add(n, AtomicOrdering::Relaxed);
            return true;
        }
        if self.check().is_err() {
            return false;
        }
        let before = count.fetch_add(n, AtomicOrdering::Relaxed);
        if limit.is_some_and(|limit| before.saturating_add(n) > limit) {
            self.interrupted.store(1, AtomicOrdering::Relaxed);
            return false;
        }
        true
    }
}

/// The meter a read charges, if any. Writes always use `NONE`.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Work<'a>(pub(super) Option<&'a WorkMeter<'a>>);

impl Work<'_> {
    pub(super) const NONE: Work<'static> = Work(None);

    #[inline]
    pub(super) fn add(self, keys: u64) -> bool {
        self.0.is_none_or(|m| {
            m.count(
                &m.keys,
                keys,
                m.limit
                    .as_ref()
                    .map(|limit| limit.load(AtomicOrdering::Relaxed)),
            )
        })
    }

    #[inline]
    pub(super) fn id(self) -> bool {
        self.0.is_none_or(|m| {
            m.limit.is_none() && m.stop.is_none() && m.deadline.is_none()
                || m.count(&m.ids, 1, m.id_limit)
        })
    }

    pub(super) fn finish<T>(self, answer: Result<T, Error>) -> Result<T, Error> {
        if let Some(m) = self.0 {
            m.check()?;
        }
        answer
    }
}

/// A read-only, interruptible view. Every potentially scanning operation
/// returns an error on interruption, including [`Self::latest_tick`] and
/// the items of [`Self::entities_of_type`]. Unmetered [`View`] APIs retain
/// their infallible signatures. No write operation uses this wrapper.
#[derive(Clone, Copy, Debug)]
pub struct MeteredView<'a> {
    pub(super) view: View<'a>,
}

impl MeteredView<'_> {
    /// The world this view reads.
    pub fn world(&self) -> &World {
        self.view.world()
    }

    /// Replace the meter, preserving the world and pinned canon.
    pub fn metered<'m>(&self, meter: &'m WorkMeter<'m>) -> MeteredView<'m>
    where
        Self: 'm,
    {
        self.view.metered(meter)
    }

    /// Resolve a stored edge triple at the pinned canon (constant time).
    pub fn edge_id(&self, source: EntityId, attr: &str, target: EntityId) -> Option<EdgeId> {
        self.view.edge_id(source, attr, target)
    }

    /// The pinned canon.
    pub fn canon(&self) -> TxId {
        self.view.canon()
    }

    /// Whether an ID is defined at the pinned canon (constant time).
    pub fn is_defined(&self, id: EntityId) -> bool {
        self.view.is_defined(id)
    }

    /// Resolve an edge at the pinned canon (constant time).
    pub fn edge(&self, id: EdgeId) -> Option<(EntityId, String, EntityId)> {
        self.view.edge(id)
    }

    /// The latest tick, interruptible even at an older canon.
    pub fn latest_tick(&self) -> Result<Tick, Error> {
        self.view.work.finish(Ok(self.view.latest_tick()))
    }

    /// The entities of a type. Each lazy ID read can fail; no partial
    /// answer should be used if any item reports an interruption.
    pub fn entities_of_type(
        &self,
        node_type: &str,
    ) -> Result<impl Iterator<Item = Result<EntityId, Error>> + '_, Error> {
        let ids = self.view.entities_of_type(node_type)?;
        self.view.work.finish(Ok(()))?;
        let mut done = false;
        Ok(ids.scan((), move |_, id| {
            if done {
                return None;
            }
            done = !self.view.work.id();
            Some(self.view.work.finish(Ok(id)))
        }))
    }
    /// Interruptible [`View::as_of`].
    pub fn as_of(&self, entity: EntityId, attr: &str, t: Tick) -> Result<Vec<Value>, Error> {
        self.view.work.finish(self.view.as_of(entity, attr, t))
    }

    /// Visit non-ref attribute values one at a time, without materializing the
    /// whole property. The order is unspecified (unindexed strings use hash
    /// order). Callback errors stop delivery; metering still bounds the scan.
    pub fn visit_non_ref_values(
        &self,
        entity: EntityId,
        attr: &str,
        t: Tick,
        mut visitor: impl FnMut(Value) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let (attr_id, def) = self.view.resolve(entity, attr)?;
        if def.value_type == ValueType::Ref {
            return Err(Error::ExpectedScalarAttribute(attr.to_string()));
        }
        self.view.work.finish(Ok(()))?;
        let mut result = Ok(());
        self.view
            .for_each_e_value_holding(entity, attr_id, t, |encoded| {
                if result.is_ok()
                    && let Some(value) = self.view.world.decode_e_part(attr_id, encoded)
                {
                    result = visitor(value);
                }
            });
        result?;
        self.view.work.finish(Ok(()))
    }

    /// Interruptible [`View::as_of_value`].
    pub fn as_of_value(
        &self,
        entity: EntityId,
        attr: &str,
        value: &Value,
        t: Tick,
    ) -> Result<bool, Error> {
        self.view
            .work
            .finish(self.view.as_of_value(entity, attr, value, t))
    }

    /// Interruptible [`View::as_of_detail`].
    pub fn as_of_detail(
        &self,
        entity: EntityId,
        attr: &str,
        t: Tick,
    ) -> Result<Vec<Change>, Error> {
        self.view
            .work
            .finish(self.view.as_of_detail(entity, attr, t))
    }

    /// Interruptible [`View::attr_scan`].
    pub fn attr_scan(&self, attr: &str, t: Tick) -> Result<Vec<(EntityId, Value)>, Error> {
        self.view.work.finish(self.view.attr_scan(attr, t))
    }

    /// Interruptible [`View::lookup`].
    pub fn lookup(&self, attr: &str, value: &Value, t: Tick) -> Result<Vec<EntityId>, Error> {
        self.view.work.finish(self.view.lookup(attr, value, t))
    }

    /// Interruptible [`View::lookup_ci`].
    pub fn lookup_ci(&self, attr: &str, text: &str, t: Tick) -> Result<Vec<EntityId>, Error> {
        self.view.work.finish(self.view.lookup_ci(attr, text, t))
    }

    /// Interruptible [`View::neighbors`].
    pub fn neighbors(
        &self,
        entity: EntityId,
        attr: &str,
        dir: Direction,
        t: Tick,
    ) -> Result<Vec<(EntityId, EdgeId)>, Error> {
        self.view
            .work
            .finish(self.view.neighbors(entity, attr, dir, t))
    }

    /// Interruptible [`View::history`].
    pub fn history(&self, entity: EntityId, attr: &str) -> Result<Vec<Change>, Error> {
        self.view.work.finish(self.view.history(entity, attr))
    }

    /// Interruptible [`View::history_scan`].
    pub fn history_scan(&self, attr: &str) -> Result<Vec<(EntityId, Change)>, Error> {
        self.view.work.finish(self.view.history_scan(attr))
    }

    /// Interruptible [`View::holds_during`].
    pub fn holds_during(
        &self,
        entity: EntityId,
        attr: &str,
        value: &Value,
    ) -> Result<IntervalSet, Error> {
        self.view
            .work
            .finish(self.view.holds_during(entity, attr, value))
    }

    /// Interruptible [`View::overlaps`].
    pub fn overlaps(&self, attr: &str) -> Result<Vec<Overlap>, Error> {
        self.view.work.finish(self.view.overlaps(attr))
    }
}
