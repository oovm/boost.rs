//! Records larger than one leaf cell.
//!
//! A `.yydx` record lives in a single slotted leaf cell, and a leaf page holds
//! roughly 4 KiB of payload. A row whose encoded value exceeds that budget used
//! to fail publication outright (`record is too large for one leaf page`), which
//! is a physical limit of the tree, not something a caller can work around by
//! writing smaller values.
//!
//! The codec therefore splits an oversized value into `SPILL_CHUNK_BYTES`
//! records under one reserved record prefix and keeps a short header in the
//! record itself. Chunk keys are derived from the record name, so a rewrite
//! replaces them in place and the record delta machinery deletes the ones a
//! shorter value no longer needs.
//!
//! Reads reassemble before the rows reach the query layer, so VOS sees whole
//! values and every windowed read only has to pull the spill prefix alongside
//! its own key range.

use std::collections::BTreeMap;

use database_types::{Error, Result};

/// Reserved record prefix holding the chunks of oversized values.
///
/// It sorts next to the other reserved record families (`__yydb/v0/object/`)
/// and never collides with row names, which start with `__yydb/v0/row/`.
pub(crate) const SPILL_PREFIX: &str = database_storage::local::SPILLED_PREFIX;

/// Smallest record name above every [`SPILL_PREFIX`] name.
pub(crate) const SPILL_END: &str = database_storage::local::SPILLED_END;

/// Largest value kept whole inside its record cell.
///
/// The same budget the embedded-object chunks use: comfortably below
/// `PAGE_PAYLOAD_LEN` once the cell header, the key and the slot directory are
/// counted.
pub(crate) const SPILL_INLINE_MAX: usize = 2048;

/// Bytes of the original value stored in one chunk record.
const SPILL_CHUNK_BYTES: usize = 2048;

/// Marker at the head of a spilled record's stored value.
///
/// Those bytes cannot start a JSON row, and the header is followed by the
/// original length so assembly knows where the last chunk ends.
const SPILL_MAGIC: &[u8] = b"\x00YYSPILL\x01";

/// Whether `key` belongs to the reserved chunk namespace.
pub(crate) fn is_spill_key(key: &str) -> bool {
    key.starts_with(SPILL_PREFIX)
}

/// Number of chunk records `len` original bytes occupy.
pub(crate) fn chunk_count(len: usize) -> usize {
    if len <= SPILL_INLINE_MAX {
        return 0;
    }
    len.div_ceil(SPILL_CHUNK_BYTES)
}

/// Record name of one chunk of `key`.
pub(crate) fn chunk_key(key: &str, index: usize) -> String {
    format!("{SPILL_PREFIX}{}/{index:06}", hex_encode(key.as_bytes()))
}

/// Split one record value into its stored header and chunk rows.
///
/// A value inside the inline budget is returned unchanged with no chunks, so
/// callers can route every write through this function.
pub(crate) fn encode_one(key: &str, value: &[u8]) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
    if value.len() <= SPILL_INLINE_MAX {
        return (value.to_vec(), Vec::new());
    }
    let mut header = Vec::with_capacity(SPILL_MAGIC.len() + 8);
    header.extend_from_slice(SPILL_MAGIC);
    header.extend((value.len() as u64).to_le_bytes());
    let chunks = value.chunks(SPILL_CHUNK_BYTES).enumerate().map(|(index, chunk)| (chunk_key(key, index), chunk.to_vec())).collect();
    (header, chunks)
}

/// Split every oversized value in `records` into chunk rows.
///
/// Reserved chunk rows are never split again, so calling this twice is a
/// no-op for an already encoded map.
pub(crate) fn encode(records: &mut BTreeMap<String, Vec<u8>>) {
    let keys: Vec<String> =
        records.iter().filter(|(key, value)| !is_spill_key(key) && value.len() > SPILL_INLINE_MAX).map(|(key, _)| key.clone()).collect();
    for key in keys {
        let Some(value) = records.remove(&key)
        else {
            continue;
        };
        let (header, chunks) = encode_one(&key, &value);
        records.insert(key, header);
        records.extend(chunks);
    }
}

