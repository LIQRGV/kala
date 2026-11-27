//! World-level catalogue rows, read through the pinned, metered view.
use super::*;
use crate::RowSource;
use kala_core::ValueType;
impl Exec<'_, '_> {
    pub(super) fn source_rows(
        &mut self,
        source: &RowSource,
        columns: &[usize],
        tick: Tick,
        ordinal: usize,
        row: Row,
        out: &mut BTreeSet<Row>,
    ) -> Result<(), QueryError> {
        if columns.len() != 1 {
            return Err(QueryError::Plan("catalogue source needs one column".into()));
        }
        let mut names = BTreeSet::new();
        let mut held = 0usize;
        let mut add = |name: &str, budget: &mut Budget| -> Result<(), QueryError> {
            budget.step(1)?;
            let bytes = 64 + name.len();
            budget.charge_bytes(bytes)?;
            if names.insert(name.to_owned()) {
                held += bytes;
            } else {
                budget.release_bytes(bytes);
            }
            if names.len() > budget.limits.max_rows {
                return Err(QueryError::RowLimit {
                    limit: budget.limits.max_rows,
                });
            }
            Ok(())
        };
        let schema = self.world.at_canon(self.view.canon());
        match source {
            RowSource::Labels => {
                for (name, _, _) in schema.node_type_definitions() {
                    add(name, &mut self.budget)?;
                }
                if schema.resolve_attribute("label").is_ok() {
                    let values = self.scan("label", tick)?;
                    for (_, v) in values.iter() {
                        self.budget.step(1)?;
                        if let Val::Str(_, name) = v {
                            add(name, &mut self.budget)?;
                        }
                    }
                }
            }
            RowSource::RelationshipTypes | RowSource::PropertyKeys => {
                for attr in schema.attributes() {
                    self.budget.step(1)?;
                    if matches!(source, RowSource::RelationshipTypes)
                        && attr.value_type == ValueType::Ref
                    {
                        add(&attr.name, &mut self.budget)?;
                        if let Some(inverse) = &attr.inverse {
                            add(inverse, &mut self.budget)?;
                        }
                    } else if matches!(source, RowSource::PropertyKeys)
                        && attr.value_type != ValueType::Ref
                    {
                        add(&attr.name, &mut self.budget)?;
                    }
                }
                if matches!(source, RowSource::PropertyKeys) {
                    add("kala_handle", &mut self.budget)?;
                }
            }
            RowSource::Server(_) => {
                return Err(QueryError::Plan(
                    "server rows are only available over Bolt".into(),
                ));
            }
        }
        for name in names {
            let mut r = row.clone();
            r[columns[0]] = Some(self.intern_str(&name)?);
            r[ordinal] = Some(Val::I64(out.len() as i64));
            self.budget.emit(out, r)?;
        }
        self.budget.release_bytes(held);
        Ok(())
    }
}
