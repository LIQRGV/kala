//! Copy-forward migrations use a logged marker and exact, replay-verified copies.
use super::schema_ops::{OrderedEntry, type_name};
use super::*;

fn convert(value: &Value, def: &AttrDef) -> Option<Value> {
    let v = match (value, def.value_type) {
        (Value::I64(v), ValueType::F64) if v.unsigned_abs() <= (1_u64 << 53) => {
            Value::F64(*v as f64)
        }
        (Value::F64(v), ValueType::I64)
            if v.is_finite()
                && v.fract() == 0.0
                && *v >= i64::MIN as f64
                && *v < 9_223_372_036_854_775_808.0 =>
        {
            Value::I64(*v as i64)
        }
        (Value::Bool(v), ValueType::I64) => Value::I64(i64::from(*v)),
        (Value::I64(0), ValueType::Bool) => Value::Bool(false),
        (Value::I64(1), ValueType::Bool) => Value::Bool(true),
        (Value::I64(v), ValueType::Str) => Value::Str(v.to_string()),
        (Value::F64(v), ValueType::Str) => Value::Str(v.to_string()),
        (Value::Bool(v), ValueType::Str) => Value::Str(v.to_string()),
        (Value::Str(v), ValueType::I64) if v.trim() == v => Value::I64(v.parse().ok()?),
        (Value::Str(v), ValueType::F64) if v.trim() == v => {
            let f: f64 = v.parse().ok()?;
            if f.is_nan() {
                return None;
            }
            Value::F64(if f == 0.0 { 0.0 } else { f })
        }
        (Value::Str(v), ValueType::Bool) => Value::Bool(match v.as_str() {
            "true" => true,
            "false" => false,
            _ => return None,
        }),
        (v, t) if v.value_type() == t => v.clone(),
        _ => return None,
    };
    if def.indexed && v.check_indexed().is_err() {
        return None;
    }
    Some(v)
}

