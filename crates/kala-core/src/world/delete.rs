//! Enumerate the facts a temporal node deletion would retract. The caller
//! stages ordinary retracts, preserving history, handles and entity identity.
use super::*;

impl Txn<'_> {
    /// The node's state at a tick, including earlier staged writes, in
    /// deterministic stored-key order. Ref states are returned in their stored
    /// orientation, including incoming references. Events are retained.
    ///
    /// A plain delete refuses any held relationship. The operation count,
    /// value bytes, key work and deadline bound this read before staging.
    pub fn delete_facts(
        &self,
        entity: EntityId,
        tick: Tick,
        detach: bool,
        remaining: usize,
        meter: &WorkMeter<'_>,
    ) -> Result<Vec<(EntityId, String, Value)>, Error> {
        meter.check()?;
        if self.s.poisoned {
            return Err(Error::TransactionAborted);
        }
        if !self.world.catalog.is_defined_staged(&self.s.ids, entity) {
            return Err(Error::UnknownEntity(entity));
        }
        self.note_read_entity(entity);
        let work = Work(Some(meter));
        let layers = [&self.world.indexes.e_index, &self.s.staged];
        let mut facts = BTreeMap::new();
        let mut bytes = crate::MAX_ENTRY_BYTES;
        // Every source of candidate histories (E, R, and staged incoming
        // entries) is judged against the same committed + staged E layers.
        let mut visit = |source, attr: AttrId, prefix: &mut Vec<u8>| {
            let def = &self.world.catalog.attributes[attr.0 as usize];
            if def.kind == Kind::Event && (detach || def.value_type != ValueType::Ref) {
                return Ok(());
            }
            if facts.contains_key(prefix.as_slice())
                || !holds(&layers, prefix, tick, TxId(u64::MAX), work)
            {
                return Ok(());
            }
            if !detach && def.value_type == ValueType::Ref {
                return Err(Error::ConnectedEntity {
                    name: self.format_id(entity),
                    handle: self.handle_of(entity).map(str::to_owned),
                    tick,
                });
            }
            if facts.len() >= remaining {
                return Err(Error::DeleteLimit);
            }
            let part = &prefix[key::E_ATTR_PREFIX_LEN..];
            let encoded = if let Some(id) = key::ValueRef::from_key_bytes(part) {
                self.s
                    .ids
                    .values
                    .resolve(id)
                    .or_else(|| self.world.indexes.values.resolve(id))
                    .ok_or_else(|| Error::Corrupt("unresolved string in a fact index".into()))?
            } else {
                part
            };
            bytes = bytes
                .checked_sub(encoded.len().saturating_add(def.name.len()))
                .ok_or(Error::WorkInterrupted {
                    reason: WorkInterruptReason::Limit,
                })?;
            facts.insert(
                prefix.clone(),
                (source, def.name.clone(), Value::decode(encoded)?),
            );
            Ok(())
        };
        let outgoing = key::e_attr_prefix(entity, AttrId(0));
        for index in layers {
            index::try_for_each_history(
                index,
                &outgoing[..key::ENTITY_PREFIX_LEN],
                |k| key::E_ATTR_PREFIX_LEN + key::e_value_bytes(k).map_or(0, |v| v.len()),
                work,
                |prefix| {
                    let attr = key::history_attribute(prefix)
                        .ok_or_else(|| Error::Corrupt("incomplete fact index key".into()))?;
                    visit(entity, attr, prefix)
                },
            )?;
        }
        let target_value = Value::Ref(entity).encode()?;
        let mut incoming = |source, attr| {
            visit(
                source,
                attr,
                &mut key::e_value_prefix(source, attr, &target_value),
            )
        };
        let reverse = key::r_attr_prefix(entity, AttrId(0));
        index::try_for_each_history(
            &self.world.indexes.r_index,
            &reverse[..key::ENTITY_PREFIX_LEN],
            |_| key::R_SOURCE_PREFIX_LEN,
            work,
            |prefix| {
                let source = key::r_source(prefix)
                    .ok_or_else(|| Error::Corrupt("incomplete reverse index key".into()))?;
                let attr = key::history_attribute(prefix)
                    .ok_or_else(|| Error::Corrupt("incomplete reverse index key".into()))?;
                incoming(source, attr)
            },
        )?;
        // The private overlay has a forward index only. Its write-order list
        // also includes corrections and retractions, so consult holds for each.
        for entry in &self.s.entries {
            if !work.add(1) {
                break;
            }
            let (fact, _) = entry.key_and_mark();
            if fact.target == Some(entity) {
                incoming(fact.entity, fact.attr)?;
            }
        }
        meter.check()?;
        Ok(facts.into_values().collect())
    }
}
