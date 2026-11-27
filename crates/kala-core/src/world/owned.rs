//! [`OwnedTxn`]: a write transaction that owns its world handle, so it can
//! be kept in a struct and borrowed again for each call.

use super::*;

/// A write transaction that owns the [`World`] handle it works on, started
/// with [`World::into_transaction`].
///
/// A [`Txn`] borrows its world mutably, so it can't outlive one call stack.
/// A caller that must keep a transaction open across separate calls (a
/// server session between two messages) keeps an `OwnedTxn` instead and
/// borrows the transaction for each call with [`OwnedTxn::with`]. Every
/// borrow is the **same** transaction: what earlier borrows staged, the
/// operations and reads they recorded, the entities and edges they
/// allocated, their schema edits, the transaction's settings
/// ([`Txn::set_concurrency`], [`Txn::set_ids_exposed`],
/// [`Txn::set_default_tick`]) and whether a failed write poisoned it. It is
/// the code and the state of [`Txn`], not a second implementation: commit
/// one and the log gets the bytes an ordinary `Txn` making the same
/// operations writes.
///
/// # The snapshot
///
/// The transaction works on the world as it was when it began (after
/// [`World::into_transaction`] caught up). The handle is never caught up
/// while the transaction is open, so [`OwnedTxn::world`] keeps answering
/// from that snapshot whatever other handles commit meanwhile; the commit
/// deals with them as [`Txn::commit`] says ("Committing over newer
/// transactions").
///
/// # What a reader of [`OwnedTxn::world`] sees
///
/// The **committed** world at the snapshot, and nothing of the open
/// transaction: not its staged facts, entities or edges (those are only in
/// the transaction), and not its schema edits. A `Txn` applies its schema
/// edits provisionally to its world, so that its own later operations
/// resolve the new names; between borrows the owned form takes them back
/// out and the world has its committed schema again (they are put back for
/// the next borrow). A query run through `world()` therefore can't see a
/// name the transaction has only staged, or miss one it has renamed away.
///
/// # Ending it
///
/// [`OwnedTxn::commit`] and [`OwnedTxn::rollback`] give the world handle
/// back, also when the commit fails. Dropping an `OwnedTxn` discards the
/// transaction (and drops the handle) exactly as dropping a `Txn` does:
/// nothing is written, and nothing is left locked, because an open
/// transaction holds no lock at all (the writer lock is taken by the commit,
/// for its own length).
#[derive(Debug)]
pub struct OwnedTxn {
    world: World,
    /// The transaction's state between borrows. During a borrow the [`Txn`]
    /// has it and this is an aborted stand-in, so a borrow that panicked
    /// leaves the transaction aborted rather than silently empty.
    state: TxnState,
    /// The node types the transaction declared, set aside between borrows
    /// with the rest of its provisional schema (which is kept in
    /// `state.schema_before`'s place, see [`SchemaSnapshot::park`]).
    parked_types: Vec<NodeType>,
}

impl World {
    /// Starts a write transaction that takes this handle with it: see
    /// [`OwnedTxn`]. Everything [`World::begin`] says holds: the handle
    /// first applies what other handles committed (no lock, no error
    /// reported; the commit checks again under the writer lock), and the
    /// latest tick is captured now.
    pub fn into_transaction(mut self) -> OwnedTxn {
        self.catch_up_if_grown();
        let state = self.new_txn_state();
        OwnedTxn {
            world: self,
            state,
            parked_types: Vec::new(),
        }
    }
}

impl OwnedTxn {
    /// Borrows the transaction for one call: `f` gets the [`Txn`], with
    /// everything earlier borrows staged, recorded and set, and whatever it
    /// does is kept for the next borrow and for the commit.
    ///
    /// If `f` panics, the transaction is aborted: its schema edits are
    /// taken back, every later operation and the commit fail with
    /// [`Error::TransactionAborted`], and nothing is ever written.
    pub fn with<R>(&mut self, f: impl FnOnce(&mut Txn<'_>) -> R) -> R {
        let mut aborted = self.world.new_txn_state();
        aborted.poisoned = true;
        let mut txn = resume(
            &mut self.world,
            &mut self.state,
            &mut self.parked_types,
            aborted,
        );
        let out = f(&mut txn);
        // Take the state back; the `Txn` is left with the stand-in, which
        // has no schema edits for its drop to restore.
        std::mem::swap(&mut txn.s, &mut self.state);
        drop(txn);
        if let Some(schema) = &mut self.state.schema_before {
            schema.park(&mut self.world.catalog, &mut self.parked_types);
        }
        out
    }

    /// The world this transaction began on: the committed state at its
    /// snapshot, for reading while the transaction is open. It shows
    /// nothing the transaction staged and never moves while the
    /// transaction is open (see [`OwnedTxn`], "What a reader sees").
    pub fn world(&self) -> &World {
        &self.world
    }

    /// The latest committed tx of the world this transaction began on (see
    /// [`Txn::snapshot_tx`]).
    pub fn snapshot_tx(&self) -> TxId {
        TxId(self.state.tx.0 - 1)
    }

    /// Commits the transaction as [`Txn::commit`] does and gives the handle
    /// back with the result.
    ///
    /// The handle comes back on every path. After a successful commit it
    /// has the transaction applied (and whatever other handles committed
    /// before it). After a failed one nothing of the transaction is in it
    /// or in the file: after [`Error::Conflict`] or a store-rule error
    /// found while committing over newer transactions, the handle has
    /// caught up with those transactions, so the next transaction on it
    /// starts from them; after [`Error::WorldLocked`] it is as it was;
    /// after [`Error::LogDiverged`], [`Error::Corrupt`] or
    /// [`Error::RecoveryRequired`] it refuses writes as [`World::catch_up`]
    /// and [`Txn::commit`] describe (reopen it, or acknowledge the
    /// recovery).
    pub fn commit(mut self) -> (World, Result<CommitResult, Error>) {
        let stand_in = self.world.new_txn_state();
        let result = resume(
            &mut self.world,
            &mut self.state,
            &mut self.parked_types,
            stand_in,
        )
        .commit();
        (self.world, result)
    }

    /// Discards the transaction and gives the handle back: nothing was
    /// written, and every ID, handle and name the transaction took is free
    /// again. The handle then applies what other handles committed while
    /// the transaction was open, as [`World::begin`] does (no lock, no
    /// error reported).
    pub fn rollback(self) -> World {
        // Between borrows the world has its committed schema and holds
        // nothing of the transaction: dropping the state is the rollback.
        let mut world = self.world;
        world.catch_up_if_grown();
        world
    }
}

/// The [`Txn`] of the transaction whose state is `state`, on its world, with
/// its provisional schema applied again. `state` is left holding `stand_in`
/// until the caller puts the transaction's state back.
fn resume<'w>(
    world: &'w mut World,
    state: &mut TxnState,
    parked_types: &mut Vec<NodeType>,
    stand_in: TxnState,
) -> Txn<'w> {
    let mut s = std::mem::replace(state, stand_in);
    if let Some(schema) = &mut s.schema_before {
        schema.resume(&mut world.catalog, parked_types);
    }
    Txn { world, s }
}
