//! Shared helpers: a span-free "shape" printer for the AST, so tests can state
//! the expected structure as a string.
#![allow(dead_code)]

use kala_datalog::ast::*;

pub fn item(i: &Item) -> String {
    match i {
        Item::Var(s) => format!("?{}", s.name),
        Item::Any(_) => "_".into(),
        Item::Keyword(k, _) => format!(":{k}"),
        Item::Id(s, _) => format!("#{s}"),
        Item::Str(s, _) => format!("{s:?}"),
        Item::Int(n, _) => format!("{n}"),
        Item::Float(f, _) => format!("{f:?}f"),
        Item::Bool(b, _) => format!("{b}"),
    }
}

pub fn tick(t: &TickExpr) -> String {
    match t {
        TickExpr::Int(n, _) => format!("{n}"),
        TickExpr::Var(s) => format!("?{}", s.name),
    }
}

pub fn pat(p: &Pattern) -> String {
    format!("[{} :{} {}]", item(&p.e), p.a.name, item(&p.v))
}

pub fn shape(q: &Query) -> String {
    let mut out = String::new();
    out += &format!(
        "find({})",
        q.find
            .iter()
            .map(|s| format!("?{}", s.name))
            .collect::<Vec<_>>()
            .join(" ")
    );
    out += &format!(
        " in({})",
        q.inputs
            .iter()
            .map(|i| match i {
                Input::Db(_) => "$".to_string(),
                Input::Var(s) => format!("?{}", s.name),
            })
            .collect::<Vec<_>>()
            .join(" ")
    );
    match &q.at {
        Some(t) => out += &format!(" at({})", tick(t)),
        None => out += " at()",
    }
    match &q.canon {
        Some(CanonExpr::Int(n, _)) => out += &format!(" canon({n})"),
        Some(CanonExpr::Var(s)) => out += &format!(" canon(?{})", s.name),
        None => out += " canon()",
    }
    out += " where{";
    for c in &q.clauses {
        match c {
            Clause::Pattern(p) => out += &format!(" {}", pat(p)),
            Clause::At {
                tick: t, patterns, ..
            } => {
                out += &format!(
                    " (at {} {})",
                    tick(t),
                    patterns.iter().map(pat).collect::<Vec<_>>().join(" ")
                )
            }
            Clause::Pred(p) => {
                out += &format!(
                    " [({} {} {})]",
                    p.op.symbol(),
                    item(&p.left),
                    item(&p.right)
                )
            }
        }
    }
    out += " }";
    out
}

/// Wrap `where_` clauses in a minimal valid query.
pub fn q(where_: &str) -> String {
    format!("[:find ?e :where {where_}]")
}
