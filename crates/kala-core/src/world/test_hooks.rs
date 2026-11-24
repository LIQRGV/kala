//! The hooks of the `test-hooks` feature: ways for tests to break or bypass
//! a world that the supported API never offers. This module is the only
//! place in `world` gated on the feature.

use super::*;
use crate::doctor::{IndexName, Tamper};

impl World {
    /// What a world-scope commit (a rename, cardinality change or migration)
    /// keeps in ONE of its two warning snapshots of this world, as
    /// `(sides, values)`. A side with fewer than two values can form no
    /// overlap pair and must not be kept. Not part of the supported API.
    #[doc(hidden)]
    pub fn world_warning_snapshot_size(&self) -> (usize, usize) {
        let snapshot = txn::WarningScope::World.intervals(self);
        (snapshot.len(), snapshot.values().map(Vec::len).sum())
    }

    /// Makes the next log append fail as `fault` says, to test failure
    /// handling. An in-memory world has no log and ignores it. Not part of
    /// the supported API.
    #[doc(hidden)]
    pub fn inject_log_fault(&mut self, fault: log::LogFault) {
        self.journal.inject_log_fault(fault);
    }

    /// How many bytes of the log file this handle has read since it was
    /// opened (catch-ups and the checks a commit makes), for tests of what a
    /// catch-up costs. Not part of the supported API.
    #[doc(hidden)]
    pub fn log_bytes_read(&self) -> u64 {
        self.journal.bytes_read
    }

    /// Like [`World::at_canon`], but always reading the main indexes with
    /// the canon skip, even at the latest canon: the reference path the
    /// latest-revision indexes must agree with. Not part of the supported
    /// API.
    #[doc(hidden)]
    pub fn at_canon_main(&self, tx: TxId) -> View<'_> {
        View {
            world: self,
            canon: tx.min(self.latest_tx()),
            latest: false,
            work: Work::NONE,
        }
    }

    /// Replaces the content hash of this world's value references with `f`
    /// and rebuilds its indexes and value table from its kept entries, as a
    /// replay with that hash would: a test hook, so a forced-collision world
    /// (`|_| 0`) can be checked to answer exactly like a normal one.
    /// [`crate::doctor`] rebuilds this world with `f` too. Only this World
    /// object changes: nothing is logged, and a reopen uses the real hash.
    /// Not part of the supported API.
    #[doc(hidden)]
    pub fn set_content_hash(&mut self, f: fn(&[u8]) -> u64) {
        let rebuilt = World::replay_with_hash(&self.journal.log_entries(), f)
            .expect("invariant: a world's kept entries replay cleanly");
        // The rebuild holds the same schema, IDs and entries; what is kept
        // from this World is what isn't derived from the log: the log file
        // and the write gate.
        let reader = self.journal.reader.clone();
        let log = self.journal.detach_log();
        let gate = std::mem::take(&mut self.gate);
        *self = rebuilt;
        match log {
            Some(log) => self.journal.attach_log(log),
            None => self.journal.set_reader(reader),
        }
        self.gate = gate;
    }

    /// Damages one stored index in memory, for doctor's tests: the
    /// indexes are copies of the log, and this is how a copy goes wrong
    /// without the log changing. Only that index changes (nothing else is
    /// kept in step), and nothing is logged. A row number past the end does
    /// nothing. Not part of the supported API.
    #[doc(hidden)]
    pub fn tamper_index(&mut self, t: Tamper) {
        self.indexes.tamper(t);
    }

    /// Test hook: alter derived metadata without writing the source log.
    #[doc(hidden)]
    pub fn tamper_schema(&mut self) {
        self.catalog
            .settings
            .push((self.latest_tx(), "schema".into(), "strict".into()));
    }
}

impl Journal {
    /// Takes the log file out, leaving an in-memory journal.
    pub(super) fn detach_log(&mut self) -> Option<LogWriter> {
        self.log.take()
    }

    /// Arms a fault for the next log append; ignored in memory.
    pub(super) fn inject_log_fault(&mut self, fault: log::LogFault) {
        if let Some(log) = self.log.as_mut() {
            log.inject_fault(fault);
        }
    }
}

impl Indexes {
    /// Damages one stored index (see [`World::tamper_index`]).
    fn tamper(&mut self, t: Tamper) {
        fn remove_nth<V>(index: &mut OrderedMap<V>, nth: usize) {
            let key = index.keys().nth(nth).map(<[u8]>::to_vec);
            if let Some(k) = key {
                index.remove(&k);
            }
        }
        fn flip_nth<V: Copy>(index: &mut OrderedMap<V>, nth: usize, flip: fn(V) -> V) {
            let Some(k) = index.keys().nth(nth).map(<[u8]>::to_vec) else {
                return;
            };
            if let Some(v) = index.get(&k).copied() {
                index.set(&k, flip(v));
            }
        }
        fn flip_mark(m: Mark) -> Mark {
            match m {
                Mark::Assert => Mark::Retract,
                // A DELETED marker has no op; it becomes a fact.
                Mark::Retract | Mark::Deleted => Mark::Assert,
            }
        }
        fn flip_op(o: Op) -> Op {
            match o {
                Op::Assert => Op::Retract,
                Op::Retract => Op::Assert,
            }
        }
        match t {
            Tamper::InsertRawKey { index, key } => match index {
                IndexName::E => {
                    self.e_index.insert(key, Mark::Assert);
                }
                IndexName::R => {
                    self.r_index.insert(key, Mark::Assert);
                }
                IndexName::ELatest => {
                    self.e_latest.insert(key, Op::Assert);
                }
                IndexName::RLatest => {
                    self.r_latest.insert(key, Op::Assert);
                }
                IndexName::V => {
                    self.v_index.insert(key, Mark::Assert);
                }
                IndexName::VLatest => {
                    self.v_latest.insert(key, Op::Assert);
                }
                IndexName::VFold => {
                    self.vfold_index.insert(key, Mark::Assert);
                }
                IndexName::VFoldLatest => {
                    self.vfold_latest.insert(key, Op::Assert);
                }
            },
            Tamper::RemoveRow { index, nth } => match index {
                IndexName::E => remove_nth(&mut self.e_index, nth),
                IndexName::R => remove_nth(&mut self.r_index, nth),
                IndexName::ELatest => remove_nth(&mut self.e_latest, nth),
                IndexName::RLatest => remove_nth(&mut self.r_latest, nth),
                IndexName::V => remove_nth(&mut self.v_index, nth),
                IndexName::VLatest => remove_nth(&mut self.v_latest, nth),
                IndexName::VFold => remove_nth(&mut self.vfold_index, nth),
                IndexName::VFoldLatest => remove_nth(&mut self.vfold_latest, nth),
            },
            Tamper::FlipOp { index, nth } => match index {
                IndexName::E => flip_nth(&mut self.e_index, nth, flip_mark),
                IndexName::R => flip_nth(&mut self.r_index, nth, flip_mark),
                IndexName::ELatest => flip_nth(&mut self.e_latest, nth, flip_op),
                IndexName::RLatest => flip_nth(&mut self.r_latest, nth, flip_op),
                IndexName::V => flip_nth(&mut self.v_index, nth, flip_mark),
                IndexName::VLatest => flip_nth(&mut self.v_latest, nth, flip_op),
                IndexName::VFold => flip_nth(&mut self.vfold_index, nth, flip_mark),
                IndexName::VFoldLatest => flip_nth(&mut self.vfold_latest, nth, flip_op),
            },
        }
    }
}
