//! Bounded transaction options and session-owned snapshot state.
use crate::{
    error::{Failure, INVALID},
    message::Map,
    packstream::{self, Value},
    registry::WriteSlot,
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
                let bad = || {
                    Failure::new(
                        write::BOOKMARK,
                        "invalid bookmark or bookmark for another database",
                    )
                };
                let Value::String(s) = item else {
                    return Err(bad());
                };
                if s.len() > 256 {
                    return Err(bad());
                }
                let parts: Vec<_> = s.split(':').collect();
                if parts.len() != 3
                    || parts[0] != "kala"
                    || parts[1] != db
                    || parts[2].is_empty()
                    || !parts[2].bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(bad());
                }
                bookmark = bookmark.max(parts[2].parse::<u64>().map_err(|_| bad())?);
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
pub(crate) enum Snapshot {
    Read(Arc<World>),
    Write(Box<OwnedTxn>, WriteSlot),
}
pub(crate) struct Transaction {
    pub snapshot: Snapshot,
    pub db: String,
    pub read_only: bool,
    pub forms: Vec<StagedWrite>,
    pub bytes: usize,
    pub queried: bool,
    pub server_statement: bool,
    pub deadline: Instant,
    pub idle_deadline: Instant,
}
impl Transaction {
    pub fn world(&self) -> &World {
        match &self.snapshot {
            Snapshot::Read(w) => w,
            Snapshot::Write(tx, _) => tx.world(),
        }
    }
    pub fn write(&mut self) -> Result<&mut OwnedTxn, Failure> {
        match &mut self.snapshot {
            Snapshot::Write(tx, _) => Ok(tx),
            _ => Err(Failure::new(
                crate::error::FORBIDDEN,
                "write privilege is required",
            )),
        }
    }
    pub fn expires(&self) -> Instant {
        self.deadline.min(self.idle_deadline)
    }
}
