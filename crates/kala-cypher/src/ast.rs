//! Parsed Cypher, before variable and time resolution.
pub(crate) mod row_params;
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

/// A complete statement classified by its parsed clauses.
#[derive(Clone, Debug)]
pub enum Statement {
    Read(Query),
    Write(WriteStatement),
    Concurrency(kala_core::Concurrency),
}

/// An update compiled after its reading pipeline has produced a bounded set of bindings.
#[derive(Clone, Debug)]
pub struct WriteStatement {
    pub(crate) read: Query,
    pub(crate) writes: Vec<Update>,
    pub(crate) tick: Option<Expr<String>>,
    /// An authored read tick also supplies the fallback of a NULL write tick.
    pub(crate) has_read_tick: bool,
    pub(crate) returns: Vec<Item>,
    pub(crate) params: Vec<String>,
}

/// One SET or REMOVE item. A missing expression retracts all values of the property.
#[derive(Clone, Debug)]
pub(crate) enum Assignment {
    Property(String, String, Option<Expr<String>>),
    Label(String, String, bool),
}

#[derive(Clone, Debug)]
pub(crate) enum Update {
    Create(Vec<Path>),
    Merge {
        path: Path,
        created: Vec<Assignment>,
        matched: Vec<Assignment>,
    },
    Assign(Vec<Assignment>),
    Delete {
        names: Vec<String>,
        detach: bool,
    },
    /// `CREATE CONSTRAINT … FOR (n:Label) REQUIRE n.prop IS :: TYPE`: the
    /// explicit declaration of attribute `prop` with a value type (and of
    /// the node type, by the schema-on-write rule, if it is new).
    Constraint {
        label: String,
        attribute: String,
        value_type: kala_core::ValueType,
        /// `IF NOT EXISTS`: a name that is already an attribute, whatever
        /// its definition, makes the statement a no-op.
        if_not_exists: bool,
    },
}

impl Update {
    fn paths(&self) -> &[Path] {
        match self {
            Self::Create(paths) => paths,
            Self::Merge { path, .. } => std::slice::from_ref(path),
            _ => &[],
        }
    }
    fn assignments(&self) -> impl Iterator<Item = &Assignment> {
        let (a, b): (&[Assignment], &[Assignment]) = match self {
            Self::Assign(items) => (items, &[]),
            Self::Merge {
                created, matched, ..
            } => (created, matched),
            _ => (&[], &[]),
        };
        a.iter().chain(b)
    }
}

impl WriteStatement {
    /// True if any part of the complete statement needs read permission.
    pub fn requires_read(&self) -> bool {
        self.has_reading_part()
            || !self.returns.is_empty()
            || self
                .writes
                .iter()
                .any(|w| !matches!(w, Update::Create(_) | Update::Constraint { .. }))
            // The parser admits no graph read in a write tick; scanning it
            // here keeps the privilege check independent of that rule.
            || self
                .write_expressions()
                .into_iter()
                .chain(self.tick.as_ref())
                .any(|expr| {
                    let mut props = vec![];
                    expr.attributes(&mut props);
                    !props.is_empty()
                })
    }
    /// Whether the statement only declares schema (a type constraint): its
    /// summary type is `s`, and it stores no facts.
    pub fn schema_only(&self) -> bool {
        self.writes
            .iter()
            .all(|w| matches!(w, Update::Constraint { .. }))
            && !self.writes.is_empty()
    }
    /// Whether the statement begins with a reading pipeline.
    pub fn has_reading_part(&self) -> bool {
        !self.read.clauses.is_empty()
    }
    /// Names of the projected RETURN columns, in authored order.
    pub fn fields(&self) -> Vec<String> {
        self.returns.iter().map(|i| i.name.clone()).collect()
    }
    /// All attribute names in read, write, conditional update and RETURN positions.
    pub fn attributes(&self) -> Vec<&str> {
        let mut names = self.read.attributes();
        for update in &self.writes {
            for path in update.paths() {
                for node in &path.nodes {
                    names.extend(node.props.iter().map(|(key, _)| key.as_str()));
                }
                for edge in &path.edges {
                    names.push(edge.kind.as_str());
                    names.extend(edge.props.iter().map(|(key, _)| key.as_str()));
                }
            }
            if let Update::Constraint { attribute, .. } = update {
                names.push(attribute.as_str());
            }
            for item in update.assignments() {
                match item {
                    Assignment::Property(_, key, _) => names.push(key.as_str()),
                    Assignment::Label(..) => names.push("label"),
                }
            }
        }
        for e in self
            .write_expressions()
            .into_iter()
            .chain(self.tick.as_ref())
            .chain(self.returns.iter().map(|i| &i.expr))
        {
            e.attributes(&mut names);
        }
        names
    }
    pub(crate) fn write_expressions(&self) -> Vec<&Expr<String>> {
        let mut exprs = vec![];
        for update in &self.writes {
            for path in update.paths() {
                for n in &path.nodes {
                    exprs.extend(n.props.iter().map(|(_, e)| e));
                }
                for r in &path.edges {
                    exprs.extend(r.props.iter().map(|(_, e)| e));
                }
            }
            for a in update.assignments() {
                if let Assignment::Property(_, _, Some(e)) = a {
                    exprs.push(e);
                }
            }
        }
        exprs
    }
    /// Parameter names from every clause, in lexical order with repetitions removed.
    pub fn parameters(&self) -> Vec<String> {
        self.params.clone()
    }
}
