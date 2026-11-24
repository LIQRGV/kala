//! Judging a log's bytes transaction by transaction: the recovery rules of
//! the [module docs](super) ("Recovery"), in two steps ([`begin_step`],
//! [`body_step`]) shared by the fail-fast [`scan`] and doctor's collecting
//! [`survey`](super::survey()).

use super::decode::parse_payload;
use super::format::{BEGIN_COVERED, BEGIN_LEN, BEGIN_TAG, HEADER_LEN};
use super::{EntryBody, LOG_FORMAT_VERSION, LOG_MAGIC, LogEntry, MAX_ENTRY_BYTES};
use crate::error::Error;
use crate::world::CutCause;

/// The committed part of a log file, as found by [`scan`].
pub(super) struct Scan {
    /// Entries of every committed transaction, in log order.
    pub(super) entries: Vec<LogEntry>,
    /// Length of the valid prefix: the header plus every committed
    /// transaction. Bytes past it are a torn or damaged tail.
    pub(super) valid_len: usize,
    /// Why the bytes past `valid_len` aren't committed; `None` if there are
    /// none.
    pub(super) cause: Option<CutCause>,
}

/// Checks the header and returns the bytes after it.
pub(super) fn check_header(bytes: &[u8]) -> Result<&[u8], Error> {
    let magic_len = bytes.len().min(LOG_MAGIC.len());
    if bytes[..magic_len] != LOG_MAGIC[..magic_len] {
        return Err(Error::BadMagic);
    }
    let Some((header, rest)) = bytes.split_first_chunk::<HEADER_LEN>() else {
        return Err(Error::Corrupt("truncated log header".to_owned()));
    };
    let version = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    if version != LOG_FORMAT_VERSION {
        return Err(Error::UnsupportedVersion(version));
    }
    Ok(rest)
}

/// Splits the next frame off `bytes`: `(payload, rest)`, or `None` if the
/// frame is incomplete. The length is checked against the bytes left.
fn next_frame(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let (len, after_len) = bytes.split_first_chunk::<4>()?;
    let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
    if after_len.len() < len {
        return None;
    }
    Some(after_len.split_at(len))
}

/// Scans a whole log file's bytes: see [`read_log`](super::read_log). This is the one
/// decoder of the log; reading and reopening both go through it, and
/// doctor's [`survey`](super::survey()) judges transactions with the same two steps
/// ([`begin_step`], [`body_step`]).
///
/// Transactions are judged one at a time from just past the header, by the
/// rules in the module docs ("Recovery"). The first one that is torn ends
/// the valid prefix ([`Scan::valid_len`]); anything else that isn't a
/// committed transaction is [`Error::Corrupt`].
pub(super) fn scan(bytes: &[u8]) -> Result<Scan, Error> {
    let mut rest = check_header(bytes)?;
    let mut entries = Vec::new();
    let mut next_entry = 1u64;
    let mut next_tx = 0u64;
    let cause = loop {
        let at = bytes.len() - rest.len();
        let (begin, after_begin) = match begin_step(rest) {
            BeginStep::End => break None,
            BeginStep::Torn => break Some(CutCause::Torn),
            BeginStep::Damaged => {
                return Err(Error::Corrupt(format!(
                    "damaged transaction header at offset {at}"
                )));
            }
            BeginStep::UnknownTag(tag) => {
                return Err(Error::Corrupt(format!(
                    "unknown record tag {tag:#04x} at offset {at}"
                )));
            }
            BeginStep::Begin(begin, after_begin) => (begin, after_begin),
        };
        // Rule 4: a sound begin record that isn't the next transaction's.
        if begin.tx != next_tx {
            return Err(Error::Corrupt(format!(
                "tx#{} found at offset {at} where tx#{next_tx} was expected",
                begin.tx
            )));
        }
        let BodyStep::Body {
            verdict,
            rest: after_body,
        } = body_step(&begin, after_begin)
        else {
            break Some(CutCause::Torn);
        };
        let mut txn = match verdict {
            Ok(txn) => txn,
            Err(BodyFault::Damaged { later: None }) => break Some(CutCause::Damaged),
            Err(BodyFault::Damaged { later: Some(later) }) => {
                return Err(Error::Corrupt(format!(
                    "tx#{}: body checksum mismatch at offset {at}, followed by a valid \
                     begin record at offset {}",
                    begin.tx,
                    bytes.len() - after_body.len() + later
                )));
            }
            Err(fault) => return Err(Error::Corrupt(format!("tx#{}: {fault}", begin.tx))),
        };
        // Rule 7, last part: entry IDs continue the log without gaps.
        for (e, expected) in txn.iter().zip(next_entry..) {
            if e.id.0 != expected {
                return Err(Error::Corrupt(format!(
                    "tx#{}: entry#{} found where entry#{expected} was expected",
                    begin.tx, e.id.0
                )));
            }
        }
        // Rule 8: committed.
        next_entry += txn.len() as u64;
        next_tx += 1;
        entries.append(&mut txn);
        rest = after_body;
    };
    Ok(Scan {
        entries,
        valid_len: bytes.len() - rest.len(),
        cause,
    })
}

