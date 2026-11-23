//! Doctor's scan of a log: the same per-transaction judgement as
//! [`scan`](super::scan::scan), but collecting findings instead of failing
//! at the first one.

use super::scan::{Begin, BeginStep, BodyFault, BodyStep, begin_step, body_step, check_header};
use super::{EntryId, LogEntry};
use crate::doctor::Finding;
use crate::error::Error;
use crate::world::TxId;

/// One transaction [`survey`] found whole and parseable, for doctor to
/// replay: its tx (from its begin record) and its entries, `Commit` last.
#[derive(Debug)]
pub(crate) struct SurveyedTxn {
    pub(crate) tx: TxId,
    pub(crate) entries: Vec<LogEntry>,
}

/// A finding of [`survey`], tagged with the tx it belongs to (doctor's
/// `since` filter).
pub(crate) type TaggedFinding = (Option<TxId>, Finding);

/// Doctor's scan of a whole log file's bytes: the same judgement as
/// [`scan`](super::scan::scan), but collecting instead of failing fast.
/// Never modifies anything.
///
/// Fails only where nothing can be scanned: [`Error::BadMagic`],
/// [`Error::UnsupportedVersion`] or [`Error::Corrupt`] for a truncated
/// header. Otherwise every problem is a finding, and the scan goes on past
/// a bad transaction whenever its begin record is sound (its `body_len` is
/// trusted to skip it): a body checksum mismatch with a valid begin record
/// after it, a `Commit` checksum mismatch, a missing `Commit` or any other
/// invalid body is reported and that transaction skipped; a tx out of
/// sequence (a [`Finding::TxGap`]) or entry IDs out of sequence (an
/// [`Finding::EntryIdGap`]) are reported and the transaction kept, unless it
/// would rewind (a repeated tx or entry ID), when it is skipped too. After a
/// skipped transaction the next one's first entry ID is taken as it comes
/// (the skipped one's IDs can't be trusted). The scan stops at a tail
/// `World::open` would cut ([`Finding::IncompleteTail`] at the offset where
/// the valid prefix ends) and at a begin record that can't be trusted.
pub(crate) fn survey(bytes: &[u8]) -> Result<(Vec<SurveyedTxn>, Vec<TaggedFinding>), Error> {
    let mut rest = check_header(bytes)?;
    let mut s = Survey::new();
    loop {
        // An in-memory buffer's offsets always fit a u64.
        let at = (bytes.len() - rest.len()) as u64;
        let Some((begin, after_begin)) = s.begin_record(rest, at) else {
            break;
        };
        let tx = TxId(begin.tx);
        let (tag, in_sequence) = s.judge_tx(tx);
        let BodyStep::Body {
            verdict,
            rest: after_body,
        } = body_step(&begin, after_begin)
        else {
            s.found.push((tag, Finding::IncompleteTail { offset: at }));
            break;
        };
        rest = after_body;
        s.next_tx = s.next_tx.max(begin.tx.saturating_add(1));
        let entries = match verdict {
            Ok(entries) => entries,
            Err(BodyFault::Damaged { later: None }) => {
                s.found.push((tag, Finding::IncompleteTail { offset: at }));
                break;
            }
            Err(fault) => {
                s.skip_body(tag, tx, at, fault);
                continue;
            }
        };
        let ascending = s.judge_entry_ids(tag, &entries);
        if in_sequence && ascending {
            s.txns.push(SurveyedTxn { tx, entries });
        }
    }
    Ok((s.txns, s.found))
}

/// What [`survey`] has collected so far, and where in the tx and entry-ID
/// sequences it stands.
struct Survey {
    /// The transactions kept for doctor's replay.
    txns: Vec<SurveyedTxn>,
    /// Every finding so far.
    found: Vec<TaggedFinding>,
    /// The tx the next transaction should have.
    next_tx: u64,
    /// The entry ID the next entry should have. `None` right after a skipped
    /// transaction: resynchronise.
    next_entry: Option<u64>,
    /// Past every entry ID seen: a lower one would rewind the log.
    floor: u64,
}

