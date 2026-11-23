//! Kala's language-neutral query layer: the shared IR every query language
//! compiles to, the plan printer, the executor and its limits.
//!
//! Front-ends (Datalog and Cypher, later Gremlin) emit IR; this
//! crate never knows which language a plan came from. It reads the world only
//! through the public kala-core API (`View` reads at a pinned canon), never the
//! storage layer. Design: docs/slice-6-decisions.md.

#![forbid(unsafe_code)]

pub mod datum;
pub mod exec;
pub mod ir;

pub use datum::Datum;
pub use exec::{
    Limits, MAX_PLAN_DEPTH, Params, QueryError, ResultSet, Usage, execute, run, run_with_cancel,
    run_with_usage, run_with_values, run_with_values_and_cancel,
};
pub use ir::{
    Access, Agg, AggFn, AttrRef, CanonSource, CmpOp, Func, MAX_EXPLAIN_BYTES, Op, OpTerm, Plan,
    Predicate, RelDef, Term, TickSource, Var,
};

mod prepared;
pub use prepared::{ParamKind, Prepared};

pub mod rows;
pub use rows::{Expr, Orientation, OutputSpec, RowAction};

mod params;
pub use params::{ParameterInput, ParameterValue, QueryParams};
