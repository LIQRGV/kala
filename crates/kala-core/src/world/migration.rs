//! Copy-forward migrations use a logged marker and exact, replay-verified copies.
use super::schema_ops::OrderedEntry;
use super::*;
use crate::error::{ErrorEntity, UnconvertibleValue};

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

/// One migration: the attribute being migrated and what it becomes.
struct Migration<'a> {
    /// The attribute's name: the source's before, the replacement's after.
    name: &'a str,
    source: AttrId,
    /// The source's definition before the migration.
    old: &'a AttrDef,
    /// The replacement's definition.
    def: &'a AttrDef,
}

/// Old edge -> the new edge(s) its pair was copied to.
type EdgeMap = BTreeMap<EdgeId, Vec<EdgeId>>;

/// The definition a migration of `old` with `options` creates, and a report
/// holding only the labels the new definition had to drop.
fn migrated_def(old: &AttrDef, options: MigrationOptions) -> (AttrDef, MigrationReport) {
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
    let inverse_given = options.inverse.is_some();
    if let Some(v) = options.inverse {
        def.inverse = Some(v);
    }
    if let Some(v) = options.reverse_cardinality {
        def.reverse_cardinality = Some(v);
    }
    // A bidirectional attribute has no inverse or reverse side, so a
    // directed source's labels are dropped unless the caller restated
    // them (then `check_direction` refuses the contradiction). Replay
    // passes the marker's definition, where both are already `None`,
    // so it regenerates the same drop.
    let mut dropped_inverse = None;
    let mut dropped_reverse_cardinality = None;
    if def.direction == Some(RefDirection::Bidirectional) {
        if !inverse_given {
            dropped_inverse = def.inverse.take();
        }
        if options.reverse_cardinality.is_none() {
            dropped_reverse_cardinality = def.reverse_cardinality.take();
        }
    }
    let report = MigrationReport {
        dropped_inverse,
        dropped_reverse_cardinality,
        ..MigrationReport::default()
    };
    (def, report)
}

/// How a source history converts to the replacement definition.
struct Conversion {
    /// Each change point's converted value; `None` if it can't be converted.
    converted: Vec<Option<Value>>,
    /// The `(entity, converted value)` pairs two different source values
    /// convert to: none of their change points is copied.
    collisions: HashSet<(EntityId, Value)>,
    /// Indexes of the change points that aren't copied (unconvertible or
    /// colliding), ascending.
    invalid: Vec<usize>,
}

impl Conversion {
    /// Converts every change point of `history` to `def`'s value type.
    fn of(history: &[(EntityId, Change)], def: &AttrDef) -> Conversion {
        let converted: Vec<_> = history
            .iter()
            .map(|(_, c)| convert(&c.value, def))
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
        Conversion {
            converted,
            collisions,
            invalid,
        }
    }
}