impl Survey {
    fn new() -> Survey {
        Survey {
            txns: Vec::new(),
            found: Vec::new(),
            next_tx: 0,
            next_entry: Some(1),
            floor: 1,
        }
    }

    /// The entry ID the `frame`-th entry of the next transaction should
    /// have, as well as it is known (for findings about unparseable bytes).
    fn entry_at(&self, frame: u64) -> EntryId {
        EntryId(self.next_entry.unwrap_or(self.floor).saturating_add(frame))
    }

    /// The begin record at the start of `rest` (file offset `at`) and the
    /// bytes after it; `None` where the survey stops: at a clean end, or,
    /// with a finding, at a torn tail or a begin record that can't be
    /// trusted.
    fn begin_record<'a>(&mut self, rest: &'a [u8], at: u64) -> Option<(Begin, &'a [u8])> {
        let expected_tx = TxId(self.next_tx);
        let stop = match begin_step(rest) {
            BeginStep::End => return None,
            BeginStep::Begin(begin, after_begin) => return Some((begin, after_begin)),
            BeginStep::Torn => Finding::IncompleteTail { offset: at },
            BeginStep::Damaged => Finding::ChecksumMismatch {
                tx: expected_tx,
                offset: at,
            },
            BeginStep::UnknownTag(tag) => Finding::InvalidEntry {
                entry: self.entry_at(0),
                reason: format!("unknown record tag {tag:#04x} at offset {at}"),
            },
        };
        self.found.push((Some(expected_tx), stop));
        None
    }

    /// Checks a begin record's `tx` against the sequence, reporting a
    /// [`Finding::TxGap`]. Returns the `since` tag for the transaction's
    /// findings and whether it may be kept (it doesn't rewind the tx
    /// sequence).
    fn judge_tx(&mut self, tx: TxId) -> (Option<TxId>, bool) {
        let expected_tx = TxId(self.next_tx);
        // A repeated or backwards tx is judged at the expected one, so
        // `since` can't hide it behind an older number.
        let tag = Some(tx.max(expected_tx));
        if tx == expected_tx {
            return (tag, true);
        }
        let f = Finding::TxGap {
            expected: expected_tx,
            found: tx,
        };
        self.found.push((tag, f));
        (tag, tx > expected_tx)
    }

    /// Reports why the body of transaction `tx` (begin record at file offset
    /// `at`) isn't a committed transaction, and skips it: its entry IDs
    /// can't be trusted, so the next transaction's are taken as they come.
    fn skip_body(&mut self, tag: Option<TxId>, tx: TxId, at: u64, fault: BodyFault) {
        let f = match fault {
            BodyFault::Damaged { .. } | BodyFault::CommitChecksum => {
                Finding::ChecksumMismatch { tx, offset: at }
            }
            BodyFault::NoCommit => Finding::MissingCommit { tx, offset: at },
            BodyFault::Invalid { frame, why } => Finding::InvalidEntry {
                entry: self.entry_at(frame as u64),
                reason: format!("tx#{}: {why}", tx.0),
            },
        };
        self.found.push((tag, f));
        self.next_entry = None;
    }

    /// Checks a parsed transaction's entry IDs against the sequence,
    /// reporting every [`Finding::EntryIdGap`], and moves the sequence past
    /// them. Returns whether the transaction may be kept (no entry ID
    /// rewinds the log).
    fn judge_entry_ids(&mut self, tag: Option<TxId>, entries: &[LogEntry]) -> bool {
        let mut ascending = true;
        for e in entries {
            if let Some(expected) = self.next_entry
                && e.id.0 != expected
            {
                let f = Finding::EntryIdGap {
                    expected: EntryId(expected),
                    found: e.id,
                };
                self.found.push((tag, f));
            }
            if e.id.0 < self.floor {
                // Kept entries must ascend (doctor's rebuild finds them by ID).
                ascending = false;
            }
            self.floor = self.floor.max(e.id.0.saturating_add(1));
            self.next_entry = Some(e.id.0.saturating_add(1));
        }
        ascending
    }
}
