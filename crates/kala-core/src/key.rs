//! Byte layout of index keys.
//!
//! The forward index `E` holds one history per `(entity, attribute, value)`:
//!
//! ```text
//! b'E' | entity (8, BE) | attr (4, BE) | value encoding | ^tick (8) | ^tx (8)
//! ```
//!
//! Every part before the value is fixed-width and the value encoding is
//! self-delimiting, so `E|e|a|` and `E|e|a|v|` are exact prefixes: no other
//! entity, attribute or value can start with the same bytes. `^tick` and
//! `^tx` are inverted, so within one `E|e|a|v|` history the newest tick (and,
//! for one tick, the newest tx) sorts first; a seek to `E|e|a|v|^T` therefore
//! lands on the latest entry with tick `<= T`.
//!
//! The reverse index `R` mirrors every `ref` fact under its target:
//!
//! ```text
//! b'R' | target (8, BE) | attr (4, BE) | source (8, BE) | ^tick (8) | ^tx (8)
//! ```
//!
//! Every part is fixed-width, so an `R` key is always [`R_KEY_LEN`] bytes
//! and `R|t|a|` and `R|t|a|s|` are exact prefixes. The tick and tx suffix
//! is the same as in `E`, so one history sorts newest first in both.
//!
//! The value index `V` mirrors every entry of an *indexed, non-ref*
//! attribute under its value ("the entity named Edwin"; `ref` attributes
//! use `R` as their value index):
//!
//! ```text
//! b'V' | attr (4, BE) | value encoding | entity (8, BE) | ^tick (8) | ^tx (8)
//! ```
//!
//! The value encoding is self-delimiting and everything around it is
//! fixed-width, so `V|a|v|` (every entity that ever had `a = v`) and
//! `V|a|v|e|` (one entity's history of that value) are exact prefixes, and
//! the same `^tick|^tx` suffix orders one history newest first, as in `E`.
//!
//! The folded value index `VFold` serves case-insensitive search. It
//! mirrors every entry of an *indexed string* attribute (the same entries as
//! `V`) under the value's search form
//! ([`fold_for_search`](crate::value::fold_for_search)), encoded as a
//! string value, followed by the original value's encoding:
//!
//! ```text
//! b'F' | attr (4, BE) | fold encoding (as Str) | original value encoding | entity (8, BE) | ^tick (8) | ^tx (8)
//! ```
//!
//! Both encodings are self-delimiting (a string ends in `0x00 0x00`, and a
//! content `0x00` is escaped), so `F|a|fold|` is an exact prefix: the
//! prefix for the fold `"ed"` never matches a key whose fold is `"edwin"`.
//! The original value comes before the entity so two values that fold
//! equal (`"Edwin"`, `"EDWIN"`) keep separate histories, each judged
//! latest-wins on its own: `F|a|fold|orig|e|` is one history. Folds may be
//! longer than the 256-byte indexed limit (a fold can grow), so the fold is
//! encoded without that check.
//!
//! **Unindexed strings in `E`.** The value part of an `E` (and `E_latest`)
//! key of an `indexed: false` string attribute is not the value encoding but
//! a fixed-width **value reference** into the world's in-memory value table
//! (derived, rebuilt from the log with the indexes), so a biography of any
//! length costs [`VALUE_REF_LEN`] bytes per key:
//!
//! ```text
//! value ref = tag 0x06 | content hash (8, BE) | collision ordinal (4, BE)
//! ```
//!
//! `0x06` is no value type tag, so a reference never parses as a value and
//! the part stays self-delimiting. Key order between references is hash
//! order, which means nothing: every public answer re-sorts by the full
//! value encoding.
//!
//! The latest-revision indexes (`E_latest`, `R_latest`, `V_latest`,
//! `VFold_latest`) use the
//! same keys without the trailing `^tx` ([`without_tx`]): one key per
//! `(…, tick)`, holding its current entry at the latest canon.
//!
//! Keys are built from encoded bytes only (the value is already
//! [`Value::encode`](crate::value::Value::encode)d), never from `Value`.

use crate::id::{AttrId, EntityId};
use crate::tick::Tick;
use crate::value::{ValueError, encoded_len};
use crate::world::TxId;

/// Tag byte of the forward index.
const E_TAG: u8 = b'E';

/// Tag byte of the reverse index.
const R_TAG: u8 = b'R';

/// Tag byte of the value index.
const V_TAG: u8 = b'V';

/// Length of `V|attr|`, the part before the value.
const V_ATTR_PREFIX_LEN: usize = 1 + 4;

/// Tag byte of the folded value index.
const VFOLD_TAG: u8 = b'F';

/// Length of `F|attr|`, the part before the fold (same as `V|attr|`).
const VFOLD_ATTR_PREFIX_LEN: usize = 1 + 4;

/// Tag byte of a value reference in an `E` key (see the module docs). Not a
/// value type tag, so a reference never decodes as a value.
pub(crate) const VALUE_REF_TAG: u8 = 0x06;

/// Length of a value reference: tag, content hash (8), ordinal (4).
pub(crate) const VALUE_REF_LEN: usize = 1 + 8 + 4;

/// The `E`-key part standing for an unindexed string value: its content
/// hash and its collision ordinal among the values sharing that hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ValueRef {
    pub(crate) hash: u64,
    pub(crate) ordinal: u32,
}

impl ValueRef {
    /// The reference's key bytes: `0x06 | hash BE | ordinal BE`.
    pub(crate) fn to_key_bytes(self) -> [u8; VALUE_REF_LEN] {
        let mut k = [0u8; VALUE_REF_LEN];
        k[0] = VALUE_REF_TAG;
        k[1..9].copy_from_slice(&self.hash.to_be_bytes());
        k[9..].copy_from_slice(&self.ordinal.to_be_bytes());
        k
    }

    /// The reference an `E`-key value part holds, or `None` if the part is
    /// not exactly a value reference (e.g. a value encoding).
    pub(crate) fn from_key_bytes(part: &[u8]) -> Option<ValueRef> {
        let k: &[u8; VALUE_REF_LEN] = part.try_into().ok()?;
        if k[0] != VALUE_REF_TAG {
            return None;
        }
        Some(ValueRef {
            hash: u64::from_be_bytes(k[1..9].try_into().ok()?),
            ordinal: u32::from_be_bytes(k[9..].try_into().ok()?),
        })
    }
}

