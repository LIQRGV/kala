//! Bounded trail traversal. One neighbor seek per frontier, no attribute scans.
use super::*;
use crate::{Orientation, PathValue, RowAction, RowFn};
use kala_core::{Direction, ValueType};

struct Frontier {
    neighbors: Vec<(EntityId, EdgeId)>,
    next: usize,
    bytes: usize,
}
impl Exec<'_, '_> {
    fn intern_path(
        &mut self,
        nodes: &[EntityId],
        edges: &[EdgeId],
        forward: &[bool],
    ) -> Result<Val, QueryError> {
        self.budget.step(edges.len() as u64 + 1)?;
        let bytes = 128 + nodes.len().saturating_mul(8) + edges.len().saturating_mul(9);
        self.budget.charge_bytes(bytes)?;
        let path = PathValue::new(nodes.to_vec(), edges.to_vec(), forward.to_vec())?;
        let (value, new) = self.interner.path(path)?;
        if !new {
            self.budget.release_bytes(bytes);
        }
        Ok(value)
    }
    pub(super) fn edge_ids(&mut self, value: &Val) -> Result<Vec<EdgeId>, QueryError> {
        let values: &[Val] = match value {
            Val::List(_, xs) => xs,
            _ => std::slice::from_ref(value),
        };
        self.budget.step(values.len() as u64)?;
        self.budget.charge_bytes(values.len().saturating_mul(8))?;
        values
            .iter()
            .map(|v| match v {
                Val::Ref(id) => id
                    .as_edge()
                    .ok_or_else(|| QueryError::TypeMismatch("relationship expected".into())),
                _ => Err(QueryError::TypeMismatch(
                    "relationship or relationship list expected".into(),
                )),
            })
            .collect()
    }
    pub(super) fn row_function(&mut self, f: RowFn, value: Val) -> Result<Val, QueryError> {
        if value == Val::Null {
            return Ok(Val::Null);
        }
        let values = match (f, value) {
            (RowFn::Length, Val::Path(_, p)) => return Ok(Val::I64(p.edges().len() as i64)),
            (RowFn::Size, Val::List(_, xs)) => return Ok(Val::I64(xs.len() as i64)),
            (RowFn::Nodes, Val::Path(_, p)) => {
                self.budget.charge_bytes(
                    64 + p.nodes().len().saturating_mul(std::mem::size_of::<Val>()),
                )?;
                p.nodes().iter().copied().map(Val::Ref).collect::<Vec<_>>()
            }
            (RowFn::Relationships, Val::Path(_, p)) => {
                self.budget.charge_bytes(
                    64 + p.edges().len().saturating_mul(std::mem::size_of::<Val>()),
                )?;
                p.edges()
                    .iter()
                    .map(|e| Val::Ref((*e).into()))
                    .collect::<Vec<_>>()
            }
            _ => {
                return Err(QueryError::TypeMismatch(format!(
                    "{} needs {}",
                    f.name(),
                    if f == RowFn::Size { "a list" } else { "a path" }
                )));
            }
        };
        let bytes = 64 + values.len().saturating_mul(std::mem::size_of::<Val>());
        self.budget.step(values.len() as u64)?;
        let (v, new) = self.interner.list(values, self.budget.limits.max_bytes)?;
        if !new {
            self.budget.release_bytes(bytes);
        }
        Ok(v)
    }
    pub(super) fn make_path(
        &mut self,
        row: &Row,
        start: usize,
        segments: &[usize],
    ) -> Result<Val, QueryError> {
        let Some(Val::Ref(start)) = row[start] else {
            return Ok(Val::Null);
        };
        let mut count = 0usize;
        for s in segments {
            self.budget.step(1)?;
            count = count.saturating_add(match row[*s].as_ref() {
                Some(Val::Path(_, p)) => p.edges().len(),
                Some(Val::Ref(_)) => 1,
                Some(Val::Null) => return Ok(Val::Null),
                _ => return Err(QueryError::TypeMismatch("invalid path segment".into())),
            });
        }
        let held = 64 + (count + 1).saturating_mul(32);
        self.budget.charge_bytes(held)?;
        let mut nodes = Vec::with_capacity(count + 1);
        let mut edges = Vec::with_capacity(count);
        let mut directions = Vec::with_capacity(count);
        nodes.push(start);
        for s in segments {
            self.budget.step(1)?;
            let from = *nodes.last().expect("path start");
            match row[*s].as_ref() {
                Some(Val::Path(_, p)) => {
                    if p.nodes()[0] != from {
                        return Err(QueryError::TypeMismatch(
                            "disconnected path segments".into(),
                        ));
                    }
                    nodes.extend_from_slice(&p.nodes()[1..]);
                    edges.extend_from_slice(p.edges());
                    directions.extend_from_slice(p.forward());
                }
                Some(Val::Ref(id)) => {
                    let id = id.as_edge().ok_or_else(|| {
                        QueryError::TypeMismatch("path segment needs a relationship".into())
                    })?;
                    let (a, _, b) = self.view.edge(id).ok_or_else(|| {
                        QueryError::TypeMismatch("unknown path relationship".into())
                    })?;
                    let forward = from == a;
                    if !forward && from != b {
                        return Err(QueryError::TypeMismatch(
                            "disconnected path relationship".into(),
                        ));
                    }
                    nodes.push(if forward { b } else { a });
                    edges.push(id);
                    directions.push(forward);
                }
                _ => unreachable!("validated segments"),
            }
        }
        let result = self.intern_path(&nodes, &edges, &directions);
        self.budget.release_bytes(held);
        result
    }
    pub(super) fn trail(
        &mut self,
        action: &RowAction<usize, Val>,
        row: Row,
        out: &mut BTreeSet<Row>,
    ) -> Result<(), QueryError> {
        let RowAction::Trail {
            source,
            target,
            path,
            attr,
            direction,
            tick,
            min,
            max,
            excluded,
            properties,
        } = action
        else {
            unreachable!("trail action")
        };
        let Some(Val::Ref(start)) = row[*source] else {
            return Ok(());
        };
        let t = rows::tick(tick);
        let schema = self.world.at_canon(self.view.canon());
        let typed = schema
            .resolve_attribute(attr)
            .is_ok_and(|(d, _)| d.value_type == ValueType::Ref);
        if !typed && *min > 0 {
            return Ok(());
        }
        let dir = match direction {
            Orientation::Out => Direction::Out,
            Orientation::In => Direction::In,
            Orientation::Either => Direction::Both,
        };
        let mut forbidden = Vec::new();
        for s in excluded {
            if let Some(v) = &row[*s] {
                forbidden.extend(self.edge_ids(v)?);
            }
        }
        let held = 256 + (max + 1).saturating_mul(128);
        self.budget.charge_bytes(held)?;
        let mut nodes = vec![start];
        let mut edges = Vec::with_capacity(*max);
        let mut forward = Vec::with_capacity(*max);
        let mut stack: Vec<Frontier> = Vec::with_capacity(max + 1);
        loop {
            self.budget.step(1)?; // includes cancellation at every visit/backtrack
            let depth = edges.len();
            let from = *nodes.last().expect("trail start");
            if depth >= *min {
                let fits = row[*target].as_ref().is_none_or(|v| *v == Val::Ref(from));
                if fits {
                    if out.len() >= self.budget.limits.max_rows {
                        return Err(QueryError::RowLimit {
                            limit: self.budget.limits.max_rows,
                        });
                    }
                    let history = self.intern_path(&nodes, &edges, &forward)?;
                    let mut r = row.clone();
                    r[*target] = Some(Val::Ref(from));
                    r[*path] = Some(history);
                    self.budget.emit(out, r)?;
                }
            }
            if depth < *max && typed {
                self.budget.step(1)?;
                let found = self.read_view().neighbors(from, attr, dir, t)?;
                self.charge_index_work()?;
                self.budget.step(found.len() as u64)?;
                let bytes = 64 + found.len().saturating_mul(32);
                self.budget.charge_bytes(bytes)?;
                stack.push(Frontier {
                    neighbors: found,
                    next: 0,
                    bytes,
                });
            } else {
                stack.push(Frontier {
                    neighbors: vec![],
                    next: 0,
                    bytes: 0,
                });
            }
            let next = loop {
                self.budget.step(1)?;
                let Some(frame) = stack.last_mut() else {
                    break None;
                };
                if let Some(&(node, edge)) = frame.neighbors.get(frame.next) {
                    frame.next += 1;
                    self.budget
                        .step((edges.len() + forbidden.len()) as u64 + 1)?;
                    if edges.contains(&edge) || forbidden.contains(&edge) {
                        continue;
                    }
                    let mut matches = true;
                    for (key, expr) in properties {
                        let a = self.property(edge.into(), key, t)?;
                        let b = self.expression(expr, &row)?;
                        if rows::equal(&a, &b, &mut self.budget)? != Some(true) {
                            matches = false;
                            break;
                        }
                    }
                    if !matches {
                        continue;
                    }
                    let (stored, _, _) = self
                        .view
                        .edge(edge)
                        .ok_or_else(|| QueryError::TypeMismatch("unknown trail edge".into()))?;
                    break Some((node, edge, stored == *nodes.last().expect("trail node")));
                }
                let frame = stack.pop().expect("frontier exists");
                self.budget.release_bytes(frame.bytes);
                if !stack.is_empty() {
                    nodes.pop();
                    edges.pop();
                    forward.pop();
                }
            };
            let Some((node, edge, dir)) = next else {
                break;
            };
            nodes.push(node);
            edges.push(edge);
            forward.push(dir);
        }
        self.budget
            .release_bytes(held + forbidden.len().saturating_mul(8));
        Ok(())
    }
}