/// Rebuild every spilled value from its chunk rows and drop the chunk rows.
///
/// A chunk set without its record (a record deleted in an earlier session) is
/// discarded: the record map is the authority on which rows exist.
pub(crate) fn assemble(records: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    let spill_keys: Vec<String> = records.keys().filter(|key| is_spill_key(key)).cloned().collect();
    if spill_keys.is_empty() {
        return Ok(());
    }
    let mut groups: BTreeMap<String, BTreeMap<usize, Vec<u8>>> = BTreeMap::new();
    for spill_key in spill_keys {
        let Some(bytes) = records.remove(&spill_key)
        else {
            continue;
        };
        let rest = &spill_key[SPILL_PREFIX.len()..];
        let (encoded, index) = rest.rsplit_once('/').ok_or_else(|| Error::Corrupt("spill chunk key is malformed"))?;
        let key = String::from_utf8(hex_decode(encoded)?).map_err(|_| Error::Corrupt("spill chunk owner is not UTF-8"))?;
        let index: usize = index.parse().map_err(|_| Error::Corrupt("spill chunk index is not a number"))?;
        groups.entry(key).or_default().insert(index, bytes);
    }
    for (key, chunks) in groups {
        let Some(header) = records.get(&key)
        else {
            continue;
        };
        let Some(expected) = header_len(header)
        else {
            // The record no longer declares a spilled value, so any chunk rows
            // under it are leftovers from a shorter rewrite or a failed commit.
            continue;
        };
        let mut value = Vec::with_capacity(expected);
        for chunk in chunks.values() {
            if value.len() >= expected {
                break;
            }
            value.extend_from_slice(chunk);
        }
        if value.len() < expected {
            return Err(Error::Corrupt("spilled record is truncated"));
        }
        value.truncate(expected);
        records.insert(key, value);
    }
    Ok(())
}

/// Original length declared by a spilled record header, if this is one.
fn header_len(value: &[u8]) -> Option<usize> {
    let rest = value.strip_prefix(SPILL_MAGIC)?;
    let bytes: [u8; 8] = rest.get(..8)?.try_into().ok()?;
    usize::try_from(u64::from_le_bytes(bytes)).ok().filter(|len| *len > SPILL_INLINE_MAX)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode(text: &str) -> Result<Vec<u8>> {
    if text.len() % 2 != 0 {
        return Err(Error::Corrupt("spill chunk owner is not hex"));
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let high = (pair[0] as char).to_digit(16).ok_or_else(|| Error::Corrupt("spill chunk owner is not hex"))?;
        let low = (pair[1] as char).to_digit(16).ok_or_else(|| Error::Corrupt("spill chunk owner is not hex"))?;
        out.push(((high << 4) | low) as u8);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_value_stays_whole() {
        let (header, chunks) = encode_one("row/a", b"small");
        assert_eq!(header, b"small");
        assert!(chunks.is_empty());
        assert_eq!(chunk_count(5), 0);
    }

    #[test]
    fn a_large_value_round_trips_through_chunks() {
        let value: Vec<u8> = (0..5_000).map(|index| (index % 251) as u8).collect();
        let mut records = BTreeMap::new();
        records.insert("row/a".to_string(), value.clone());
        encode(&mut records);
        assert_eq!(records.len(), 1 + chunk_count(value.len()), "one header row plus chunks");
        assert!(records["row/a"].starts_with(SPILL_MAGIC));
        assemble(&mut records).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records["row/a"], value);
    }

    #[test]
    fn shrinking_drops_stale_chunks_and_reassembly_uses_the_header() {
        let mut records = BTreeMap::new();
        records.insert("row/a".to_string(), vec![b'x'; 9_000]);
        encode(&mut records);
        // The record now holds a short value, but the old chunk rows are still
        // in the map: the header decides how much of them counts.
        records.insert("row/a".to_string(), b"short".to_vec());
        assemble(&mut records).unwrap();
        assert_eq!(records["row/a"], b"short".to_vec());
        assert_eq!(records.len(), 1, "chunk rows are removed from the record map");
    }

    #[test]
    fn a_deleted_record_discards_its_chunks() {
        let mut records = BTreeMap::new();
        records.insert("row/a".to_string(), vec![b'x'; 4_000]);
        encode(&mut records);
        records.remove("row/a");
        assemble(&mut records).unwrap();
        assert!(records.is_empty());
    }

    #[test]
    fn a_truncated_chunk_set_is_corrupt() {
        let mut records = BTreeMap::new();
        records.insert("row/a".to_string(), vec![b'x'; 5_000]);
        encode(&mut records);
        let last = chunk_key("row/a", chunk_count(5_000) - 1);
        records.remove(&last);
        assert!(matches!(assemble(&mut records), Err(Error::Corrupt(_))));
    }

    #[test]
    fn chunk_keys_never_collide_with_row_names() {
        assert!(is_spill_key(&chunk_key("row/a", 0)));
        assert!(!is_spill_key("__yydb/v0/row/a"));
        assert!(SPILL_END > SPILL_PREFIX);
    }
}
