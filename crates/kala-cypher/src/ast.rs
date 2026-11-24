//! Parsed Cypher, before variable and time resolution.
use kala_query::{Expr, Orientation, Term};
/// One node pattern.
#[derive(Clone, Debug)]
pub struct Node {
    pub name: Option<String>,
    pub labels: Vec<String>,
    pub props: Vec<(String, Expr<String>)>,
}
/// One relationship pattern.
#[derive(Clone, Debug)]
pub struct Edge {
    pub bounds: Option<(usize, usize)>,
    pub name: Option<String>,
    pub kind: String,
    pub direction: Orientation,
    pub props: Vec<(String, Expr<String>)>,
}
/// A chain, with one more node than edges.
#[derive(Clone, Debug)]
pub struct Path {
    pub name: Option<String>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}
/// A MATCH clause and its local tick.
#[derive(Clone, Debug)]
pub struct Match {
    pub optional: bool,
    pub paths: Vec<Path>,
    pub tick: Option<Term>,
    pub canon: Option<Term>,
    pub predicate: Option<Expr<String>>,
}
/// A return item, with its original display name or explicit alias.
#[derive(Clone, Debug)]
pub struct Item {
    pub expr: Expr<String>,
    pub name: String,
}
/// A read query.
#[derive(Clone, Debug)]
pub struct Query {
    pub include_all: bool,
    pub clauses: Vec<Clause>,
    pub items: Vec<Item>,
    pub distinct: bool,
    pub order: Vec<(Expr<String>, bool)>,
    pub skip: Option<Term>,
    pub limit: Option<Term>,
}
impl Query {
    /// All referenced attributes, including maps and sort expressions.
    pub fn attributes(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for clause in &self.clauses {
            let m = match clause {
                Clause::Match(m) => m,
                Clause::With(q, predicate) => {
                    out.extend(q.attributes());
                    if let Some(e) = predicate {
                        e.attributes(&mut out);
                    }
                    continue;
                }
                Clause::Unwind(e, _) => {
                    e.attributes(&mut out);
                    continue;
                }
                Clause::Source(s) => {
                    if let Some(e) = &s.predicate {
                        e.attributes(&mut out);
                    }
                    continue;
                }
            };
            for p in &m.paths {
                for n in &p.nodes {
                    for (k, e) in &n.props {
                        out.push(k.as_str());
                        e.attributes(&mut out);
                    }
                }
                for r in &p.edges {
                    out.push(r.kind.as_str());
                    for (k, e) in &r.props {
                        out.push(k.as_str());
                        e.attributes(&mut out);
                    }
                }
            }
            if let Some(e) = &m.predicate {
                e.attributes(&mut out);
            }
        }
        for i in &self.items {
            i.expr.attributes(&mut out);
        }
        for (e, _) in &self.order {
            e.attributes(&mut out);
        }
        out
    }
}

/// A read pipeline clause. WITH's query contains only projection fields.
#[derive(Clone, Debug)]
pub enum Clause {
    Match(Match),
    With(Box<Query>, Option<Expr<String>>),
    Unwind(Expr<String>, String),
    Source(Source),
}

/// One catalogue or adapter row source and its projected bindings.
#[derive(Clone, Debug)]
pub struct Source {
    pub source: kala_query::RowSource,
    pub yields: Vec<(String, String)>,
    pub predicate: Option<Expr<String>>,
    pub tick: Option<Term>,
    pub canon: Option<Term>,
}
impl Query {
    /// Server metadata can be prepared without opening a world.
    pub fn server_request(&self) -> Option<&kala_query::ServerRequest> {
        self.clauses.iter().find_map(|c| match c {
            Clause::Source(Source {
                source: kala_query::RowSource::Server(request),
                ..
            }) => Some(request),
            _ => None,
        })
    }
}
