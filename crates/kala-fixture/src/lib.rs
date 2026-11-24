//! The YAML fixture format and its loader.
//!
//! A fixture is a small, operation-oriented YAML description of a world: its
//! schema, its entities, and the write primitives that build its history, in
//! order. [`load_fixture`] builds an in-memory [`World`] from one;
//! [`load_fixture_to`] builds a new log file. The loader only calls the
//! native API ([`World::begin`], [`Txn::create_entity`](kala_core::Txn::create_entity),
//! [`Txn::assert_with_precision`](kala_core::Txn::assert_with_precision), [`Txn::retract_with_precision`](kala_core::Txn::retract_with_precision),
//! [`Txn::correct`](kala_core::Txn::correct), [`Txn::commit`](kala_core::Txn::commit)), so a loaded world is exactly the world
//! the same calls would build by hand: same entry IDs, transactions, ticks and
//! precisions, byte for byte.
//!
//! # Format
//!
//! Every top-level key is optional; an empty document loads an empty world (no
//! node types, no attributes). Unknown keys anywhere are an error.
//!
//! ```yaml
//! schema:
//!   node_types: [CHARACTER, PLACE, ITEM, EVENT]
//!   attributes:
//!     - { name: name,       type: string }                         # defaults: kind state, cardinality one, indexed true
//!     - { name: alias,      type: string, cardinality: many }
//!     - { name: biography,  type: string, indexed: false }
//!     - { name: located_in, type: ref }                            # ref default direction: directed
//!     - { name: ally_of,    type: ref, cardinality: many, direction: bidirectional }
//!     - { name: parent_of,  type: ref, cardinality: many, inverse: child_of }
//!     - { name: ruled_by,   type: ref, reverse: one }              # reverse-side cardinality
//!     - { name: bought,     type: ref, kind: event, cardinality: many }
//!     - { name: disguised,  type: bool }
//! entities:                     # created in one transaction before everything else,
//!   CHARACTER: [edwin, greg, mara]   # per type in list order (types in schema order)
//!   PLACE: [varrow, ashford]
//! transactions:
//!   - tick: -300                # default tick for ops in this transaction without one
//!     ops:
//!       - assert: [greg, located_in, varrow]
//!       - assert: [greg, name, "Greg"]
//!       - assert: [edwin, ally_of, mara]
//!         tick: -208
//!         precision: year
//!         label: edwin-mara
//!   - ops:
//!       - create: [CHARACTER, lisa]                    # an entity introduced later
//!       - retract: [greg, located_in, varrow]
//!         tick: -250
//!         label: greg-leaves
//!       - assert: [[edwin, present, carnath], disguised, true]   # edge subject: [source, attr, target]
//!   - ops:
//!       - correct: greg-leaves
//!         replace: { tick: -280 }                      # value / tick / op / precision, any subset
//!       - correct: edwin-mara
//!         delete: true
//! ```
//!
//! ## `schema`
//!
//! - `node_types`: a list of node type names, in ID order.
//! - `attributes`: a list of attribute declarations, in ID order. `name` and
//!   `type` are required; `type` is `bool | i64 | f64 | string | ref`.
//!   Optional: `kind: state | event` (default `state`), `cardinality: one |
//!   many` (default `one`), `indexed: true | false` (default `true`), and for
//!   `ref` attributes `direction: directed | bidirectional` (default
//!   `directed`), `inverse: <name>` and `reverse: one | many` (reverse-side
//!   cardinality). The store checks the declaration as [`World::in_memory`]
//!   does (duplicate names, a direction on a non-ref attribute...).
//!
//! ## `entities`
//!
//! A mapping from node type to a list of handles. All of them are created in
//! one transaction before any other, per type in list order, types in schema
//! order (whatever their order in the YAML). With no entities, nothing is
//! committed for them.
//!
//! ## `transactions` (or the bare `operations` form)
//!
//! `transactions` is a list of `{ tick?, ops }`; each becomes one native
//! transaction. `tick` is the default tick of its ops that don't give one.
//! The bare form is a top-level `operations:` list of ops instead, each its
//! own transaction (and, in [`load_fixture_to`], its own fsync). A fixture
//! has at most one of the two.
//!
//! An op is exactly one of:
//!
//! - `assert: [subject, attr, value]` / `retract: [subject, attr, value]`,
//!   with optional `tick` (an i64), `precision` (`exact | day | month | year
//!   | decade | century`, default `exact`) and `label`. A missing tick is the
//!   transaction's `tick`, else the world's latest tick when the transaction
//!   started (the CLAUDE.md rule: every tick-less write in one transaction
//!   lands on the same tick).
//! - `create: [NODE_TYPE, handle]`: a new entity, usable by the later ops of
//!   its transaction and every later transaction.
//! - `correct: <label>` with either `delete: true` or `replace: { value?,
//!   tick?, op?, precision? }` (`op` is `assert | retract`). A label may be
//!   corrected several times; each correction applies to the chain's current
//!   revision.
//!
//! Entities are always named by handle. A subject is a handle or an edge
//! triple `[source, attr, target]` naming an edge that already exists (the
//! loader never interns one implicitly). A value of a `ref` attribute is a
//! handle or an edge triple; any other value is a YAML scalar of the
//! attribute's type: a string for `string` (`007`, `true` and `null` are not
//! strings: quote them), an integer in i64 range for `i64`, a number for
//! `f64` (an integer only if exactly representable, `|n| <= 2^53`), `true` /
//! `false` for `bool` (`yes`, `no`, `on`, `off` are not booleans).
//!
//! A `label` (assert / retract only) is unique in the fixture and names the
//! entry its op stored. It can be corrected from the next transaction on
//! (entry IDs exist only after the commit); correcting a label whose op stored
//! nothing (a "0 changed" no-op) is an error.
//!
//! ## Errors and limits
//!
//! A [`FixtureError`] says where ([`OpRef`], 0-based; `None` for the schema
//! and entities sections) and what ([`FixtureErrorKind`]). Everything the YAML
//! parser rejects is [`FixtureErrorKind::Yaml`]: syntax, duplicate keys, merge
//! keys (`<<`), unsupported tags, several documents, and the budgets (input
//! over [`MAX_FIXTURE_BYTES`], nesting deeper than 16, more than 1,000 aliases
//! or anchors, alias expansion). Shape errors after parsing are
//! [`FixtureErrorKind::Invalid`], including the loader's own limits: at most
//! 1,000,000 ops, 100,000 ops per transaction, 1,000,000 entities, 65,536 node
//! types and 65,536 attributes, and handles, labels and node type names of at
//! most 256 bytes. The store's refusals are [`FixtureErrorKind::Store`].
//!
//! Input checks that need no world (size, YAML, shape) run before a world or
//! file exists. If [`load_fixture_to`] fails later, the file holds the
//! transactions committed so far; the caller removes it.
//!
//! The loader never panics, whatever the input.
//!
//! ```
//! use kala_core::{Direction, Tick};
//! use kala_fixture::load_fixture;
//!
//! let world = load_fixture(
//!     r#"
//! schema:
//!   node_types: [CHARACTER]
//!   attributes:
//!     - { name: ally_of, type: ref, cardinality: many, direction: bidirectional }
//! entities:
//!   CHARACTER: [edwin, mara]
//! operations:
//!   - assert: [edwin, ally_of, mara]
//!     tick: -208
//! "#,
//! )
//! .unwrap();
//! let edwin = world.entity_by_handle("edwin").unwrap();
//! let allies = world.neighbors(edwin, "ally_of", Direction::Out, Tick(-200)).unwrap();
//! assert_eq!(allies.len(), 1);
//! ```

