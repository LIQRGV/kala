//! Identify parameter UNWINDs whose elements are only inspected as rows.
//! Do this before attribute admission so a row field can never turn into
//! a graph property read, even if the parameter's contents change.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
struct Usage {
    /// Visible variable -> direct UNWIND parameter.
    rows: BTreeMap<String, String>,
    /// A second use of the parameter or a use of the whole row.
    ordinary: BTreeSet<String>,
}

pub(crate) fn resolve(statement: &mut Statement) {
    // A parameter used earlier in a statement can be disqualified by its
    // last use. Analyze every use before lowering any field or source.
    let mut usage = Usage::default();
    usage.statement(statement, false);
    usage.rows.clear();
    usage.statement(statement, true);
}

impl Usage {
    fn statement(&mut self, statement: &mut Statement, lower: bool) {
        let query = match statement {
            Statement::Read(q) => q,
            Statement::Write(w) => &mut w.read,
            Statement::Concurrency(_) => return,
        };
        for clause in &mut query.clauses {
            match clause {
                Clause::Unwind(expr, var) => {
                    if let Expr::Param(p) = expr {
                        self.rows.insert(var.clone(), p.clone());
                        if lower && !self.ordinary.contains(p) {
                            *expr = Expr::RowParameter(p.clone());
                        }
                    } else {
                        self.expr(expr, lower);
                    }
                }
                Clause::Match(m) => {
                    for path in &mut m.paths {
                        self.path(path, lower);
                    }
                    if let Some(e) = &mut m.predicate {
                        self.expr(e, lower);
                    }
                    self.term(m.tick.as_ref());
                    self.term(m.canon.as_ref());
                }
                Clause::With(q, predicate) => {
                    if q.include_all && q.distinct {
                        self.ordinary.extend(self.rows.values().cloned());
                    }
                    self.projection(q, lower);
                    if !q.include_all {
                        self.rows.clear();
                    }
                    if let Some(e) = predicate {
                        self.expr(e, lower);
                    }
                }
                Clause::Source(s) => {
                    self.term(s.tick.as_ref());
                    self.term(s.canon.as_ref());
                    if let Some(e) = &mut s.predicate {
                        self.expr(e, lower);
                    }
                }
            }
        }
        match statement {
            Statement::Read(q) => self.projection(q, lower),
            Statement::Write(w) => {
                // The internal read projection carries the row index to the
                // stager. Only author-written expressions use a whole row.
                for update in &mut w.writes {
                    match update {
                        Update::Create(paths) => {
                            for path in paths {
                                self.path(path, lower);
                            }
                        }
                        Update::Merge {
                            path,
                            created,
                            matched,
                        } => {
                            self.path(path, lower);
                            self.assignments(created, lower);
                            self.assignments(matched, lower);
                        }
                        Update::Assign(items) => self.assignments(items, lower),
                        Update::Delete { names, .. } => {
                            for n in names {
                                self.whole(n);
                            }
                        }
                        Update::Constraint { .. } => {}
                    }
                }
                if let Some(e) = &mut w.tick {
                    self.expr(e, lower);
                }
                for item in &mut w.returns {
                    self.expr(&mut item.expr, lower);
                }
            }
            Statement::Concurrency(_) => {}
        }
    }
    fn term(&mut self, t: Option<&Term>) {
        if let Some(Term::Param(p)) = t {
            self.ordinary.insert(p.clone());
        }
    }
    fn whole(&mut self, var: &str) {
        if let Some(p) = self.rows.get(var) {
            self.ordinary.insert(p.clone());
        }
    }
    fn path(&mut self, path: &mut Path, lower: bool) {
        for n in path
            .name
            .iter()
            .chain(path.nodes.iter().filter_map(|n| n.name.as_ref()))
            .chain(path.edges.iter().filter_map(|e| e.name.as_ref()))
        {
            self.whole(n);
        }
        for (_, e) in path
            .nodes
            .iter_mut()
            .flat_map(|n| &mut n.props)
            .chain(path.edges.iter_mut().flat_map(|e| &mut e.props))
        {
            self.expr(e, lower);
        }
    }
    fn assignments(&mut self, items: &mut [Assignment], lower: bool) {
        for item in items {
            match item {
                Assignment::Property(n, _, expr) => {
                    self.whole(n);
                    if let Some(e) = expr {
                        self.expr(e, lower);
                    }
                }
                Assignment::Label(n, ..) => self.whole(n),
            }
        }
    }
    fn projection(&mut self, q: &mut Query, lower: bool) {
        for i in &mut q.items {
            self.expr(&mut i.expr, lower);
        }
        // ORDER BY sees projected aliases before its input variables.
        let mut shadowed = Vec::new();
        for item in &q.items {
            if let Some(p) = self.rows.remove(&item.name) {
                shadowed.push((item.name.clone(), p));
            }
        }
        for (e, _) in &mut q.order {
            self.expr(e, lower);
        }
        self.rows.extend(shadowed);
        self.term(q.limit.as_ref());
        self.term(q.skip.as_ref());
    }
    fn expr(&mut self, expr: &mut Expr<String>, lower: bool) {
        match expr {
            Expr::Var(v) => self.whole(v),
            Expr::Param(p) => {
                self.ordinary.insert(p.clone());
            }
            Expr::Property { entity, key, .. } => {
                if lower
                    && let Some(p) = self.rows.get(entity)
                    && !self.ordinary.contains(p)
                {
                    *expr = Expr::RowField {
                        parameter: p.clone(),
                        row: entity.clone(),
                        key: key.clone(),
                    };
                }
            }
            Expr::Function(_, e) | Expr::Not(e) | Expr::IsNull(e) => self.expr(e, lower),
            Expr::ListIndex(a, b)
            | Expr::Disjoint(a, b)
            | Expr::Compare(_, a, b)
            | Expr::And(a, b)
            | Expr::Or(a, b)
            | Expr::Xor(a, b) => {
                self.expr(a, lower);
                self.expr(b, lower);
            }
            Expr::List(xs) => {
                for e in xs {
                    self.expr(e, lower);
                }
            }
            Expr::Aggregate {
                argument: Some(e), ..
            } => self.expr(e, lower),
            _ => {}
        }
    }
}