/// The first half of judging the transaction at the start of `rest`
/// (recovery rules 1 to 4, all but the tx check, which is the caller's).
pub(super) enum BeginStep<'a> {
    /// Rule 1: no bytes left, a clean end.
    End,
    /// Rule 2 (not even a whole begin record), or rule 3 with a zero-filled
    /// tail: the last append never fully reached the disk.
    Torn,
    /// Rule 3: a begin record whose checksum fails, not a zero-filled tail.
    /// None of its fields can be trusted, so nothing after it can be framed.
    Damaged,
    /// Rule 4: a sound begin record with a tag other than `0x07`.
    UnknownTag(u8),
    /// A sound begin record and the bytes after it.
    Begin(Begin, &'a [u8]),
}

pub(super) fn begin_step(rest: &[u8]) -> BeginStep<'_> {
    if rest.is_empty() {
        return BeginStep::End;
    }
    let Some((begin, after_begin)) = rest.split_first_chunk::<BEGIN_LEN>() else {
        return BeginStep::Torn;
    };
    let begin = Begin::parse(begin);
    // Only a zero-filled tail (how some file systems expose an append that
    // never reached the disk) is a torn write; anything else may hide
    // committed data behind it.
    if !begin.crc_ok {
        return if rest.iter().all(|&b| b == 0) {
            BeginStep::Torn
        } else {
            BeginStep::Damaged
        };
    }
    if begin.tag != BEGIN_TAG {
        return BeginStep::UnknownTag(begin.tag);
    }
    BeginStep::Begin(begin, after_begin)
}

/// The second half of judging a transaction with a sound begin record
/// (recovery rules 5 to 7, all but the entry-ID check, which is the
/// caller's: the scan and doctor treat gaps differently).
pub(super) enum BodyStep<'a> {
    /// Rule 5: the body runs past the end of the file.
    Torn,
    /// The body is all there: its parsed entries (the `Commit` last) or why
    /// they aren't a committed transaction, and the bytes after the body.
    Body {
        verdict: Result<Vec<LogEntry>, BodyFault>,
        rest: &'a [u8],
    },
}

/// Why a whole transaction body (rule 6 or 7) isn't a committed transaction.
#[derive(Debug)]
pub(super) enum BodyFault {
    /// Rule 6: `body_crc` fails. `later` is the offset, from just past the
    /// body, of a later valid begin record, if any: without one the
    /// transaction is a damaged tail.
    Damaged { later: Option<usize> },
    /// Rule 7: the body has no `Commit` (its frames end without one).
    NoCommit,
    /// Rule 7: the `Commit` checksum doesn't match.
    CommitChecksum,
    /// Rule 7: anything else; `frame` is the index of the offending frame.
    Invalid { frame: usize, why: String },
}

impl std::fmt::Display for BodyFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyFault::Damaged { .. } => f.write_str("body checksum mismatch"),
            BodyFault::NoCommit => f.write_str("no Commit"),
            BodyFault::CommitChecksum => f.write_str("Commit checksum mismatch"),
            BodyFault::Invalid { why, .. } => f.write_str(why),
        }
    }
}