impl Txn<'_> {
    /// `entity` for an error message: its ID and `TYPE#n` (node types this
    /// transaction declared included, as they're applied to the world).
    fn error_entity(&self, entity: EntityId) -> ErrorEntity {
        ErrorEntity {
            id: entity,
            formatted: self.world.format_id(entity),
        }
    }
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
        let mutual = self.world.catalog.is_bidirectional(attr);
        let view = self.world.at_canon(self.world.latest_tx());
        let ids: Box<dyn Iterator<Item = EntityId> + '_> = match subjects {
            Some(ids) => Box::new(ids.iter().copied()),
            None => Box::new(self.world.catalog.defined_ids()),
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
            let old = txn.world.catalog.attributes[source.0 as usize].clone();
            if old.deprecated {
                return Err(Error::DeprecatedAttribute(name.into()));
            }
            let skip_unconvertible = options.skip_unconvertible;
            let (def, mut report) = migrated_def(&old, options);
            let m = Migration {
                name,
                source,
                old: &old,
                def: &def,
            };
            txn.check_migration(&m)?;
            let history = txn.copy_history(source);
            txn.check_no_pairs_both_ways(&m, &history)?;
            let conversion = Conversion::of(&history, &def);
            if !conversion.invalid.is_empty() && !skip_unconvertible {
                return Err(txn.unconvertible(&m, &history, &conversion.invalid));
            }
            report.skipped = conversion.invalid.len();
            // The marker authorizes only the exact copy sequence replay regenerates.
            txn.order.push(OrderedEntry::Schema(SchemaEdit::Migration {
                source,
                def: def.clone(),
                skip_unconvertible,
            }));
            txn.retire_source(&m)?;
            txn.declare_replacement(&m)?;
            let first_edge = txn.ids.edges.len();
            let edge_map = txn.copy_change_points(&m, history, conversion, &mut report)?;
            txn.copy_edge_properties(source, &edge_map, &mut report)?;
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

    /// The rules a migration is refused by before any history is read: ref
    /// and non-ref attributes don't convert into each other, the new
    /// definition's direction labels fit it, and a state with retracts
    /// can't become an event.
    fn check_migration(&self, m: &Migration<'_>) -> Result<(), Error> {
        if (m.old.value_type == ValueType::Ref) != (m.def.value_type == ValueType::Ref) {
            return Err(Error::Schema(
                "ref and non-ref attributes cannot be migrated into each other".into(),
            ));
        }
        check_direction(m.def)?;
        if m.old.kind == Kind::State && m.def.kind == Kind::Event {
            let logged_retract = self.world.journal.has_retract_of(m.source);
            let staged_retract = self.entries.iter().any(|e| {
                let (f, mark) = e.key_and_mark();
                f.attr == m.source && mark == Mark::Retract
            });
            if logged_retract || staged_retract {
                return Err(Error::Schema(format!(
                    "cannot migrate :{} to event: its log contains retracts",
                    m.name
                )));
            }
        }
        Ok(())
    }

    /// A directed attribute becomes bidirectional only if no pair in its
    /// `history` is stored both ways (they would read as duplicates).
    fn check_no_pairs_both_ways(
        &self,
        m: &Migration<'_>,
        history: &[(EntityId, Change)],
    ) -> Result<(), Error> {
        if m.old.direction != Some(RefDirection::Directed)
            || m.def.direction != Some(RefDirection::Bidirectional)
        {
            return Ok(());
        }
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
        if both.is_empty() {
            return Ok(());
        }
        let entity = |id| self.error_entity(id);
        Err(Error::PairsBothWays {
            attr: m.name.into(),
            pairs: both
                .iter()
                .take(5)
                .map(|(a, b)| (entity(*a), entity(*b)))
                .collect(),
        })
    }

    /// The error for the change points `invalid` (indexes into `history`)
    /// that can't be converted, with the first few as examples.
    fn unconvertible(
        &self,
        m: &Migration<'_>,
        history: &[(EntityId, Change)],
        invalid: &[usize],
    ) -> Error {
        let examples = invalid
            .iter()
            .take(5)
            .map(|i| {
                let (e, c) = &history[*i];
                UnconvertibleValue {
                    entity: self.error_entity(*e),
                    tick: c.tick,
                    value: c.value.clone(),
                }
            })
            .collect();
        Error::Unconvertible {
            attr: m.name.into(),
            from: m.old.value_type,
            to: m.def.value_type,
            count: invalid.len(),
            examples,
        }
    }

    /// Renames the source attribute to the first free `NAME@old` (then
    /// `NAME@old2`, ...) and deprecates it: its history stays readable
    /// under that name, and the name is free for the replacement.
    fn retire_source(&mut self, m: &Migration<'_>) -> Result<(), Error> {
        let name = m.name;
        let mut suffix = 1usize;
        let old_name = loop {
            let candidate = if suffix == 1 {
                format!("{name}@old")
            } else {
                format!("{name}@old{suffix}")
            };
            if !self.world.catalog.attr_by_name.contains_key(&candidate)
                && !self.world.catalog.inverse_by_name.contains_key(&candidate)
            {
                break candidate;
            }
            suffix += 1;
        };
        let mut retired = m.old.clone();
        retired.name = old_name.clone();
        // The inverse belongs to the replacement; old forward history remains readable.
        retired.inverse = None;
        self.schema_edit(
            SchemaEdit::Attribute {
                id: m.source,
                def: retired.clone(),
            },
            SchemaChange::Rename {
                old: name.into(),
                new: old_name.clone(),
            },
            true,
        )?;
        retired.deprecated = true;
        self.schema_edit(
            SchemaEdit::Attribute {
                id: m.source,
                def: retired,
            },
            SchemaChange::Deprecate { name: old_name },
            true,
        )
    }

    /// Declares the replacement attribute under the source's name, as the
    /// next attribute ID.
    fn declare_replacement(&mut self, m: &Migration<'_>) -> Result<(), Error> {
        let target = AttrId(
            u32::try_from(self.world.catalog.attributes.len())
                .map_err(|_| Error::Schema("too many attributes".into()))?,
        );
        self.schema_edit(
            SchemaEdit::Attribute {
                id: target,
                def: m.def.clone(),
            },
            SchemaChange::Attribute {
                name: m.name.into(),
                value_type: m.def.value_type,
                inferred: false,
            },
            true,
        )
    }

    /// Copies every convertible change point of `history` onto the
    /// replacement attribute, counting them in `report`. Returns, for each
    /// old edge whose pair was copied, the new edge(s) it became: the edges
    /// its properties follow.
    fn copy_change_points(
        &mut self,
        m: &Migration<'_>,
        history: Vec<(EntityId, Change)>,
        conversion: Conversion,
        report: &mut MigrationReport,
    ) -> Result<EdgeMap, Error> {
        let Conversion {
            converted,
            collisions,
            ..
        } = conversion;
        // A bidirectional pair becomes two directed facts, one each way.
        let split_pairs = m.old.direction == Some(RefDirection::Bidirectional)
            && m.def.direction == Some(RefDirection::Directed);
        let mut edge_map = EdgeMap::new();
        for ((entity, c), new) in history.into_iter().zip(converted) {
            let Some(value) = new else { continue };
            if collisions.contains(&(entity, value.clone())) {
                continue;
            }
            let mut copies = vec![(entity, value.clone())];
            if split_pairs
                && let Value::Ref(other) = value
                && other != entity
            {
                copies.push((other, Value::Ref(entity)));
            }
            let old_edge = if let Value::Ref(other) = c.value {
                self.world
                    .catalog
                    .edge_of(&self.ids, entity, m.source, other)
            } else {
                None
            };
            for (entity, value) in copies {
                let new_edge = self.copy_point(entity, m.name, &value, &c)?;
                report.change_points += 1;
                if let (Some(old_edge), Some(new_edge)) = (old_edge, new_edge) {
                    let edges = edge_map.entry(old_edge).or_default();
                    if !edges.contains(&new_edge) {
                        edges.push(new_edge);
                    }
                }
            }
        }
        Ok(edge_map)
    }

    /// Copies the properties of every old edge in `edge_map` (every
    /// attribute but the migrated one, `source`) onto the new edges it
    /// became, counting them in `report`.
    fn copy_edge_properties(
        &mut self,
        source: AttrId,
        edge_map: &EdgeMap,
        report: &mut MigrationReport,
    ) -> Result<(), Error> {
        // Collect each property once; memory is linear in the copied history.
        let subjects: Vec<_> = edge_map.keys().map(|e| EntityId::from(*e)).collect();
        let attributes: Vec<_> = if edge_map.is_empty() {
            Vec::new()
        } else {
            self.world
                .catalog
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
            let properties: Vec<_> = self.copy_history_subjects(attr, Some(&subjects));
            for (entity, c) in properties {
                for edge in &edge_map[&entity
                    .as_edge()
                    .expect("invariant: selected edge properties")]
                {
                    self.copy_point((*edge).into(), &prop_name, &c.value, &c)?;
                    report.edge_properties += 1;
                }
            }
        }
        Ok(())
    }

    /// Stages one copied change point: `c` with `value` on `entity`'s
    /// attribute `name`, interning its edge if it needs one. Returns the edge
    /// of a copied `ref` fact.
    fn copy_point(
        &mut self,
        entity: EntityId,
        name: &str,
        value: &Value,
        c: &Change,
    ) -> Result<Option<EdgeId>, Error> {
        // Edge properties of deprecated attributes (e.g. `x@old` from an
        // earlier migration) are history too and must follow their edges.
        let fact = self.world.prepare_fact(
            &self.ids,
            FactRequest {
                entity,
                attr: name,
                value,
                tick: c.tick,
                op: c.op,
                precision: c.precision,
                deprecated: DeprecatedWrites::MigrationCopy,
            },
        )?;
        check_entry_size(name, &fact)?;
        let edge = if let Some(target) = fact.target {
            if let Some(edge) =
                self.world
                    .catalog
                    .prepare_edge(&self.ids, fact.entity, fact.attr, target)?
            {
                self.order.push(OrderedEntry::Edge(self.ids.edges.len()));
                self.ids.push_edge(edge);
            }
            self.world
                .catalog
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
            .catalog
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
        // Replay regenerates the copy on the world as replayed so far: no
        // look at the file (this may itself be a catch-up in progress).
        let mut txn = self.begin_without_catch_up();
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
