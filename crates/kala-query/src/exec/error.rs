//! Why a query couldn't run.

use std::fmt;

use kala_core::Error as CoreError;

/// Why a query couldn't run.
#[derive(Debug)]
pub enum QueryError {
    /// The caller cancelled this run; no partial answer is returned.
    Cancelled,
    /// A parameter the plan uses or declares has no value.
    MissingParam(String),
    /// A parameter has the wrong type for where it is used.
    ParamType {
        /// The parameter's name.
        name: String,
        /// What was expected (`i64`, `ref`, `non-negative i64`, ...).
        expected: String,
    },
    /// An operator reads at a tick variable that nothing before it binds.
    TickNotBound(String),
    /// A constant or a bound value has the wrong type for where it is used.
    TypeMismatch(String),
    /// The plan is malformed: an infeasible access, an unbound variable, a
    /// union of inputs binding different variables, ...
    Plan(String),
    /// An operator produced more than `limit` rows.
    RowLimit {
        /// The row limit that was exceeded.
        limit: usize,
    },
    /// The query took more than `limit` steps.
    StepBudget {
        /// The step budget that was exceeded.
        limit: u64,
    },
    /// The query needed more than `limit` bytes at once.
    MemoryBudget {
        /// The byte budget that was exceeded.
        limit: usize,
    },
    /// Arithmetic left its type's range: an i64 `sum` aggregate that
    /// doesn't fit in an i64.
    Overflow(String),
    /// A kala-core read failed (unknown attribute, unknown node type, ...).
    Core(CoreError),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::Cancelled => write!(f, "cancelled"),
            QueryError::MissingParam(n) => write!(f, "missing parameter ${n}"),
            QueryError::ParamType { name, expected } => {
                write!(f, "parameter ${name} must be {expected}")
            }
            QueryError::TickNotBound(v) => {
                write!(f, "tick variable ?{v} is not bound where it is used")
            }
            QueryError::TypeMismatch(m) => write!(f, "type mismatch: {m}"),
            QueryError::Plan(m) => write!(f, "invalid plan: {m}"),
            QueryError::RowLimit { limit } => {
                write!(f, "query produced more than {limit} rows")
            }
            QueryError::StepBudget { limit } => {
                write!(f, "query took more than {limit} steps")
            }
            QueryError::MemoryBudget { limit } => {
                write!(f, "query needed more than {limit} bytes of memory")
            }
            QueryError::Overflow(m) => write!(f, "arithmetic overflow: {m}"),
            QueryError::Core(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for QueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            QueryError::Core(e) => Some(e),
            _ => None,
        }
    }
}

impl From<CoreError> for QueryError {
    fn from(e: CoreError) -> QueryError {
        QueryError::Core(e)
    }
}
