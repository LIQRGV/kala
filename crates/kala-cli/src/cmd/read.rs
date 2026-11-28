//! What every read command shares: the `@T` / `--at` tick, opening the
//! world read-only at a canon, the default tick, and entity arguments.

use std::path::Path;

use kala_core::{EntityId, Tick, TxId, Value, ValueType, View, World};

use crate::cli::Canon;
use crate::error::CliError;
use crate::escape;
use crate::out::Labeler;

/// Parses a positional `@T` (T a signed 64-bit integer); a usage error
/// otherwise.
pub fn parse_at(s: &str) -> Result<Tick, CliError> {
    s.strip_prefix('@')
        .and_then(|d| d.parse::<i64>().ok())
        .map(Tick)
        .ok_or_else(|| {
            CliError::Usage(format!(
                "invalid tick `{}`: expected `@T`, T a signed 64-bit integer",
                escape::value(s)
            ))
        })
}

/// The tick the command line gives, from a positional `@T` or `--at T`;
/// giving both is a usage error. `None`: use the latest tick.
pub fn tick_arg(positional: Option<&str>, at: Option<i64>) -> Result<Option<Tick>, CliError> {
    match (positional, at) {
        (Some(_), Some(_)) => Err(CliError::Usage(
            "the tick is given twice: use either `@T` or `--at T`".to_string(),
        )),
        (Some(p), None) => parse_at(p).map(Some),
        (None, at) => Ok(at.map(Tick)),
    }
}

/// A kala-core read error, as a CLI error.
pub fn core_err(e: kala_core::Error) -> CliError {
    CliError::core(&e, e.to_string())
}

/// Like [`core_err`], but an unknown entity names the argument as typed:
/// kala-core's message shows only the raw ID.
pub fn entity_err(arg: &str) -> impl Fn(kala_core::Error) -> CliError + '_ {
    move |e| match e {
        kala_core::Error::UnknownEntity(_) => {
            CliError::core(&e, format!("{}: {e}", escape::value(arg)))
        }
        e => core_err(e),
    }
}

/// Opens a world file for reading with [`World::open_read_only`]: no lock
/// (it works while another process has the world open), and the file is
/// never changed (a torn or damaged tail is left in it and only left out of
/// the answers). Missing: a user error; corrupt, not a Kala log, or
/// unreadable: a store error.
pub fn open_world(path: &Path) -> Result<World, CliError> {
    World::open_read_only(path).map_err(|e| CliError::at_path(path, &e))
}

/// An open world and the canon a read command reads at.
pub struct Ctx {
    pub world: World,
    /// The canon used: the one asked for, capped at the latest tx.
    pub canon: TxId,
}

impl Ctx {
    /// Opens the world file read-only (see [`open_world`]) and resolves
    /// `--canon` (default: the latest).
    pub fn open(path: &Path, canon: Option<Canon>) -> Result<Ctx, CliError> {
        let world = open_world(path)?;
        let asked = canon.map_or_else(|| world.latest_tx(), |c| TxId(c.0));
        let canon = world.at_canon(asked).canon();
        Ok(Ctx { world, canon })
    }

    pub fn view(&self) -> View<'_> {
        self.world.at_canon(self.canon)
    }

    /// The tick given, else the latest tick at the canon. Call it once per
    /// command: at an older canon the latest tick is a walk over the world.
    pub fn tick(&self, given: Option<Tick>) -> Tick {
        given.unwrap_or_else(|| self.view().latest_tick())
    }

    pub fn labeler(&self, tick: Tick) -> Labeler<'_> {
        Labeler::new(&self.world, self.canon, tick)
    }

    /// An entity argument: an ID (`CHARACTER#1`, `edge#9`) if it parses as
    /// one (whether it is defined is left to the read, which reports an
    /// unknown entity), else a handle.
    pub fn entity(&self, arg: &str) -> Result<EntityId, CliError> {
        if let Some(id) = self.world.parse_id(arg) {
            return Ok(id);
        }
        self.world.entity_by_handle(arg).ok_or_else(|| {
            CliError::User(format!(
                "unknown entity `{}`: not a handle or an ID of this world",
                escape::value(arg)
            ))
        })
    }
}

/// A value argument read as type `ty`, a ref by entity syntax
/// ([`Ctx::entity`]). `whose` names what has that type, already escaped
/// (``attribute `name` ``); an error reads "`s` is not an i64 (the type of
/// `<whose>`)".
pub fn parse_value(ctx: &Ctx, ty: ValueType, s: &str, whose: &str) -> Result<Value, CliError> {
    let bad = |what: &str| {
        CliError::User(format!(
            "`{}` is not {what} (the type of {whose})",
            escape::value(s)
        ))
    };
    Ok(match ty {
        ValueType::Bool => match s {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err(bad("a bool (`true` or `false`)")),
        },
        ValueType::I64 => Value::I64(s.parse().map_err(|_| bad("an i64"))?),
        ValueType::F64 => {
            let x: f64 = s.parse().map_err(|_| bad("an f64"))?;
            Value::f64(x).map_err(|_| bad("an f64 (NaN is never stored)"))?
        }
        ValueType::Str => Value::Str(s.to_string()),
        ValueType::Ref => Value::Ref(ctx.entity(s)?),
    })
}
