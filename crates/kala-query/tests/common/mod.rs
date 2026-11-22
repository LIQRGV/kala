//! Shared helpers for the executor tests (plan builders, result helpers).
#![allow(dead_code)]

use kala_core::{EntityId, Tick, Value, World};
use kala_query::{
    Access, AttrRef, CanonSource, CmpOp, Op, Plan, Predicate, ResultSet, Term, TickSource, Var,
};

pub const VARROW: &str = include_str!("../../../kala-core/tests/fixtures/varrow.yaml");

pub fn varrow() -> World {
    kala_core::load_fixture(VARROW).expect("varrow fixture loads")
}

pub fn v(n: &str) -> Var {
    Var(n.to_string())
}
pub fn tv(n: &str) -> Term {
    Term::Var(v(n))
}
pub fn tp(n: &str) -> Term {
    Term::Param(n.to_string())
}
pub fn tr(e: EntityId) -> Term {
    Term::Const(Value::Ref(e))
}
pub fn ti(n: i64) -> Term {
    Term::Const(Value::I64(n))
}
pub fn ts(s: &str) -> Term {
    Term::Const(Value::Str(s.to_string()))
}
pub fn b(o: Op) -> Box<Op> {
    Box::new(o)
}

pub fn at(t: i64) -> TickSource {
    TickSource::Const(Tick(t))
}

/// `Expand` over `input`.
pub fn expand_on(
    input: Op,
    entity: Term,
    attr: &str,
    value: Term,
    tick: TickSource,
    access: Access,
) -> Op {
    Op::Expand {
        input: b(input),
        entity,
        attr: AttrRef::Named(attr.to_string()),
        value,
        tick,
        access,
    }
}

/// `Expand` over `Unit`.
pub fn expand(entity: Term, attr: &str, value: Term, tick: TickSource, access: Access) -> Op {
    expand_on(Op::Unit, entity, attr, value, tick, access)
}

pub fn handle_expand(entity: Term, value: Term, access: Access) -> Op {
    Op::Expand {
        input: b(Op::Unit),
        entity,
        attr: AttrRef::Handle,
        value,
        tick: TickSource::Default,
        access,
    }
}

pub fn filter(input: Op, op: CmpOp, left: Term, right: Term) -> Op {
    Op::Filter {
        input: b(input),
        pred: Predicate { op, left, right },
    }
}

pub fn join(l: Op, r: Op) -> Op {
    Op::Join {
        left: b(l),
        right: b(r),
    }
}

pub fn project(input: Op, vars: &[&str]) -> Op {
    Op::Project {
        input: b(input),
        vars: vars.iter().map(|n| v(n)).collect(),
    }
}

pub fn node_scan(input: Op, var: &str, ty: &str) -> Op {
    Op::NodeScan {
        input: b(input),
        var: v(var),
        node_type: ty.to_string(),
    }
}

pub fn node_seek(input: Op, var: &str, entity: Term) -> Op {
    Op::NodeSeek {
        input: b(input),
        var: v(var),
        entity,
    }
}

/// A plan with the given find vars, default tick `Default`, latest canon.
pub fn plan(find: &[&str], root: Op) -> Plan {
    Plan {
        find: find.iter().map(|n| v(n)).collect(),
        params: vec![],
        default_tick: TickSource::Default,
        canon: CanonSource::Latest,
        root,
    }
}

/// Single-column plan: `Project <var> (Expand ..)`.
pub fn one_col(var: &str, root: Op) -> Plan {
    plan(&[var], project(root, &[var]))
}

/// The rows of a one-column result, as values.
pub fn col0(rs: &ResultSet) -> Vec<Value> {
    rs.rows.iter().map(|r| r[0].clone()).collect()
}

/// Entities as the sorted, deduplicated `Value::Ref` rows a one-column
/// result must have.
pub fn ref_rows(mut es: Vec<EntityId>) -> Vec<Value> {
    es.sort();
    es.dedup();
    es.into_iter().map(Value::Ref).collect()
}

/// Sorts and dedups rows by `Value`'s order (the executor's row order).
pub fn sorted_rows(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort();
    rows.dedup();
    rows
}