impl Txn<'_> {
    /// Current history including earlier staged writes, in canonical orientation.
    fn copy_history(&self, attr: AttrId) -> Vec<(EntityId, Change)> {
        self.copy_history_subjects(attr, None)
    }
    fn copy_history_subjects(
        &self,
        attr: AttrId,
        subjects: Option<&[EntityId]>,
    ) -> Vec<(EntityId, Change)> {
        let mut points = BTreeMap::new();
        let mutual = self.world.is_bidirectional(attr);
        let view = self.world.at_canon(self.world.latest_tx);
        let ids: Box<dyn Iterator<Item = EntityId> + '_> = match subjects {
            Some(ids) => Box::new(ids.iter().copied()),
            None => Box::new(self.world.defined_ids()),
        };
        for entity in ids {
            for change in view.changes(entity, attr) {
                if subjects.is_none()
                    && mutual
                    && let Value::Ref(target) = change.value
                    && entity > target
                {
                    continue;
                }
                points.insert((entity, change.tick, change.value.clone()), change);
            }
        }
        for staged in &self.entries {
            let (fact, mark) = staged.key_and_mark();
            if fact.attr != attr {
                continue;
            }
            let value = Value::decode(&fact.value).expect("invariant: staged fact values encode");
            let reverse = if subjects.is_some() && mutual {
                match &value {
                    Value::Ref(target) if *target != fact.entity => {
                        Some((*target, Value::Ref(fact.entity)))
                    }
                    _ => None,
                }
            } else {
                None
            };
            // Restricted property reads are oriented around their old edge
            // subjects, even when a mutual pair is stored under the other end.
            // The subject list comes from ordered edge-map keys; logarithmic
            // membership avoids quadratic scans of large ref migrations.
            for (entity, value) in std::iter::once((fact.entity, value)).chain(reverse) {
                if subjects.is_some_and(|ids| ids.binary_search(&entity).is_err()) {
                    continue;
                }
                let key = (entity, fact.tick, value.clone());
                if let Some(op) = mark.op() {
                    points.insert(
                        key,
                        Change {
                            tick: fact.tick,
                            op,
                            value,
                            precision: fact.precision,
                        },
                    );
                } else {
                    points.remove(&key);
                }
            }
        }
        points.into_iter().map(|((e, _, _), c)| (e, c)).collect()
    }
    /// Copies every current change point into a new attribute, preserving ticks,
    /// precision, redundant assertions, and edge properties. Errors abort the txn.
    pub fn migrate_attribute(
        &mut self,
        name: &str,
        options: MigrationOptions,
    ) -> Result<MigrationReport, Error> {
        self.guard(|txn| {
            let source = txn.attr_id(name)?;
            let old = txn.world.attributes[source.0 as usize].clone();
            if old.deprecated {
                return Err(Error::DeprecatedAttribute(name.into()));
            }
            let mut def = old.clone();
            def.inferred = false;
            def.deprecated = false;
            if let Some(v) = options.value_type {
                def.value_type = v;
            }
            if let Some(v) = options.kind {
                def.kind = v;
            }
            if let Some(v) = options.cardinality {
                def.cardinality = v;
            }
            if let Some(v) = options.direction {
                def.direction = Some(v);
            }
            if let Some(v) = options.indexed {
                def.indexed = v;
            }
            if let Some(v) = options.inverse {
                def.inverse = Some(v);
            }
            if let Some(v) = options.reverse_cardinality {
                def.reverse_cardinality = Some(v);
            }
            if (old.value_type == ValueType::Ref) != (def.value_type == ValueType::Ref) {
                return Err(Error::Schema(
                    "ref and non-ref attributes cannot be migrated into each other".into(),
                ));
            }
            check_direction(&def)?;
            if old.kind == Kind::State && def.kind == Kind::Event {
                let logged_retract = txn.world.entries.iter().any(|e| {
                    as_fact(&e.body)
                        .is_some_and(|(_, a, _, _, op, _)| a == source && op == Op::Retract)
                });
                let staged_retract = txn.entries.iter().any(|e| {
                    let (f, m) = e.key_and_mark();
                    f.attr == source && m == Mark::Retract
                });
                if logged_retract || staged_retract {
                    return Err(Error::Schema(format!(
                        "cannot migrate :{name} to event: its log contains retracts"
                    )));
                }
            }
            let history = txn.copy_history(source);
            if old.direction == Some(RefDirection::Directed)
                && def.direction == Some(RefDirection::Bidirectional)
            {
                let pairs: HashSet<_> = history
                    .iter()
                    .filter_map(|(e, c)| {
                        if let Value::Ref(t) = c.value {
                            Some((*e, t))
                        } else {
                            None
                        }
                    })
                    .collect();
                let mut both: Vec<_> = pairs
                    .iter()
                    .filter(|(a, b)| a < b && pairs.contains(&(*b, *a)))
                    .copied()
                    .collect();
                both.sort_unstable();
                if !both.is_empty() {
                    return Err(Error::Schema(format!(
                        "cannot migrate :{name}: pairs have history in both directions: {:?}",
                        &both[..both.len().min(5)]
                    )));
                }
            }
            let converted: Vec<_> = history
                .iter()
                .map(|(_, c)| convert(&c.value, &def))
                .collect();
            let mut originals: HashMap<(EntityId, Value), Value> = HashMap::new();
            let mut collisions = HashSet::new();
            for ((entity, c), new) in history.iter().zip(&converted) {
                if let Some(new) = new {
                    let key = (*entity, new.clone());
                    if let Some(previous) = originals.insert(key.clone(), c.value.clone())
                        && previous != c.value
                    {
                        collisions.insert(key);
                    }
                }
            }
            let invalid: Vec<_> = history
                .iter()
                .zip(&converted)
                .enumerate()
                .filter_map(|(i, ((e, _), v))| {
                    (v.as_ref()
                        .is_none_or(|v| collisions.contains(&(*e, v.clone()))))
                    .then_some(i)
                })
                .collect();
            if !invalid.is_empty() && !options.skip_unconvertible {
                let examples: Vec<_> = invalid
                    .iter()
                    .take(5)
                    .map(|i| {
                        let (e, c) = &history[*i];
                        format!("({:?}, {}, {:?})", e, c.tick.0, c.value)
                    })
                    .collect();
                return Err(Error::Schema(format!(
                    "{} unconvertible change points migrating :{} ({} -> {}): {}",
                    invalid.len(),
                    name,
                    type_name(old.value_type),
                    type_name(def.value_type),
                    examples.join(", ")
                )));
            }
            let mut report = MigrationReport {
                skipped: invalid.len(),
                ..MigrationReport::default()
            };
            // The marker authorizes only the exact copy sequence replay regenerates.
            txn.order.push(OrderedEntry::Schema(SchemaEdit::Migration {
                source,
                def: def.clone(),
                skip_unconvertible: options.skip_unconvertible,
            }));
            let mut suffix = 1usize;
            let old_name = loop {
                let candidate = if suffix == 1 {
                    format!("{name}@old")
                } else {
                    format!("{name}@old{suffix}")
                };
                if !txn.world.attr_by_name.contains_key(&candidate)
                    && !txn.world.inverse_by_name.contains_key(&candidate)
                {
                    break candidate;
                }
                suffix += 1;
            };
            let mut retired = old.clone();
            retired.name = old_name.clone();
            // The inverse belongs to the replacement; old forward history remains readable.
            retired.inverse = None;
            txn.schema_edit(
                SchemaEdit::Attribute {
                    id: source,
                    def: retired.clone(),
                },
                SchemaChange::Rename {
                    old: name.into(),
                    new: old_name.clone(),
                },
                true,
            )?;
            retired.deprecated = true;
            txn.schema_edit(
                SchemaEdit::Attribute {
                    id: source,
                    def: retired,
                },
                SchemaChange::Deprecate { name: old_name },
                true,
            )?;
            let target = AttrId(
                u32::try_from(txn.world.attributes.len())
                    .map_err(|_| Error::Schema("too many attributes".into()))?,
            );
            txn.schema_edit(
                SchemaEdit::Attribute {
                    id: target,
                    def: def.clone(),
                },
                SchemaChange::Attribute {
                    name: name.into(),
                    value_type: def.value_type,
                    inferred: false,
                },
                true,
            )?;
            let first_edge = txn.ids.edges.len();
            let mut edge_map: BTreeMap<EdgeId, Vec<EdgeId>> = BTreeMap::new();
            for ((entity, c), new) in history.into_iter().zip(converted) {
                let Some(value) = new else { continue };
                if collisions.contains(&(entity, value.clone())) {
                    continue;
                }
                let mut copies = vec![(entity, value.clone())];
                if old.direction == Some(RefDirection::Bidirectional)
                    && def.direction == Some(RefDirection::Directed)
                    && let Value::Ref(other) = value
                    && other != entity
                {
                    copies.push((other, Value::Ref(entity)));
                }
                let old_edge = if let Value::Ref(other) = c.value {
                    txn.world.edge_of(&txn.ids, entity, source, other)
                } else {
                    None
                };
                for (entity, value) in copies {
                    let new_edge = txn.copy_point(entity, name, &value, &c)?;
                    report.change_points += 1;
                    if let (Some(old_edge), Some(new_edge)) = (old_edge, new_edge) {
                        let edges = edge_map.entry(old_edge).or_default();
                        if !edges.contains(&new_edge) {
                            edges.push(new_edge);
                        }
                    }
                }
            }
            // Collect each property once; memory is linear in the copied history.
            let subjects: Vec<_> = edge_map.keys().map(|e| EntityId::from(*e)).collect();
            let attributes: Vec<_> = if edge_map.is_empty() {
                Vec::new()
            } else {
                txn.world
                    .attributes
                    .iter()
                    .enumerate()
                    .map(|(i, d)| (AttrId(i as u32), d.name.clone()))
                    .collect()
            };
            for (attr, prop_name) in attributes {
                if attr == source {
                    continue;
                }
                let properties: Vec<_> = txn.copy_history_subjects(attr, Some(&subjects));
                for (entity, c) in properties {
                    for edge in &edge_map[&entity
                        .as_edge()
                        .expect("invariant: selected edge properties")]
                    {
                        txn.copy_point((*edge).into(), &prop_name, &c.value, &c)?;
                        report.edge_properties += 1;
                    }
                }
            }
            report.edges = txn.ids.edges.len() - first_edge;
            txn.schema_changes.push(SchemaChange::Migration {
                name: name.into(),
                from: old.value_type,
                to: def.value_type,
                report: report.clone(),
            });
            Ok(report)
        })
    }
    fn copy_point(
        &mut self,
        entity: EntityId,
        name: &str,
        value: &Value,
        c: &Change,
    ) -> Result<Option<EdgeId>, Error> {
        let mut fact = self
            .world
            .prepare_fact(&self.ids, entity, name, value, c.tick, c.op)?;
        fact.precision = c.precision;
        check_entry_size(name, &fact)?;
        let edge = if let Some(target) = fact.target {
            if let Some(edge) =
                self.world
                    .prepare_edge(&self.ids, fact.entity, fact.attr, target)?
            {
                self.order.push(OrderedEntry::Edge(self.ids.edges.len()));
                self.ids.push_edge(edge);
            }
            self.world
                .edge_of(&self.ids, fact.entity, fact.attr, target)
        } else {
            None
        };
        if has_key(&self.staged, &fact) {
            return Err(Error::TxnKeyConflict {
                entity,
                attr: name.into(),
                tick: c.tick,
            });
        }
        self.stage(Staged::Fact(fact));
        Ok(edge)
    }
}