/// The length of the value part at the start of `rest` (an `E` key after
/// `E|e|a|`): a value reference or a value encoding. Never panics.
fn e_value_part_len(rest: &[u8]) -> Result<usize, ValueError> {
    if rest.first() == Some(&VALUE_REF_TAG) {
        return if rest.len() >= VALUE_REF_LEN {
            Ok(VALUE_REF_LEN)
        } else {
            Err(ValueError::Malformed("truncated value reference"))
        };
    }
    encoded_len(rest)
}

/// Length of every `R` key: `R|target|attr|source|^tick|^tx`.
pub(crate) const R_KEY_LEN: usize = 1 + 8 + 4 + 8 + SUFFIX_LEN;

/// Length of `E|entity|attr|`, the part before the value.
pub(crate) const E_ATTR_PREFIX_LEN: usize = 1 + 8 + 4;

/// Length of the `^tick|^tx` suffix after the value.
const SUFFIX_LEN: usize = 8 + 8;

/// `E|entity|attr|`: the prefix of every history of one attribute.
pub(crate) fn e_attr_prefix(entity: EntityId, attr: AttrId) -> Vec<u8> {
    let mut k = Vec::with_capacity(E_ATTR_PREFIX_LEN + 16 + SUFFIX_LEN);
    k.push(E_TAG);
    k.extend_from_slice(&entity.to_bytes());
    k.extend_from_slice(&attr.0.to_be_bytes());
    k
}

/// `E|entity|attr|value|`: the prefix of one value's history.
pub(crate) fn e_value_prefix(entity: EntityId, attr: AttrId, value: &[u8]) -> Vec<u8> {
    let mut k = e_attr_prefix(entity, attr);
    k.extend_from_slice(value);
    k
}

/// Appends `^tick` to a value prefix: the seek key for "latest entry with
/// tick `<= tick`". It is shorter than any full key with the same `^tick`,
/// so it sorts before all of them.
pub(crate) fn push_seek_tick(value_prefix: &mut Vec<u8>, tick: Tick) {
    value_prefix.extend_from_slice(&tick.encode_inverted());
}

/// The full `E` key of one fact.
pub(crate) fn e_fact_key(
    entity: EntityId,
    attr: AttrId,
    value: &[u8],
    tick: Tick,
    tx: TxId,
) -> Vec<u8> {
    let mut k = e_value_prefix(entity, attr, value);
    push_seek_tick(&mut k, tick);
    k.extend_from_slice(&(!tx.0).to_be_bytes());
    k
}

/// The tx of a full `E` key (its trailing `^tx`), or `None` if the key is
/// too short to have one.
pub(crate) fn e_key_tx(key: &[u8]) -> Option<TxId> {
    let start = key.len().checked_sub(8)?;
    let bytes: [u8; 8] = key.get(start..)?.try_into().ok()?;
    Some(TxId(!u64::from_be_bytes(bytes)))
}

/// The tick and tx of a full `E` or `R` key (its trailing `^tick|^tx`),
/// or `None` if the key is too short to have them. Reads only the fixed
/// suffix, so it needs no value decoding.
pub(crate) fn e_key_tick_and_tx(key: &[u8]) -> Option<(Tick, TxId)> {
    let start = key.len().checked_sub(16)?;
    let tick: [u8; 8] = key.get(start..start + 8)?.try_into().ok()?;
    Some((Tick::decode_inverted(tick), e_key_tx(key)?))
}

/// The largest possible key under `value_prefix`; seek *exclusively* past
/// it to reach the next distinct value.
///
/// Every key under a value prefix is that prefix plus exactly
/// [`SUFFIX_LEN`] bytes, so no key under it is greater than the all-`0xFF`
/// suffix, and every key strictly greater belongs to a later value (or a
/// later attribute/entity, which the caller's prefix check catches). It
/// works for latest-revision keys (prefix plus 8 bytes) too: they are
/// shorter than the all-`0xFF` suffix and still share the prefix.
pub(crate) fn past_value(value_prefix: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(value_prefix.len() + SUFFIX_LEN);
    k.extend_from_slice(value_prefix);
    k.extend_from_slice(&[0xFF; SUFFIX_LEN]);
    k
}

/// The largest possible key with the same history prefix and tick as the
/// full `E` or `R` key `key`; seek *exclusively* past it to reach the next
/// (older) tick of that history. Both layouts end in `^tx` (8 bytes), so
/// the all-`0xFF` tx part is the greatest key of that tick.
pub(crate) fn past_tick(key: &[u8]) -> Vec<u8> {
    let head = &key[..key.len().saturating_sub(8)];
    let mut k = Vec::with_capacity(head.len() + 8);
    k.extend_from_slice(head);
    k.extend_from_slice(&[0xFF; 8]);
    k
}

/// The full `E` or `R` key `key` without its `^tx`: the key's history
/// prefix and tick, shared by every entry at that `(…, tick)`.
pub(crate) fn without_tx(key: &[u8]) -> &[u8] {
    &key[..key.len().saturating_sub(8)]
}

/// Splits the value part (a value encoding or a value reference) out of a
/// full `E` key under `E|e|a|`. Fails if the key is too short or its value
/// part isn't well-formed.
pub(crate) fn e_value_bytes(key: &[u8]) -> Result<&[u8], ValueError> {
    value_bytes_before(key, SUFFIX_LEN)
}

/// The value part (encoding or reference) of an `E`-layout key under
/// `E|e|a|` that ends in a suffix of exactly `suffix_len` bytes after it.
fn value_bytes_before(key: &[u8], suffix_len: usize) -> Result<&[u8], ValueError> {
    let rest = key
        .get(E_ATTR_PREFIX_LEN..)
        .ok_or(ValueError::Malformed("index key shorter than its prefix"))?;
    let len = e_value_part_len(rest)?;
    if rest.len() != len + suffix_len {
        return Err(ValueError::Malformed(
            "index key suffix has the wrong length",
        ));
    }
    Ok(&rest[..len])
}

/// Splits a full `E` key under `E|e|a|` into its value encoding and tick.
/// Fails like [`e_value_bytes`] if the key isn't a well-formed `E` key.
pub(crate) fn e_value_and_tick(key: &[u8]) -> Result<(&[u8], Tick), ValueError> {
    let value = e_value_bytes(key)?;
    let start = E_ATTR_PREFIX_LEN + value.len();
    // `e_value_bytes` checked that exactly `^tick|^tx` follows the value.
    let tick: [u8; 8] = key
        .get(start..start + 8)
        .and_then(|b| b.try_into().ok())
        .ok_or(ValueError::Malformed("index key suffix is not ^tick|^tx"))?;
    Ok((value, Tick::decode_inverted(tick)))
}

