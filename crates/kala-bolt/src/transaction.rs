//! Bounded transaction options and session-owned snapshot state.
use crate::{
    error::{Failure, INVALID},
    message::Map,
    packstream::{self, Value},
    reads::Reads,
    registry::{Registry, WriteSlot},
    server::ServerConfig,
    write,
};
use kala_core::{Concurrency, OwnedTxn, World};
use kala_lang::write::StagedWrite;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) struct Options {
    pub read_only: bool,
    pub concurrency: Concurrency,
    pub timeout: Duration,
    pub bookmark: u64,
}
impl Options {
    pub fn parse(extra: &Map, db: &str, config: &ServerConfig) -> Result<Self, Failure> {
        let read_only = match extra.get("mode") {
            None => false,
            Some(Value::String(m)) if m == "r" => true,
            Some(Value::String(m)) if m == "w" => false,
            _ => return Err(Failure::invalid()),
        };
        let mut concurrency = Concurrency::default();
        if let Some(metadata) = extra.get("tx_metadata") {
            let Value::Map(metadata) = metadata else {
                return Err(Failure::invalid());
            };
            packstream::encode_with_limits(
                &Value::Map(metadata.clone()),
                16 * 1024,
                config.max_depth,
            )
            .map_err(|_| Failure::new(INVALID, "tx_metadata exceeds the 16 KiB limit"))?;
            if let Some(mode) = metadata.get("concurrency_mode") {
                concurrency = match mode {
                    Value::String(s) if s == "weak" => Concurrency::Weak,
                    Value::String(s) if s == "optimistic" => Concurrency::Optimistic,
                    _ => {
                        return Err(Failure::new(
                            INVALID,
                            "concurrency_mode must be weak or optimistic",
                        ));
                    }
                };
            }
        }
        let timeout = match extra.get("tx_timeout") {
            None | Some(Value::Integer(0)) => config.tx_timeout,
            Some(Value::Integer(ms)) if *ms > 0 => {
                config.tx_timeout.min(Duration::from_millis(*ms as u64))
            }
            _ => return Err(Failure::invalid()),
        };
        let mut bookmark = 0;
        if let Some(bookmarks) = extra.get("bookmarks") {
            let Value::List(bookmarks) = bookmarks else {
                return Err(Failure::new(
                    write::BOOKMARK,
                    "bookmarks must be a list of strings",
                ));
            };
            if bookmarks.len() > 64 {
                return Err(Failure::new(
                    write::BOOKMARK,
                    "at most 64 bookmarks are allowed",
                ));
            }
            for item in bookmarks {
                let Value::String(s) = item else {
                    return Err(Failure::new(write::BOOKMARK, "invalid bookmark"));
                };
                // A driver's bookmark manager sends every bookmark it holds,
                // whatever database the request targets: one that names
                // another database is skipped, only a malformed one refused.
                if let Some(tx) = bookmark_tx(s, db)? {
                    bookmark = bookmark.max(tx);
                }
            }
        }
        Ok(Self {
            read_only,
            concurrency,
            timeout,
            bookmark,
        })
    }
}
/// The tx a `kala:<db>:<tx>` bookmark names, or `None` when it is a
/// well-formed bookmark of another database (which says nothing about `db`).
fn bookmark_tx(bookmark: &str, db: &str) -> Result<Option<u64>, Failure> {
    let bad = || Failure::new(write::BOOKMARK, "invalid bookmark");
    if bookmark.len() > 256 {
        return Err(bad());
    }
    let (name, tx) = bookmark
        .strip_prefix("kala:")
        .and_then(|rest| rest.rsplit_once(':'))
        .ok_or_else(bad)?;
    if !kala_system::valid_database_name(name)
        || tx.is_empty()
        || !tx.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(bad());
    }
    let tx = tx.parse::<u64>().map_err(|_| bad())?;
    Ok((name == db).then_some(tx))
}
/// What an explicit transaction reads and, once it writes, stages into.
pub(crate) enum Snapshot {
    /// The registry's shared read snapshot, pinned when the transaction
    /// began. Every transaction starts here, whatever its mode, and one
    /// that never writes stays here: it costs no replay and no slot.
    Read(Arc<World>),
    /// A private writable handle and the transaction staged on it, opened
    /// at the transaction's first write form, with its slot under the
    /// write-transaction cap.
    Write(Box<OwnedTxn>, WriteSlot),
}
pub(crate) struct Transaction {
    pub snapshot: Snapshot,
    pub db: String,
    pub read_only: bool,
    /// The mode BEGIN asked for; set on the writable handle when it opens.
    pub concurrency: Concurrency,
    pub forms: Vec<StagedWrite>,
    pub bytes: usize,
    pub queried: bool,
    pub server_statement: bool,
    /// What the queries run so far depend on, kept until the writable
    /// handle exists to record it on.
    pub pending: Reads,
    /// Set when the transaction queried the read snapshot pinned at BEGIN
    /// and its writable handle then opened on a newer world (see
    /// [`Transaction::writer`]): the BEGIN snapshot, which its later
    /// queries keep reading so that one transaction never shows two worlds.
    pub begun: Option<Arc<World>>,
    /// The end of the transaction's total time (`--tx-timeout`, or the
    /// client's shorter `tx_timeout`). It bounds all server-side work:
    /// queries, staging, the commit and sending responses.
    pub deadline: Instant,
    /// When the client has been idle for too long: counted from the moment
    /// the last response was sent, and only read while the next request is
    /// awaited. Work in progress is never "idle".
    pub idle_deadline: Instant,
}
impl Transaction {
    /// The committed world this transaction's queries read: one snapshot
    /// for its whole life, never showing its own staged writes.
    pub fn world(&self) -> &World {
        match (&self.begun, &self.snapshot) {
            (Some(begun), _) => begun,
            (None, Snapshot::Read(w)) => w,
            (None, Snapshot::Write(tx, _)) => tx.world(),
        }
    }
    /// The transaction's writable handle, if it has written.
    pub fn written(&mut self) -> Option<&mut OwnedTxn> {
        match &mut self.snapshot {
            Snapshot::Write(tx, _) => Some(tx),
            Snapshot::Read(_) => None,
        }
    }
    /// The writable transaction, opened now if this is the first write.
    ///
    /// Opening takes a slot under the write-transaction cap (for `account`)
    /// and replays the world into a private handle, so it happens here, at
    /// the first write form, and never at BEGIN: drivers begin every
    /// transaction in write mode, and most of them only read.
    ///
    /// **The snapshot rule.** The handle opens on the world as it is now,
    /// which may be newer than the read snapshot pinned at BEGIN (tx
    /// `pinned`).
    ///
    /// - If the transaction has run no query yet, nothing depended on the
    ///   older snapshot and the newer one simply becomes the transaction's.
    /// - If it has, the read keys its queries recorded (`pending`) are
    ///   noted on the handle, and when other transactions committed in
    ///   between, the handle is told that those reads were made at
    ///   `pinned` (`Txn::set_read_version`): COMMIT then checks them
    ///   against everything committed after BEGIN's snapshot, so only a
    ///   commit that wrote something the queries read is a conflict. The
    ///   transaction's later queries keep reading the BEGIN snapshot
    ///   ([`Transaction::begun`]), so it never shows two worlds.
    /// - Two cases fall back to "read everything as of BEGIN" (any commit
    ///   since then conflicts): the pending set overflowed its bound
    ///   (`Reads::merge`), or the schema changed in between (the read keys
    ///   are names, and a name may mean another attribute by now).
    ///
    /// Weak mode records and checks none of it.
    ///
    /// **The latest tick** is the BEGIN snapshot's, whatever the handle
    /// opened on (`Txn::set_start_tick`): tick-less writes land on it, and
    /// the commit warns if the world's latest tick is another one by then.
    ///
    /// `admin` is whether the identity administers the database: without
    /// it the transaction may add to the schema but not change what the
    /// committed schema has (`Txn::set_schema_additions_only`).
    pub fn writer(
        &mut self,
        registry: &Registry,
        config: &ServerConfig,
        account: &str,
        admin: bool,
    ) -> Result<&mut OwnedTxn, Failure> {
        if let Snapshot::Read(world) = &self.snapshot {
            let begun = Arc::clone(world);
            let pinned = begun.latest_tx();
            let slot = registry.write_slot(config.max_write_transactions, account)?;
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            let mut owned = registry
                .open_writer(&self.db, config.busy_timeout.min(remaining))?
                .into_transaction();
            if owned.snapshot_tx() < pinned {
                // The file no longer holds what the read snapshot showed.
                return Err(Failure::new(
                    write::CONFLICT,
                    "the world changed; run the transaction again",
                ));
            }
            let moved = owned.snapshot_tx() != pinned;
            // The pending keys are names: exact only if every name means
            // on the handle what it meant on the BEGIN snapshot.
            let same_schema = !moved || {
                let now = owned.world();
                begun.attributes().eq(now.attributes()) && begun.node_types().eq(now.node_types())
            };
            owned.with(|tx| {
                tx.set_concurrency(self.concurrency);
                tx.set_deadline(self.deadline);
                tx.set_schema_additions_only(!admin);
                tx.set_start_tick(begun.latest_tick());
                if self.queried {
                    if same_schema {
                        self.pending.note(tx);
                    } else {
                        tx.note_read_all();
                    }
                    if moved {
                        tx.set_read_version(pinned);
                    }
                }
            });
            self.begun = (self.queried && moved).then_some(begun);
            self.pending = Reads::default();
            self.snapshot = Snapshot::Write(Box::new(owned), slot);
        }
        self.written().ok_or_else(Failure::invalid)
    }
    /// When to give up waiting for the client's next request.
    pub fn idle_expires(&self) -> Instant {
        self.deadline.min(self.idle_deadline)
    }
}