pub(super) fn body_step<'a>(begin: &Begin, after_begin: &'a [u8]) -> BodyStep<'a> {
    // The length is checked against the bytes left before it is used.
    let Some(body_len) = usize::try_from(begin.body_len)
        .ok()
        .filter(|&n| n <= after_begin.len())
    else {
        return BodyStep::Torn;
    };
    let (body, rest) = after_begin.split_at(body_len);
    // Every byte is there but they don't match: damage. Cut only if nothing
    // that could be a later committed transaction follows.
    let verdict = if crc32fast::hash(body) != begin.body_crc {
        Err(BodyFault::Damaged {
            later: later_begin(rest, begin.tx),
        })
    } else {
        parse_body(body, begin.tx)
    };
    BodyStep::Body { verdict, rest }
}

/// The offset in `bytes` of the first valid begin record (tag `0x07`,
/// matching `begin_crc`) whose tx is greater than `tx`, trying every byte
/// offset: a damaged transaction followed by one may hide committed data, so
/// it must not be cut (recovery rule 6).
fn later_begin(bytes: &[u8], tx: u64) -> Option<usize> {
    (0..bytes.len().saturating_sub(BEGIN_LEN - 1)).find(|&i| {
        // The tag is checked first so most offsets cost no checksum.
        bytes[i] == BEGIN_TAG
            && bytes[i..]
                .first_chunk::<BEGIN_LEN>()
                .map(Begin::parse)
                .is_some_and(|b| b.crc_ok && b.tx > tx)
    })
}

/// A decoded begin record (no field is trusted unless `crc_ok`).
pub(super) struct Begin {
    tag: u8,
    pub(super) tx: u64,
    body_len: u64,
    body_crc: u32,
    /// Whether the record's own checksum matches its first 21 bytes.
    crc_ok: bool,
}

impl Begin {
    fn parse(b: &[u8; BEGIN_LEN]) -> Begin {
        let u64_at = |i: usize| {
            let mut x = [0u8; 8];
            x.copy_from_slice(&b[i..i + 8]);
            u64::from_le_bytes(x)
        };
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Begin {
            tag: b[0],
            tx: u64_at(1),
            body_len: u64_at(9),
            body_crc: u32_at(17),
            crc_ok: crc32fast::hash(&b[..BEGIN_COVERED]) == u32_at(BEGIN_COVERED),
        }
    }
}

/// Parses a transaction body whose framing checked out: frames exactly
/// filling it, exactly one `Commit`, last, for `tx`, with a matching
/// checksum. Returns its entries, the `Commit` included. Entry IDs are not
/// checked here (see [`BodyStep`]).
fn parse_body(body: &[u8], tx: u64) -> Result<Vec<LogEntry>, BodyFault> {
    let mut entries = Vec::new();
    let mut rest = body;
    loop {
        let invalid = |why: String| BodyFault::Invalid {
            frame: entries.len(),
            why,
        };
        // Checked before the frame is split off: a declared length over the
        // limit is damage whatever bytes follow (no writer produces one), so
        // it is never mistaken for a frame cut short.
        if let Some(len) = rest.first_chunk::<4>().map(|b| u32::from_le_bytes(*b))
            && usize::try_from(len).map_or(true, |len| len > MAX_ENTRY_BYTES)
        {
            return Err(invalid(format!(
                "a frame declares a {len}-byte entry, over the {MAX_ENTRY_BYTES}-byte limit"
            )));
        }
        let Some((payload, after)) = next_frame(rest) else {
            return Err(if rest.is_empty() {
                BodyFault::NoCommit
            } else {
                invalid("a frame runs past the end of the transaction".to_owned())
            });
        };
        rest = after;
        let entry = parse_payload(payload).map_err(invalid)?;
        if let EntryBody::Commit { tx: ctx, checksum } = entry.body {
            if !rest.is_empty() {
                return Err(invalid("bytes after the Commit".to_owned()));
            }
            if ctx.0 != tx {
                return Err(invalid(format!("its Commit says tx#{}", ctx.0)));
            }
            // Covered: every byte of the body except the Commit's own
            // checksum field, its last 4 bytes (a parsed Commit frame is
            // 25 bytes, so the body is at least that long).
            if crc32fast::hash(&body[..body.len() - 4]) != checksum {
                return Err(BodyFault::CommitChecksum);
            }
            entries.push(entry);
            return Ok(entries);
        }
        entries.push(entry);
    }
}