/// Splits a latest-index `E` key (`E|e|a|v|^tick`, a full key
/// [`without_tx`]) into its value encoding and tick. Fails if the key isn't
/// a well-formed latest `E` key.
pub(crate) fn latest_e_value_and_tick(key: &[u8]) -> Result<(&[u8], Tick), ValueError> {
    let value = value_bytes_before(key, 8)?;
    let start = E_ATTR_PREFIX_LEN + value.len();
    // `value_bytes_before` checked that exactly `^tick` follows the value.
    let tick: [u8; 8] = key
        .get(start..start + 8)
        .and_then(|b| b.try_into().ok())
        .ok_or(ValueError::Malformed("index key suffix is not ^tick"))?;
    Ok((value, Tick::decode_inverted(tick)))
}

/// A decoded latest-index `E` key: `(entity, attr, value encoding, tick)`.
pub(crate) type LatestEKeyParts<'k> = (EntityId, AttrId, &'k [u8], Tick);

/// Splits a latest-index `E` key (`E|e|a|v|^tick`) into its parts. Fails if
/// it isn't one (wrong tag, too short, a malformed value part or a suffix
/// other than exactly `^tick`). Never panics, whatever the bytes.
pub(crate) fn decode_latest_e_key(key: &[u8]) -> Result<LatestEKeyParts<'_>, ValueError> {
    if key.first() != Some(&E_TAG) {
        return Err(ValueError::Malformed("not an E key"));
    }
    let (value, tick) = latest_e_value_and_tick(key)?;
    let short = || ValueError::Malformed("index key shorter than its prefix");
    let entity = EntityId::from_bytes(bytes8(key, 1).ok_or_else(short)?);
    let attr: [u8; 4] = key
        .get(9..13)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(short)?;
    Ok((entity, AttrId(u32::from_be_bytes(attr)), value, tick))
}

/// A decoded latest-index `R` key: `(target, attr, source, tick)`.
pub(crate) type LatestRKeyParts = (EntityId, AttrId, EntityId, Tick);

/// Splits a latest-index `R` key (`R|t|a|s|^tick`) into its parts, or
/// `None` if it isn't exactly [`R_LATEST_KEY_LEN`] bytes with the `R` tag.
pub(crate) fn decode_latest_r_key(key: &[u8]) -> Option<LatestRKeyParts> {
    let (source, tick) = latest_r_source_and_tick(key)?;
    let target = EntityId::from_bytes(bytes8(key, 1)?);
    let attr: [u8; 4] = key.get(9..13)?.try_into().ok()?;
    Some((target, AttrId(u32::from_be_bytes(attr)), source, tick))
}

/// Length of a latest-index `R` key: `R|target|attr|source|^tick`.
pub(crate) const R_LATEST_KEY_LEN: usize = R_KEY_LEN - 8;

/// Splits a latest-index `R` key (`R|t|a|s|^tick`, a full key
/// [`without_tx`]) into its source and tick, or `None` if it isn't exactly
/// [`R_LATEST_KEY_LEN`] bytes with the `R` tag.
pub(crate) fn latest_r_source_and_tick(key: &[u8]) -> Option<(EntityId, Tick)> {
    if key.len() != R_LATEST_KEY_LEN || key.first() != Some(&R_TAG) {
        return None;
    }
    Some((
        EntityId::from_bytes(bytes8(key, R_ATTR_PREFIX_LEN)?),
        Tick::decode_inverted(bytes8(key, R_SOURCE_PREFIX_LEN)?),
    ))
}

/// The full `R` key of one ref fact `source attr target`.
pub(crate) fn r_fact_key(
    target: EntityId,
    attr: AttrId,
    source: EntityId,
    tick: Tick,
    tx: TxId,
) -> Vec<u8> {
    let mut k = Vec::with_capacity(R_KEY_LEN);
    k.push(R_TAG);
    k.extend_from_slice(&target.to_bytes());
    k.extend_from_slice(&attr.0.to_be_bytes());
    k.extend_from_slice(&source.to_bytes());
    k.extend_from_slice(&tick.encode_inverted());
    k.extend_from_slice(&(!tx.0).to_be_bytes());
    k
}

/// Length of `R|target|attr|`, the part before the source.
pub(crate) const R_ATTR_PREFIX_LEN: usize = 1 + 8 + 4;

/// Length of `R|target|attr|source|`: one source's history prefix.
pub(crate) const R_SOURCE_PREFIX_LEN: usize = R_ATTR_PREFIX_LEN + 8;

/// `R|target|attr|`: the prefix of every source of one attribute into
/// `target`.
pub(crate) fn r_attr_prefix(target: EntityId, attr: AttrId) -> Vec<u8> {
    let mut k = Vec::with_capacity(R_KEY_LEN);
    k.push(R_TAG);
    k.extend_from_slice(&target.to_bytes());
    k.extend_from_slice(&attr.0.to_be_bytes());
    k
}

/// The source of an `R` key, or of an `R|t|a|s|` prefix; `None` if the
/// bytes are too short to hold one.
pub(crate) fn r_source(key: &[u8]) -> Option<EntityId> {
    bytes8(key, R_ATTR_PREFIX_LEN).map(EntityId::from_bytes)
}

/// Reads 8 bytes at `at`, if the key has them.
fn bytes8(key: &[u8], at: usize) -> Option<[u8; 8]> {
    key.get(at..at + 8)?.try_into().ok()
}

/// A decoded `E` key: `(entity, attr, value encoding, tick, tx)`.
pub(crate) type EKeyParts<'k> = (EntityId, AttrId, &'k [u8], Tick, TxId);

/// Splits a full `E` key into its parts. Fails if the key isn't a
/// well-formed `E` key.
pub(crate) fn decode_e_key(key: &[u8]) -> Result<EKeyParts<'_>, ValueError> {
    if key.first() != Some(&E_TAG) {
        return Err(ValueError::Malformed("not an E key"));
    }
    let (value, tick) = e_value_and_tick(key)?;
    // `e_value_and_tick` checked the key holds the prefix, a value and the
    // `^tick|^tx` suffix, so these reads are in bounds.
    let short = || ValueError::Malformed("index key shorter than its prefix");
    let entity = EntityId::from_bytes(bytes8(key, 1).ok_or_else(short)?);
    let attr: [u8; 4] = key
        .get(9..13)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(short)?;
    let tx = e_key_tx(key).ok_or_else(short)?;
    Ok((entity, AttrId(u32::from_be_bytes(attr)), value, tick, tx))
}