#![forbid(unsafe_code)]

mod apply;
mod error;
mod plan;
mod yaml;

use std::path::Path;

use kala_core::World;

use apply::run;
use error::store;
use plan::plan;

pub use error::{FixtureError, FixtureErrorKind, OpRef};

/// The largest fixture the loader reads, in bytes (checked before parsing).
pub const MAX_FIXTURE_BYTES: usize = 16 * 1024 * 1024;

/// Loads a fixture into a new in-memory world. See the module docs for the
/// format. Never panics; a fixture that fails anywhere returns no world.
pub fn load_fixture(yaml: &str) -> Result<World, FixtureError> {
    let plan = plan(yaml)?;
    let mut world = World::in_memory(plan.schema.clone()).map_err(|e| store(None, e))?;
    run(&plan, &mut world)?;
    Ok(world)
}

/// Loads a fixture into a new log file at `path` (same rules as
/// [`World::create`]: an existing path is refused and left alone). The size,
/// YAML and shape checks run before the file is created; a later failure
/// leaves the file holding the transactions committed so far. The bare
/// `operations` form commits, and so fsyncs, once per op.
pub fn load_fixture_to(yaml: &str, path: &Path) -> Result<World, FixtureError> {
    let plan = plan(yaml)?;
    let mut world = World::create(path, plan.schema.clone()).map_err(|e| store(None, e))?;
    run(&plan, &mut world)?;
    Ok(world)
}
