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
    pub name: Option<String>,
    pub kind: String,
    pub direction: Orientation,
    pub props: Vec<(String, Expr<String>)>,
}
/// A chain, with one more node than edges.
#[derive(Clone, Debug)]
pub struct Path {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}
/// A MATCH clause and its local tick.
#[derive(Clone, Debug)]
pub struct Match {
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
    pub matches: Vec<Match>,
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
        for m in &self.matches {
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