/// A decoded `R` key: `(target, attr, source, tick, tx)`.
pub(crate) type RKeyParts = (EntityId, AttrId, EntityId, Tick, TxId);

/// Splits a full `R` key into its parts. Fails if the key isn't exactly
/// [`R_KEY_LEN`] bytes with the `R` tag.
pub(crate) fn decode_r_key(key: &[u8]) -> Result<RKeyParts, ValueError> {
    if key.len() != R_KEY_LEN || key[0] != R_TAG {
        return Err(ValueError::Malformed("not an R key"));
    }
    // Fixed width: every slice below is in bounds after the length check.
    let get8 = |at| bytes8(key, at).ok_or(ValueError::Malformed("not an R key"));
    let attr: [u8; 4] = key[9..13]
        .try_into()
        .map_err(|_| ValueError::Malformed("not an R key"))?;
    Ok((
        EntityId::from_bytes(get8(1)?),
        AttrId(u32::from_be_bytes(attr)),
        EntityId::from_bytes(get8(13)?),
        Tick::decode_inverted(get8(21)?),
        TxId(!u64::from_be_bytes(get8(29)?)),
    ))
}

/// Length of the entity part after the value in a `V` key.
pub(crate) const V_ENTITY_LEN: usize = 8;

/// `V|attr|value|`: the prefix of every entity's history of one value.
pub(crate) fn v_value_prefix(attr: AttrId, value: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(V_ATTR_PREFIX_LEN + value.len() + V_ENTITY_LEN + SUFFIX_LEN);
    k.push(V_TAG);
    k.extend_from_slice(&attr.0.to_be_bytes());
    k.extend_from_slice(value);
    k
}

/// The full `V` key of one entry of `entity attr value` at `tick`, written
/// by `tx`.
pub(crate) fn v_fact_key(
    attr: AttrId,
    value: &[u8],
    entity: EntityId,
    tick: Tick,
    tx: TxId,
) -> Vec<u8> {
    let mut k = v_value_prefix(attr, value);
    k.extend_from_slice(&entity.to_bytes());
    push_seek_tick(&mut k, tick);
    k.extend_from_slice(&(!tx.0).to_be_bytes());
    k
}

/// The entity of a `V|a|v|e|` history prefix (its last 8 bytes), or `None`
/// if it is too short to hold one.
pub(crate) fn v_prefix_entity(history_prefix: &[u8]) -> Option<EntityId> {
    let start = history_prefix.len().checked_sub(V_ENTITY_LEN)?;
    bytes8(history_prefix, start).map(EntityId::from_bytes)
}

/// A `V`-layout key split at its entity: `(attr, value encoding, entity,
/// the bytes after the entity)`.
type VKeySplit<'k> = (AttrId, &'k [u8], EntityId, &'k [u8]);

/// Splits a `V`-layout key ending in exactly `suffix_len` bytes after the
/// entity into its [`VKeySplit`]. Never panics.
fn split_v_key(key: &[u8], suffix_len: usize) -> Result<VKeySplit<'_>, ValueError> {
    if key.first() != Some(&V_TAG) {
        return Err(ValueError::Malformed("not a V key"));
    }
    let short = || ValueError::Malformed("index key shorter than its prefix");
    let attr: [u8; 4] = key
        .get(1..V_ATTR_PREFIX_LEN)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(short)?;
    let rest = key.get(V_ATTR_PREFIX_LEN..).ok_or_else(short)?;
    let len = encoded_len(rest)?;
    if rest.len() != len + V_ENTITY_LEN + suffix_len {
        return Err(ValueError::Malformed(
            "index key suffix has the wrong length",
        ));
    }
    let entity = EntityId::from_bytes(bytes8(rest, len).ok_or_else(short)?);
    Ok((
        AttrId(u32::from_be_bytes(attr)),
        &rest[..len],
        entity,
        &rest[len + V_ENTITY_LEN..],
    ))
}

/// A decoded `V` key: `(attr, value encoding, entity, tick, tx)`.
pub(crate) type VKeyParts<'k> = (AttrId, &'k [u8], EntityId, Tick, TxId);

/// Splits a full `V` key into its parts. Fails if it isn't a well-formed
/// `V` key; never panics, whatever the bytes.
pub(crate) fn decode_v_key(key: &[u8]) -> Result<VKeyParts<'_>, ValueError> {
    let (attr, value, entity, suffix) = split_v_key(key, SUFFIX_LEN)?;
    let bad = || ValueError::Malformed("index key suffix is not ^tick|^tx");
    let tick = Tick::decode_inverted(bytes8(suffix, 0).ok_or_else(bad)?);
    let tx = TxId(!u64::from_be_bytes(bytes8(suffix, 8).ok_or_else(bad)?));
    Ok((attr, value, entity, tick, tx))
}

/// A decoded latest-index `V` key: `(attr, value encoding, entity, tick)`.
pub(crate) type LatestVKeyParts<'k> = (AttrId, &'k [u8], EntityId, Tick);

/// Splits a latest-index `V` key (`V|a|v|e|^tick`, a full key
/// [`without_tx`]) into its parts. Fails if it isn't one; never panics.
pub(crate) fn decode_latest_v_key(key: &[u8]) -> Result<LatestVKeyParts<'_>, ValueError> {
    let (attr, value, entity, suffix) = split_v_key(key, 8)?;
    let bad = || ValueError::Malformed("index key suffix is not ^tick");
    let tick = Tick::decode_inverted(bytes8(suffix, 0).ok_or_else(bad)?);
    Ok((attr, value, entity, tick))
}

/// `F|attr|fold|`: the prefix of every history whose value folds to
/// `fold` (`fold` is the fold's string value encoding).
pub(crate) fn vfold_prefix(attr: AttrId, fold: &[u8]) -> Vec<u8> {
    let mut k =
        Vec::with_capacity(VFOLD_ATTR_PREFIX_LEN + 2 * fold.len() + V_ENTITY_LEN + SUFFIX_LEN);
    k.push(VFOLD_TAG);
    k.extend_from_slice(&attr.0.to_be_bytes());
    k.extend_from_slice(fold);
    k
}

