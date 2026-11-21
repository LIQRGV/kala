//! Byte-level helpers for log format v2 (docs/slice-2-decisions.md, Item 1).
//! Built only from the documented layout; no private code is called.
//!
//! ```text
//! file  = header(12) txn*
//! txn   = begin(25) body
//! begin = 0x07 | tx u64 | body_len u64 | body_crc u32 | begin_crc u32   (LE)
//! body  = frame*  (frame = u32 LE payload_len | payload), last frame = Commit
//! ```
#![allow(dead_code)]

pub const HEADER_LEN: usize = 12;
pub const BEGIN_LEN: usize = 25;
pub const BEGIN_TAG: u8 = 0x07;
/// u32 len + kind + entry id + tx + checksum.
pub const COMMIT_FRAME: usize = 4 + 1 + 8 + 8 + 4;

pub fn crc(bytes: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(bytes);
    h.finalize()
}

pub fn header(version: u32) -> Vec<u8> {
    let mut h = b"KALALOG\0".to_vec();
    h.extend_from_slice(&version.to_le_bytes());
    h
}

/// A begin record with explicit fields and a CORRECT begin_crc.
pub fn begin_raw(tag: u8, tx: u64, body_len: u64, body_crc: u32) -> Vec<u8> {
    let mut b = vec![tag];
    b.extend_from_slice(&tx.to_le_bytes());
    b.extend_from_slice(&body_len.to_le_bytes());
    b.extend_from_slice(&body_crc.to_le_bytes());
    assert_eq!(b.len(), 21);
    let c = crc(&b);
    b.extend_from_slice(&c.to_le_bytes());
    assert_eq!(b.len(), BEGIN_LEN);
    b
}

/// The begin record for `body`.
pub fn begin(tx: u64, body: &[u8]) -> Vec<u8> {
    begin_raw(BEGIN_TAG, tx, body.len() as u64, crc(body))
}

/// begin + body.
pub fn txn(tx: u64, body: &[u8]) -> Vec<u8> {
    let mut v = begin(tx, body);
    v.extend_from_slice(body);
    v
}

pub fn frame(kind: u8, id: u64, body: &[u8]) -> Vec<u8> {
    let mut payload = vec![kind];
    payload.extend_from_slice(&id.to_le_bytes());
    payload.extend_from_slice(body);
    let mut f = (payload.len() as u32).to_le_bytes().to_vec();
    f.extend_from_slice(&payload);
    f
}

/// A Commit frame with a correct checksum over `prior` (the txn's frames
/// before the Commit; the begin record is not covered).
pub fn commit_frame(prior: &[u8], entry_id: u64, tx: u64) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&21u32.to_le_bytes());
    f.push(5);
    f.extend_from_slice(&entry_id.to_le_bytes());
    f.extend_from_slice(&tx.to_le_bytes());
    let mut h = crc32fast::Hasher::new();
    h.update(prior);
    h.update(&f);
    f.extend_from_slice(&h.finalize().to_le_bytes());
    assert_eq!(f.len(), COMMIT_FRAME);
    f
}

/// Location of one transaction in a well-formed v2 file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub tx: u64,
    /// offset of the begin record
    pub start: usize,
    /// offset of the first body byte (start + 25)
    pub body_start: usize,
    /// offset just past the body (= start of the next txn)
    pub end: usize,
}

/// Parses a well-formed v2 file by its begin records (CRCs are not checked).
/// Panics on anything that does not fit: the file must be one the library wrote.
pub fn spans(bytes: &[u8]) -> Vec<Span> {
    let mut out = Vec::new();
    let mut at = HEADER_LEN;
    while at < bytes.len() {
        assert!(at + BEGIN_LEN <= bytes.len(), "truncated begin at {at}");
        assert_eq!(bytes[at], BEGIN_TAG, "not a v2 begin record at {at}");
        let tx = u64::from_le_bytes(bytes[at + 1..at + 9].try_into().unwrap());
        let len = u64::from_le_bytes(bytes[at + 9..at + 17].try_into().unwrap()) as usize;
        let body_start = at + BEGIN_LEN;
        let end = body_start + len;
        assert!(end <= bytes.len(), "body of tx {tx} runs past EOF");
        out.push(Span {
            tx,
            start: at,
            body_start,
            end,
        });
        at = end;
    }
    out
}

/// `(start, end, kind)` of every frame in a txn's body.
pub fn frame_spans(bytes: &[u8], s: Span) -> Vec<(usize, usize, u8)> {
    let mut out = Vec::new();
    let mut at = s.body_start;
    while at < s.end {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        out.push((at, at + 4 + len, bytes[at + 4]));
        at += 4 + len;
    }
    assert_eq!(at, s.end, "frames must exactly fill the body");
    out
}

/// Rewrites the begin record of the txn at `s` so that body_len, body_crc and
/// begin_crc are correct for the CURRENT bytes of `bytes[s.body_start..s.end]`
/// (use after editing the body, to model damage that framing cannot see).
pub fn reseal(bytes: &mut [u8], s: Span) {
    let b = begin(s.tx, &bytes[s.body_start..s.end]);
    bytes[s.start..s.body_start].copy_from_slice(&b);
}

/// Replaces the body of the LAST txn in `bytes` with `body` and rewrites its
/// begin record (tx kept) so framing is valid.
pub fn replace_last_body(bytes: &mut Vec<u8>, body: &[u8]) {
    let s = *spans(bytes).last().unwrap();
    bytes.truncate(s.start);
    bytes.extend_from_slice(&txn(s.tx, body));
}