impl World {
    pub(super) fn migration_expected(
        &mut self,
        source: AttrId,
        def: &AttrDef,
        skip: bool,
    ) -> Result<std::collections::VecDeque<EntryBody>, Error> {
        let old = self
            .attributes
            .get(source.0 as usize)
            .ok_or_else(|| Error::Schema("unknown migration source".into()))?;
        if old.name != def.name || def.inferred || def.deprecated {
            return Err(Error::Schema("invalid migration definition".into()));
        }
        let name = old.name.clone();
        let options = MigrationOptions {
            value_type: Some(def.value_type),
            kind: Some(def.kind),
            cardinality: Some(def.cardinality),
            direction: def.direction,
            indexed: Some(def.indexed),
            inverse: def.inverse.clone(),
            reverse_cardinality: def.reverse_cardinality,
            skip_unconvertible: skip,
        };
        let mut txn = self.begin();
        txn.migrate_attribute(&name, options)?;
        let encoded = txn.encode_ordered()?;
        let expected_marker = EntryBody::Schema(SchemaEdit::Migration {
            source,
            def: def.clone(),
            skip_unconvertible: skip,
        });
        if encoded
            .entries
            .first()
            .is_none_or(|e| e.body != expected_marker)
        {
            return Err(Error::Schema(
                "migration marker does not match the requested definition".into(),
            ));
        }
        let n = encoded.entries.len();
        Ok(encoded
            .entries
            .into_iter()
            .skip(1)
            .take(n.saturating_sub(2))
            .map(|e| e.body)
            .collect())
    }
}