/// The full `VFold` key of one entry of `entity attr original` at `tick`,
/// written by `tx`; `fold` is the encoding of the original's fold.
pub(crate) fn vfold_fact_key(
    attr: AttrId,
    fold: &[u8],
    original: &[u8],
    entity: EntityId,
    tick: Tick,
    tx: TxId,
) -> Vec<u8> {
    let mut k = vfold_prefix(attr, fold);
    k.extend_from_slice(original);
    k.extend_from_slice(&entity.to_bytes());
    push_seek_tick(&mut k, tick);
    k.extend_from_slice(&(!tx.0).to_be_bytes());
    k
}

/// The length of the `F|a|fold|orig|e|` history prefix of a `VFold` key
/// (either layout) under the `F|a|fold|` prefix of length `fold_prefix_len`.
/// Never panics and never exceeds `key.len()`: a key whose original part
/// doesn't parse (only reachable by tampering) is taken whole.
pub(crate) fn vfold_history_end(fold_prefix_len: usize, key: &[u8]) -> usize {
    key.get(fold_prefix_len..)
        .and_then(|rest| encoded_len(rest).ok())
        .map_or(key.len(), |len| {
            (fold_prefix_len + len + V_ENTITY_LEN).min(key.len())
        })
}

/// A `VFold`-layout key split at its entity: `(attr, fold encoding,
/// original encoding, entity, the bytes after the entity)`.
type VFoldKeySplit<'k> = (AttrId, &'k [u8], &'k [u8], EntityId, &'k [u8]);

/// Splits a `VFold`-layout key ending in exactly `suffix_len` bytes after
/// the entity into its [`VFoldKeySplit`]. Never panics.
fn split_vfold_key(key: &[u8], suffix_len: usize) -> Result<VFoldKeySplit<'_>, ValueError> {
    if key.first() != Some(&VFOLD_TAG) {
        return Err(ValueError::Malformed("not a VFold key"));
    }
    let short = || ValueError::Malformed("index key shorter than its prefix");
    let attr: [u8; 4] = key
        .get(1..VFOLD_ATTR_PREFIX_LEN)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(short)?;
    let rest = key.get(VFOLD_ATTR_PREFIX_LEN..).ok_or_else(short)?;
    let fold_len = encoded_len(rest)?;
    let after_fold = rest.get(fold_len..).ok_or_else(short)?;
    let orig_len = encoded_len(after_fold)?;
    if after_fold.len() != orig_len + V_ENTITY_LEN + suffix_len {
        return Err(ValueError::Malformed(
            "index key suffix has the wrong length",
        ));
    }
    let entity = EntityId::from_bytes(bytes8(after_fold, orig_len).ok_or_else(short)?);
    Ok((
        AttrId(u32::from_be_bytes(attr)),
        &rest[..fold_len],
        &after_fold[..orig_len],
        entity,
        &after_fold[orig_len + V_ENTITY_LEN..],
    ))
}

/// A decoded `VFold` key: `(attr, fold encoding, original encoding, entity,
/// tick, tx)`.
pub(crate) type VFoldKeyParts<'k> = (AttrId, &'k [u8], &'k [u8], EntityId, Tick, TxId);

/// Splits a full `VFold` key into its parts. Fails if it isn't a
/// well-formed `VFold` key; never panics, whatever the bytes.
pub(crate) fn decode_vfold_key(key: &[u8]) -> Result<VFoldKeyParts<'_>, ValueError> {
    let (attr, fold, original, entity, suffix) = split_vfold_key(key, SUFFIX_LEN)?;
    let bad = || ValueError::Malformed("index key suffix is not ^tick|^tx");
    let tick = Tick::decode_inverted(bytes8(suffix, 0).ok_or_else(bad)?);
    let tx = TxId(!u64::from_be_bytes(bytes8(suffix, 8).ok_or_else(bad)?));
    Ok((attr, fold, original, entity, tick, tx))
}

/// A decoded latest-index `VFold` key: `(attr, fold encoding, original
/// encoding, entity, tick)`.
pub(crate) type LatestVFoldKeyParts<'k> = (AttrId, &'k [u8], &'k [u8], EntityId, Tick);

/// Splits a latest-index `VFold` key (`F|a|fold|orig|e|^tick`, a full key
/// [`without_tx`]) into its parts. Fails if it isn't one; never panics.
pub(crate) fn decode_latest_vfold_key(key: &[u8]) -> Result<LatestVFoldKeyParts<'_>, ValueError> {
    let (attr, fold, original, entity, suffix) = split_vfold_key(key, 8)?;
    let bad = || ValueError::Malformed("index key suffix is not ^tick");
    let tick = Tick::decode_inverted(bytes8(suffix, 0).ok_or_else(bad)?);
    Ok((attr, fold, original, entity, tick))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::NodeTypeId;
    use crate::value::Value;

    fn ent(t: u16, seq: u64) -> EntityId {
        EntityId::new(NodeTypeId(t), seq).expect("valid id")
    }

    fn venc(v: &Value) -> Vec<u8> {
        v.encode().unwrap()
    }

    fn s(x: &str) -> Value {
        Value::Str(x.to_string())
    }

    const MAX_SEQ: u64 = (1 << 48) - 1;

    #[test]
    fn attr_prefix_layout() {
        let k = e_attr_prefix(ent(1, 1), AttrId(7));
        assert_eq!(k.len(), E_ATTR_PREFIX_LEN);
        assert_eq!(k[0], b'E');
        assert_eq!(&k[1..9], &ent(1, 1).to_bytes());
        assert_eq!(&k[9..13], &7u32.to_be_bytes());
    }

    #[test]
    fn attr_prefix_does_not_overlap_neighbouring_attr_ids() {
        let e = ent(1, 1);
        let v = venc(&Value::I64(0));
        for (a, b) in [
            (0u32, 1u32),
            (1, 2),
            (255, 256),
            (u32::MAX - 1, u32::MAX),
            (0, u32::MAX),
        ] {
            let pa = e_attr_prefix(e, AttrId(a));
            let pb = e_attr_prefix(e, AttrId(b));
            let ka = e_fact_key(e, AttrId(a), &v, Tick(0), TxId(1));
            let kb = e_fact_key(e, AttrId(b), &v, Tick(0), TxId(1));
            assert!(!ka.starts_with(&pb), "attr {a} key under prefix of {b}");
            assert!(!kb.starts_with(&pa), "attr {b} key under prefix of {a}");
            assert!(ka.starts_with(&pa) && kb.starts_with(&pb));
            // Even the largest key of the lower attr sorts before the other's prefix.
            let max_a = past_value(&e_value_prefix(e, AttrId(a), &v));
            assert!(max_a < pb, "attr {a} range reaches attr {b}");
        }
    }

    #[test]
    fn attr_prefix_u32_max_is_well_formed() {
        let e = ent(1, 1);
        let p = e_attr_prefix(e, AttrId(u32::MAX));
        assert_eq!(&p[9..13], &[0xFF; 4]);
        let k = e_fact_key(
            e,
            AttrId(u32::MAX),
            &venc(&Value::Bool(true)),
            Tick(5),
            TxId(9),
        );
        assert!(k.starts_with(&p));
    }

    #[test]
    fn attr_prefix_does_not_overlap_neighbouring_entities() {
        let a = AttrId(3);
        let v = venc(&s("x"));
        let pairs = [
            (ent(1, 1), ent(1, 2)),
            (ent(1, 255), ent(1, 256)),
            // last seq of a type vs first seq of the next type
            (ent(1, MAX_SEQ), ent(2, 1)),
            (ent(0, MAX_SEQ), ent(1, 1)),
            (ent(1, 1), ent(32767, MAX_SEQ)),
        ];
        for (x, y) in pairs {
            assert!(x < y);
            let px = e_attr_prefix(x, a);
            let py = e_attr_prefix(y, a);
            let kx = e_fact_key(x, a, &v, Tick(i64::MIN), TxId(0));
            let ky = e_fact_key(y, a, &v, Tick(i64::MAX), TxId(u64::MAX));
            assert!(!kx.starts_with(&py) && !ky.starts_with(&px));
            assert!(kx.starts_with(&px) && ky.starts_with(&py));
            // every possible key of x (for any attr, incl. u32::MAX) sorts before y's keys
            let max_x = past_value(&e_value_prefix(x, AttrId(u32::MAX), &v));
            assert!(max_x < py, "{x:?} range reaches {y:?}");
        }
    }

    #[test]
    fn past_value_skips_exactly_one_values_keys() {
        let e = ent(1, 1);
        let a = AttrId(2);
        // "a" is a byte-prefix of "a\0b" and "ab" before encoding; the escape
        // encoding must keep their key ranges disjoint.
        let vals = [s(""), s("a"), s("a\0b"), s("ab"), s("b"), s("\0")];
        let mut encs: Vec<Vec<u8>> = vals.iter().map(venc).collect();
        encs.sort();
        for (i, v) in encs.iter().enumerate() {
            let prefix = e_value_prefix(e, a, v);
            let past = past_value(&prefix);
            assert!(past.starts_with(&prefix));
            assert_eq!(past.len(), prefix.len() + SUFFIX_LEN);
            // extreme keys of this value are within [prefix, past]
            let lo = e_fact_key(e, a, v, Tick(i64::MAX), TxId(u64::MAX));
            let hi = e_fact_key(e, a, v, Tick(i64::MIN), TxId(0));
            assert!(prefix <= lo && lo <= hi && hi <= past);
            for (j, w) in encs.iter().enumerate() {
                if i == j {
                    continue;
                }
                let k = e_fact_key(e, a, w, Tick(0), TxId(1));
                assert!(
                    !k.starts_with(&prefix),
                    "value {j} key under value {i} prefix"
                );
                if j < i {
                    assert!(k < prefix);
                } else {
                    assert!(k > past, "next value {j} key not past value {i}");
                }
            }
        }
    }

    #[test]
    fn fact_key_is_prefix_plus_inverted_tick_and_tx() {
        let e = ent(1, 1);
        let v = venc(&Value::I64(5));
        let k = e_fact_key(e, AttrId(1), &v, Tick(-3), TxId(4));
        let p = e_value_prefix(e, AttrId(1), &v);
        assert!(k.starts_with(&p));
        assert_eq!(k.len(), p.len() + SUFFIX_LEN);
        assert_eq!(&k[p.len()..p.len() + 8], &Tick(-3).encode_inverted());
        assert_eq!(&k[p.len() + 8..], &(!4u64).to_be_bytes());
        // newer tick sorts first; newer tx at one tick sorts first
        let newer = e_fact_key(e, AttrId(1), &v, Tick(-2), TxId(1));
        assert!(newer < k);
        let newer_tx = e_fact_key(e, AttrId(1), &v, Tick(-3), TxId(5));
        assert!(newer_tx < k);
    }

    #[test]
    fn seek_key_sorts_before_all_entries_at_that_tick_and_after_later_ticks() {
        let e = ent(1, 1);
        let v = venc(&Value::Bool(true));
        let mut seek = e_value_prefix(e, AttrId(1), &v);
        push_seek_tick(&mut seek, Tick(10));
        let at10 = e_fact_key(e, AttrId(1), &v, Tick(10), TxId(u64::MAX));
        let at11 = e_fact_key(e, AttrId(1), &v, Tick(11), TxId(0));
        let at9 = e_fact_key(e, AttrId(1), &v, Tick(9), TxId(7));
        assert!(at11 < seek);
        assert!(seek <= at10);
        assert!(seek < at9);
    }

    #[test]
    fn e_value_bytes_returns_value_part() {
        let e = ent(1, 1);
        for v in [
            Value::Bool(false),
            Value::I64(i64::MIN),
            Value::F64(1.5),
            s(""),
            s("a\0b\0"),
            Value::Ref(ent(2, 3)),
        ] {
            let ve = venc(&v);
            let k = e_fact_key(e, AttrId(9), &ve, Tick(1), TxId(1));
            assert_eq!(e_value_bytes(&k).unwrap(), &ve[..]);
        }
    }

    #[test]
    fn e_value_bytes_rejects_short_key() {
        assert!(e_value_bytes(&[]).is_err());
        assert!(e_value_bytes(b"E").is_err());
        let p = e_attr_prefix(ent(1, 1), AttrId(1));
        assert_eq!(p.len(), E_ATTR_PREFIX_LEN);
        // exactly the prefix: no value at all
        assert!(e_value_bytes(&p).is_err());
        assert!(e_value_bytes(&p[..E_ATTR_PREFIX_LEN - 1]).is_err());
    }

    #[test]
    fn e_value_bytes_rejects_wrong_suffix_length() {
        let e = ent(1, 1);
        let ve = venc(&Value::I64(1));
        let k = e_fact_key(e, AttrId(1), &ve, Tick(1), TxId(1));
        // one byte short, one byte too long, missing entirely
        assert!(e_value_bytes(&k[..k.len() - 1]).is_err());
        let mut long = k.clone();
        long.push(0);
        assert!(e_value_bytes(&long).is_err());
        let no_suffix = e_value_prefix(e, AttrId(1), &ve);
        assert!(e_value_bytes(&no_suffix).is_err());
        // only ^tick, no ^tx
        let mut half = no_suffix.clone();
        push_seek_tick(&mut half, Tick(1));
        assert!(e_value_bytes(&half).is_err());
    }

    #[test]
    fn e_value_bytes_rejects_unterminated_string() {
        let e = ent(1, 1);
        let mut k = e_attr_prefix(e, AttrId(1));
        k.push(0x04); // str tag
        k.extend_from_slice(b"abc"); // no 0x00 0x00 terminator
        k.extend_from_slice(&[0x11; SUFFIX_LEN]);
        assert!(e_value_bytes(&k).is_err());
    }

    #[test]
    fn e_value_bytes_rejects_unknown_tag_and_truncated_fixed_value() {
        let e = ent(1, 1);
        let mut k = e_attr_prefix(e, AttrId(1));
        k.push(0x7F);
        k.extend_from_slice(&[0; 20]);
        assert!(e_value_bytes(&k).is_err());
        // i64 tag with only 3 payload bytes and no suffix
        let mut k = e_attr_prefix(e, AttrId(1));
        k.extend_from_slice(&[0x02, 1, 2, 3]);
        assert!(e_value_bytes(&k).is_err());
    }

    #[test]
    fn e_key_tx_round_trips_with_e_fact_key() {
        let v = venc(&Value::I64(5));
        for tx in [0u64, 1, 42, u64::MAX] {
            for tick in [i64::MIN, -1, 0, i64::MAX] {
                let k = e_fact_key(ent(1, 1), AttrId(3), &v, Tick(tick), TxId(tx));
                assert_eq!(e_key_tx(&k), Some(TxId(tx)));
            }
        }
    }

    #[test]
    fn e_key_tx_is_none_for_too_short_keys() {
        assert_eq!(e_key_tx(&[]), None);
        assert_eq!(e_key_tx(&[1, 2, 3, 4, 5, 6, 7]), None);
        // exactly 8 bytes is "long enough" to read a tx from
        assert_eq!(e_key_tx(&[0xFF; 8]), Some(TxId(0)));
    }

    #[test]
    fn e_value_and_tick_round_trips_value_and_tick() {
        let e = ent(1, 1);
        for v in [
            Value::Bool(true),
            Value::I64(-7),
            Value::F64(2.5),
            s("a\0b"),
            Value::Ref(ent(2, 3)),
        ] {
            let ve = venc(&v);
            for tick in [i64::MIN, -1, 0, i64::MAX] {
                let k = e_fact_key(e, AttrId(4), &ve, Tick(tick), TxId(9));
                let (vb, t) = e_value_and_tick(&k).unwrap();
                assert_eq!(vb, &ve[..]);
                assert_eq!(t, Tick(tick));
            }
        }
    }

    #[test]
    fn e_value_and_tick_rejects_short_key() {
        assert!(e_value_and_tick(&[]).is_err());
        assert!(e_value_and_tick(b"E").is_err());
        let p = e_attr_prefix(ent(1, 1), AttrId(1));
        assert!(e_value_and_tick(&p).is_err());
        assert!(e_value_and_tick(&p[..E_ATTR_PREFIX_LEN - 1]).is_err());
    }

    #[test]
    fn e_value_and_tick_rejects_malformed_value_part() {
        let e = ent(1, 1);
        // unknown tag
        let mut k = e_attr_prefix(e, AttrId(1));
        k.push(0x7F);
        k.extend_from_slice(&[0; 20]);
        assert!(e_value_and_tick(&k).is_err());
        // unterminated string
        let mut k = e_attr_prefix(e, AttrId(1));
        k.push(0x04);
        k.extend_from_slice(b"abc");
        k.extend_from_slice(&[0x11; SUFFIX_LEN]);
        assert!(e_value_and_tick(&k).is_err());
        // truncated fixed-width value
        let mut k = e_attr_prefix(e, AttrId(1));
        k.extend_from_slice(&[0x02, 1, 2, 3]);
        assert!(e_value_and_tick(&k).is_err());
    }

    #[test]
    fn e_value_and_tick_rejects_wrong_suffix_length() {
        let e = ent(1, 1);
        let ve = venc(&Value::I64(1));
        let k = e_fact_key(e, AttrId(1), &ve, Tick(1), TxId(1));
        assert!(e_value_and_tick(&k[..k.len() - 1]).is_err());
        let mut long = k.clone();
        long.push(0);
        assert!(e_value_and_tick(&long).is_err());
        let mut half = e_value_prefix(e, AttrId(1), &ve);
        assert!(e_value_and_tick(&half).is_err());
        push_seek_tick(&mut half, Tick(1));
        assert!(e_value_and_tick(&half).is_err());
    }

    // ---- R keys ----

    fn edge_ent(seq: u64) -> EntityId {
        crate::id::EdgeId::new(seq).expect("valid edge").into()
    }

    #[test]
    fn r_key_layout_and_round_trip() {
        let (t, a, src) = (ent(2, 5), AttrId(9), ent(1, 7));
        let k = r_fact_key(t, a, src, Tick(-5), TxId(42));
        assert_eq!(k.len(), R_KEY_LEN);
        assert_eq!(k[0], b'R');
        assert_eq!(&k[1..9], &t.to_bytes());
        assert_eq!(&k[9..13], &9u32.to_be_bytes());
        assert_eq!(&k[13..21], &src.to_bytes());
        assert_eq!(decode_r_key(&k).unwrap(), (t, a, src, Tick(-5), TxId(42)));
    }

    #[test]
    fn r_key_round_trips_extremes() {
        let ids = [ent(0, 1), ent(1, MAX_SEQ), ent(32767, MAX_SEQ), edge_ent(1)];
        for &t in &ids {
            for &s in &ids {
                for tick in [i64::MIN, -1, 0, 1, i64::MAX] {
                    for (a, tx) in [(0u32, 0u64), (u32::MAX, u64::MAX), (7, 1)] {
                        let k = r_fact_key(t, AttrId(a), s, Tick(tick), TxId(tx));
                        assert_eq!(
                            decode_r_key(&k).unwrap(),
                            (t, AttrId(a), s, Tick(tick), TxId(tx))
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn r_keys_do_not_run_into_neighbouring_targets_attrs_sources() {
        let big = ent(32767, MAX_SEQ);
        let edge = edge_ent(1);
        assert!(big < edge, "edge-tagged ids sort after every entity id");
        let ids = [
            ent(1, 1),
            ent(1, 2),
            ent(1, MAX_SEQ),
            ent(2, 1),
            big,
            edge,
            edge_ent(2),
        ];
        let attrs = [
            AttrId(0),
            AttrId(1),
            AttrId(255),
            AttrId(256),
            AttrId(u32::MAX),
        ];
        // (target, attr, source) triples; lexicographic order must equal key order
        // of the (target, attr, source) prefix, and no prefix may contain another's keys.
        let mut triples = Vec::new();
        for &t in &ids {
            for &a in &attrs {
                for &s in &ids {
                    triples.push((t, a, s));
                }
            }
        }
        let low = |x: &(EntityId, AttrId, EntityId)| {
            r_fact_key(x.0, x.1, x.2, Tick(i64::MAX), TxId(u64::MAX))
        };
        let high =
            |x: &(EntityId, AttrId, EntityId)| r_fact_key(x.0, x.1, x.2, Tick(i64::MIN), TxId(0));
        for x in &triples {
            for y in &triples {
                if x == y {
                    continue;
                }
                let (px, py) = (&low(x)[..21], &low(y)[..21]);
                assert_ne!(px, py);
                assert!(!low(y).starts_with(px) && !high(y).starts_with(px));
                // Disjoint ranges ordered like the triples.
                if x < y {
                    assert!(high(x) < low(y), "{x:?} range reaches {y:?}");
                }
            }
        }
    }

    #[test]
    fn r_key_ticks_sort_newest_first_within_one_triple() {
        let (t, a, s) = (ent(1, 1), AttrId(3), ent(2, 2));
        let ticks = [i64::MIN, -1, 0, 1, i64::MAX];
        let mut keys: Vec<Vec<u8>> = ticks
            .iter()
            .map(|&x| r_fact_key(t, a, s, Tick(x), TxId(1)))
            .collect();
        keys.sort();
        let sorted: Vec<i64> = keys.iter().map(|k| decode_r_key(k).unwrap().3.0).collect();
        assert_eq!(sorted, vec![i64::MAX, 1, 0, -1, i64::MIN]);
    }

    #[test]
    fn r_key_same_tick_sorts_newest_tx_first() {
        let (t, a, s) = (ent(1, 1), AttrId(3), ent(2, 2));
        let k1 = r_fact_key(t, a, s, Tick(0), TxId(1));
        let k9 = r_fact_key(t, a, s, Tick(0), TxId(9));
        let kmax = r_fact_key(t, a, s, Tick(0), TxId(u64::MAX));
        assert!(kmax < k9 && k9 < k1);
    }

    #[test]
    fn decode_r_key_rejects_wrong_length_and_tag() {
        let k = r_fact_key(ent(1, 1), AttrId(1), ent(1, 2), Tick(0), TxId(1));
        assert!(decode_r_key(&k).is_ok());
        assert!(decode_r_key(&[]).is_err());
        assert!(decode_r_key(&k[..k.len() - 1]).is_err());
        let mut long = k.clone();
        long.push(0);
        assert!(decode_r_key(&long).is_err());
        for tag in [b'E', b'V', 0u8, 0xFF] {
            let mut bad = k.clone();
            bad[0] = tag;
            assert!(decode_r_key(&bad).is_err(), "tag {tag}");
        }
    }

    // ---- E key decoding ----

    #[test]
    fn e_key_round_trips() {
        let values = [
            Value::I64(i64::MIN),
            Value::I64(0),
            Value::Bool(true),
            s(""),
            s("Edwin"),
            Value::Ref(ent(3, 4)),
            Value::Ref(edge_ent(9)),
        ];
        for e in [ent(1, 1), ent(32767, MAX_SEQ), edge_ent(1)] {
            for v in &values {
                let enc = venc(v);
                for tick in [i64::MIN, -1, 0, 1, i64::MAX] {
                    for (a, tx) in [(0u32, 0u64), (u32::MAX, u64::MAX), (3, 5)] {
                        let k = e_fact_key(e, AttrId(a), &enc, Tick(tick), TxId(tx));
                        let (de, da, dv, dt, dx) = decode_e_key(&k).unwrap();
                        assert_eq!(
                            (de, da, dv, dt, dx),
                            (e, AttrId(a), &enc[..], Tick(tick), TxId(tx))
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn decode_e_key_rejects_wrong_tag_and_malformed_keys() {
        let enc = venc(&Value::I64(5));
        let k = e_fact_key(ent(1, 1), AttrId(1), &enc, Tick(0), TxId(1));
        assert!(decode_e_key(&k).is_ok());
        for tag in [b'R', b'V', 0u8, 0xFF] {
            let mut bad = k.clone();
            bad[0] = tag;
            assert!(decode_e_key(&bad).is_err(), "tag {tag}");
        }
        assert!(decode_e_key(&[]).is_err());
        assert!(decode_e_key(&k[..E_ATTR_PREFIX_LEN]).is_err());
        assert!(decode_e_key(&k[..k.len() - 1]).is_err());
        let mut long = k.clone();
        long.push(0);
        assert!(decode_e_key(&long).is_err());
    }

    #[test]
    fn e_and_r_decoders_reject_each_others_keys() {
        let r = r_fact_key(ent(1, 1), AttrId(1), ent(1, 2), Tick(0), TxId(1));
        assert!(decode_e_key(&r).is_err());
        let e = e_fact_key(
            ent(1, 1),
            AttrId(1),
            &venc(&Value::Ref(ent(1, 2))),
            Tick(0),
            TxId(1),
        );
        assert!(decode_r_key(&e).is_err());
    }
}
