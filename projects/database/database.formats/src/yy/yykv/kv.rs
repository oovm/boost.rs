//! `.yykv` shard codec: a base snapshot plus an append-only delta area.
//!
//! A shard file is `magic | base_len | base checksum | base body | delta frames`. The base
//! body is the version 3 snapshot layout (`next_revision`, record count, records) and is the
//! compaction target: rewriting it collapses every superseded version of a key. Each commit
//! after that appends one checksummed delta frame, so the durable write of a commit is the
//! changed bytes only — never the whole shard.
//!
//! Version 3 files (whole-file rewrite per commit, no delta area) still decode and are
//! upgraded to version 4 when a writer opens them. Version 2 has no checksum and is rejected
//! explicitly.

use std::{
    collections::{BTreeMap, HashSet},
    io::{Cursor, Read, Write},
};

use database_types::{
    ReasonCode,
    cluster::{Error, Result},
};

use super::{InlineValue, Key, ObjectRef, Record, StoredValue};

/// `.yykv` file magic including the format version.
pub const MAGIC: &[u8] = b"YYKV\x04";
/// Version 3 magic: a base snapshot with no delta area.
pub const PREVIOUS_MAGIC: &[u8] = b"YYKV\x03";
/// Legacy version 2 magic (no checksum) — open must refuse and ask for recreate.
pub const LEGACY_MAGIC: &[u8] = b"YYKV\x02";

/// Version 4 file header: magic + base section length `u64` + base checksum `u64`.
pub const FILE_HEADER_BYTES: usize = MAGIC.len() + 8 + 8;
/// Version 3 file header: magic + body checksum `u64`.
const V3_FILE_HEADER_BYTES: usize = PREVIOUS_MAGIC.len() + 8;
/// Base body header: `next_revision u64` + record count `u32`.
pub const BASE_HEADER_BYTES: usize = 12;
/// Delta frame header: payload length `u32` + payload checksum `u64`.
pub const FRAME_HEADER_BYTES: usize = 12;
/// Largest delta frame accepted from disk.
///
/// The limit keeps a corrupt length field from being read as a torn tail and bounds the
/// memory one frame can allocate during recovery.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Decoded snapshot body (no path / IO).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Next revision to allocate.
    pub next_revision: u64,
    /// Records keyed for O(log n) lookup.
    pub records: BTreeMap<Key, Record>,
}

/// One decoded shard file: base snapshot plus the delta frames replayed on top of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardImage {
    /// Next revision to allocate after replaying every delta frame.
    pub next_revision: u64,
    /// Live records after replaying every delta frame.
    pub records: BTreeMap<Key, Record>,
    /// Byte length of the base section, header included.
    pub base_len: usize,
    /// Byte length of the complete delta frames after the base section.
    pub delta_len: usize,
    /// Whether the file ends with an incomplete delta frame.
    ///
    /// An incomplete tail is an interrupted append, not corruption: no commit boundary was
    /// crossed, so the shard owner truncates it back to `base_len + delta_len`.
    pub torn_tail: bool,
}

/// One mutation stored inside a delta frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaOp {
    /// Insert or replace a record.
    Put(Record),
    /// Remove a key.
    Delete(Key),
}

/// Encode a record map as a version 4 shard file with an empty delta area.
///
/// `next_revision` is the next revision to allocate, following the PostgreSQL shard
/// contract: an empty shard starts at `1`, allocated revisions are strictly below it, and
/// the writer bumps it after every allocation.
pub fn encode_snapshot(records: &BTreeMap<Key, Record>, next_revision: u64) -> Result<Vec<u8>> {
    let body = encode_body(records, next_revision)?;
    let base_len = FILE_HEADER_BYTES + body.len();
    let mut bytes = Vec::with_capacity(base_len);
    bytes.write_all(MAGIC)?;
    bytes.write_all(&(body.len() as u64).to_le_bytes())?;
    bytes.write_all(&checksum(&body).to_le_bytes())?;
    bytes.write_all(&body)?;
    Ok(bytes)
}

/// Encode one delta frame: the post-commit `next_revision` plus the ops it carries.
pub fn encode_delta_frame(next_revision: u64, ops: &[DeltaOp]) -> Result<Vec<u8>> {
    if next_revision == 0 {
        return Err(Error::Unsupported("yykv next_revision must be at least 1"));
    }
    let mut payload = Vec::new();
    payload.write_all(&next_revision.to_le_bytes())?;
    write_count(&mut payload, ops.len())?;
    for op in ops {
        match op {
            DeltaOp::Put(record) => {
                if record.revision == 0 || record.revision >= next_revision {
                    return Err(Error::Unsupported("yykv revision must be below next_revision"));
                }
                payload.write_all(&[0])?;
                write_key(&mut payload, &record.key)?;
                payload.write_all(&record.revision.to_le_bytes())?;
                let encoded_expiry = record.expires_at.map(|deadline| deadline as u64).unwrap_or(0);
                payload.write_all(&encoded_expiry.to_le_bytes())?;
                write_value(&mut payload, &record.value)?;
            }
            DeltaOp::Delete(key) => {
                payload.write_all(&[1])?;
                write_key(&mut payload, key)?;
            }
        }
    }
    let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + payload.len());
    frame.write_all(&u32::try_from(payload.len()).map_err(|_| Error::Unsupported("yykv delta frame too large"))?.to_le_bytes())?;
    frame.write_all(&checksum(&payload).to_le_bytes())?;
    frame.write_all(&payload)?;
    Ok(frame)
}

/// Decode `.yykv` bytes into a [`Snapshot`], accepting legacy revision layouts.
///
/// Use [`decode_snapshot_strict`] when the caller must reject pre-contract shards instead
/// of normalizing them.
pub fn decode_snapshot(bytes: &[u8]) -> Result<Snapshot> {
    decode_with(bytes, RevisionMode::LegacyTolerant, ValueMode::Any)
        .map(|image| Snapshot { next_revision: image.next_revision, records: image.records })
}

/// Decode `.yykv` bytes while enforcing the PostgreSQL revision contract.
///
/// Rejects legacy v2 magic, an empty shard recorded as revision `0`, and any record sitting
/// on `next_revision`. Corruption carries [`ReasonCode`] so a host can branch on the failure
/// class without parsing text.
pub fn decode_snapshot_strict(bytes: &[u8]) -> Result<Snapshot> {
    decode_with(bytes, RevisionMode::Contract, ValueMode::Any)
        .map(|image| Snapshot { next_revision: image.next_revision, records: image.records })
}

/// Strict decode that also rejects object-backed values.
///
/// Use this on a node that has no object store bound: an inline-only reader must not
/// silently treat a hash reference as row data.
pub fn decode_snapshot_inline_only(bytes: &[u8]) -> Result<Snapshot> {
    decode_with(bytes, RevisionMode::Contract, ValueMode::InlineOnly)
        .map(|image| Snapshot { next_revision: image.next_revision, records: image.records })
}

/// Decode a shard file with its base layout and delta bookkeeping kept.
///
/// This is what a shard owner uses: [`ShardImage::base_len`] and [`ShardImage::delta_len`]
/// locate the append offset, and [`ShardImage::torn_tail`] says whether the file must be
/// truncated before the next commit.
pub fn decode_shard_image(bytes: &[u8]) -> Result<ShardImage> {
    decode_with(bytes, RevisionMode::Contract, ValueMode::Any)
}

#[derive(Clone, Copy)]
enum RevisionMode {
    Contract,
    LegacyTolerant,
}

#[derive(Clone, Copy)]
enum ValueMode {
    Any,
    InlineOnly,
}

fn corrupt(reason: ReasonCode, message: &'static str) -> Error {
    Error::CorruptReason { reason, message }
}

fn decode_with(bytes: &[u8], revision_mode: RevisionMode, value_mode: ValueMode) -> Result<ShardImage> {
    if bytes.len() < MAGIC.len() {
        return Err(corrupt(ReasonCode::Truncated, "truncated yykv header"));
    }
    if bytes.starts_with(LEGACY_MAGIC) {
        return Err(corrupt(
            ReasonCode::LegacyVersion,
            "legacy yykv v2 snapshot has no checksum: delete the shard file and let the node recreate it",
        ));
    }
    if bytes.starts_with(PREVIOUS_MAGIC) {
        // Version 3: the whole file is the base section, with no delta area.
        if bytes.len() < V3_FILE_HEADER_BYTES {
            return Err(corrupt(ReasonCode::Truncated, "truncated yykv checksum"));
        }
        let expected_checksum = read_u64_at(bytes, PREVIOUS_MAGIC.len())?;
        let body = &bytes[V3_FILE_HEADER_BYTES..];
        if checksum(body) != expected_checksum {
            return Err(corrupt(ReasonCode::ChecksumMismatch, "yykv checksum mismatch"));
        }
        let snapshot = decode_body(body, revision_mode, value_mode)?;
        return Ok(ShardImage {
            next_revision: snapshot.next_revision,
            records: snapshot.records,
            base_len: bytes.len(),
            delta_len: 0,
            torn_tail: false,
        });
    }
    if !bytes.starts_with(MAGIC) {
        return Err(corrupt(ReasonCode::BadMagic, "unknown yykv header"));
    }
    if bytes.len() < FILE_HEADER_BYTES {
        return Err(corrupt(ReasonCode::Truncated, "truncated yykv base length"));
    }
    let base_body_len = read_u64_at(bytes, MAGIC.len())? as usize;
    let expected_checksum = read_u64_at(bytes, MAGIC.len() + 8)?;
    let body_start = FILE_HEADER_BYTES;
    let body_end = body_start.checked_add(base_body_len).ok_or_else(|| corrupt(ReasonCode::Truncated, "truncated yykv base section"))?;
    let body = bytes.get(body_start..body_end).ok_or_else(|| corrupt(ReasonCode::Truncated, "truncated yykv base section"))?;
    if checksum(body) != expected_checksum {
        return Err(corrupt(ReasonCode::ChecksumMismatch, "yykv checksum mismatch"));
    }
    let mut snapshot = decode_body(body, revision_mode, value_mode)?;
    let replay = replay_frames(bytes, body_end, &mut snapshot, value_mode)?;
    Ok(ShardImage {
        next_revision: snapshot.next_revision,
        records: snapshot.records,
        base_len: body_end,
        delta_len: replay.delta_len,
        torn_tail: replay.torn_tail,
    })
}

struct Replay {
    delta_len: usize,
    torn_tail: bool,
}

/// Replays every complete delta frame after `offset` onto `snapshot`.
///
/// Stops at an incomplete trailing frame and reports it instead of failing: an interrupted
/// append never crossed a commit boundary. A complete frame that fails its checksum or
/// declares an implausible length is corruption and is returned as an error.
fn replay_frames(bytes: &[u8], offset: usize, snapshot: &mut Snapshot, value_mode: ValueMode) -> Result<Replay> {
    let mut cursor = offset;
    let mut frames_len = 0;
    while cursor < bytes.len() {
        let available = bytes.len() - cursor;
        if available < FRAME_HEADER_BYTES {
            return Ok(Replay { delta_len: frames_len, torn_tail: true });
        }
        let payload_len = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
        // A frame always carries `next_revision` + op count, and a length beyond the frame
        // cap cannot be an interrupted append of a real commit.
        if payload_len < BASE_HEADER_BYTES || payload_len > MAX_FRAME_BYTES {
            return Err(corrupt(ReasonCode::UnknownTag, "yykv delta frame length out of range"));
        }
        let payload_start = cursor + FRAME_HEADER_BYTES;
        let payload_end = payload_start + payload_len;
        if payload_end > bytes.len() {
            return Ok(Replay { delta_len: frames_len, torn_tail: true });
        }
        let expected_checksum = read_u64_at(bytes, cursor + 4)?;
        let payload = &bytes[payload_start..payload_end];
        if checksum(payload) != expected_checksum {
            return Err(corrupt(ReasonCode::ChecksumMismatch, "yykv delta checksum mismatch"));
        }
        let frame = decode_frame_payload(payload, value_mode)?;
        if frame.next_revision < snapshot.next_revision {
            return Err(corrupt(ReasonCode::UnknownTag, "yykv delta revisions out of order"));
        }
        for op in frame.ops {
            match op {
                DeltaOp::Put(record) => {
                    snapshot.records.insert(record.key.clone(), record);
                }
                DeltaOp::Delete(key) => {
                    snapshot.records.remove(&key);
                }
            }
        }
        snapshot.next_revision = frame.next_revision;
        frames_len += FRAME_HEADER_BYTES + payload_len;
        cursor = payload_end;
    }
    Ok(Replay { delta_len: frames_len, torn_tail: false })
}

struct DecodedFrame {
    next_revision: u64,
    ops: Vec<DeltaOp>,
}

fn decode_frame_payload(payload: &[u8], value_mode: ValueMode) -> Result<DecodedFrame> {
    let mut cursor = Cursor::new(payload);
    let next_revision = read_u64(&mut cursor)?;
    if next_revision == 0 {
        return Err(corrupt(ReasonCode::LegacyVersion, "legacy yykv revision layout: next_revision is zero"));
    }
    let count = read_u32(&mut cursor)? as usize;
    // Every op costs at least a tag byte, so a count beyond the remaining bytes is corrupt.
    if count > payload.len().saturating_sub(BASE_HEADER_BYTES) {
        return Err(corrupt(ReasonCode::UnknownTag, "yykv delta op count out of range"));
    }
    let mut ops = Vec::with_capacity(count);
    for _ in 0..count {
        match read_u8(&mut cursor)? {
            0 => {
                let key = read_key(&mut cursor)?;
                let revision = read_u64(&mut cursor)?;
                if revision == 0 || revision >= next_revision {
                    return Err(Error::Corrupt("invalid shard revision"));
                }
                let encoded_expiry = read_u64(&mut cursor)?;
                let expires_at = (encoded_expiry != 0).then_some(encoded_expiry as i64);
                let value = read_value(&mut cursor, value_mode)?;
                ops.push(DeltaOp::Put(Record { key, value, revision, expires_at }));
            }
            1 => ops.push(DeltaOp::Delete(read_key(&mut cursor)?)),
            _ => return Err(corrupt(ReasonCode::UnknownTag, "unknown yykv delta op")),
        }
    }
    if cursor.position() != payload.len() as u64 {
        return Err(corrupt(ReasonCode::TrailingBytes, "trailing yykv delta bytes"));
    }
    Ok(DecodedFrame { next_revision, ops })
}

/// Decodes a version 3 base body.
fn decode_body(body: &[u8], revision_mode: RevisionMode, value_mode: ValueMode) -> Result<Snapshot> {
    let mut cursor = Cursor::new(body);
    let mut next_revision = read_u64(&mut cursor)?;
    if next_revision == 0 && matches!(revision_mode, RevisionMode::Contract) {
        return Err(corrupt(ReasonCode::LegacyVersion, "legacy yykv revision layout: next_revision is zero"));
    }
    let count = read_u32(&mut cursor)? as usize;
    let mut records = BTreeMap::new();
    let mut revisions = HashSet::with_capacity(count);
    let mut legacy_revision = false;
    for _ in 0..count {
        let key = read_key(&mut cursor)?;
        let revision = read_u64(&mut cursor)?;
        if revision == 0 || revision > next_revision || !revisions.insert(revision) {
            return Err(Error::Corrupt("invalid shard revision"));
        }
        // Shards written before the PostgreSQL contract used `next_revision` as the last
        // allocated revision, so its newest record can match it. Normalize that on read.
        legacy_revision |= revision == next_revision;
        if legacy_revision && matches!(revision_mode, RevisionMode::Contract) {
            return Err(corrupt(ReasonCode::LegacyVersion, "legacy yykv revision layout: record sits on next_revision"));
        }
        let encoded_expiry = read_u64(&mut cursor)?;
        let expires_at = (encoded_expiry != 0).then_some(encoded_expiry as i64);
        let value = read_value(&mut cursor, value_mode)?;
        let record = Record { key: key.clone(), value, revision, expires_at };
        if records.insert(key, record).is_some() {
            return Err(corrupt(ReasonCode::DuplicateKey, "duplicate shard key"));
        }
    }
    if cursor.position() != body.len() as u64 {
        return Err(corrupt(ReasonCode::TrailingBytes, "trailing shard bytes"));
    }
    if next_revision == 0 {
        // Legacy empty shard: the next allocation was stored as `0`.
        next_revision = 1;
    }
    else if legacy_revision {
        next_revision = next_revision.checked_add(1).ok_or(Error::Corrupt("legacy shard revision space exhausted"))?;
    }
    Ok(Snapshot { next_revision, records })
}

fn encode_body(records: &BTreeMap<Key, Record>, next_revision: u64) -> Result<Vec<u8>> {
    if next_revision == 0 {
        return Err(Error::Unsupported("yykv next_revision must be at least 1"));
    }
    if records.values().any(|record| record.revision == 0 || record.revision >= next_revision) {
        return Err(Error::Unsupported("yykv revision must be below next_revision"));
    }
    let mut body = Vec::new();
    body.write_all(&next_revision.to_le_bytes())?;
    write_count(&mut body, records.len())?;
    for record in records.values() {
        write_key(&mut body, &record.key)?;
        body.write_all(&record.revision.to_le_bytes())?;
        let encoded_expiry = record.expires_at.map(|deadline| deadline as u64).unwrap_or(0);
        body.write_all(&encoded_expiry.to_le_bytes())?;
        write_value(&mut body, &record.value)?;
    }
    Ok(body)
}

fn write_key(bytes: &mut Vec<u8>, key: &Key) -> Result<()> {
    write_bytes(bytes, key.namespace.0.as_bytes())?;
    write_bytes(bytes, &key.bytes)
}

fn read_key(cursor: &mut Cursor<&[u8]>) -> Result<Key> {
    let namespace = String::from_utf8(read_bytes(cursor)?).map_err(|_| corrupt(ReasonCode::UnknownTag, "shard namespace utf8"))?;
    let key = read_bytes(cursor)?;
    Ok(Key::new(namespace, key))
}

fn write_value(bytes: &mut Vec<u8>, value: &StoredValue) -> Result<()> {
    match value {
        StoredValue::Inline(value) => {
            bytes.write_all(&[0])?;
            write_bytes(bytes, &value.0)?;
        }
        StoredValue::Object(value) => {
            bytes.write_all(&[1])?;
            write_bytes(bytes, value.hash_hex.as_bytes())?;
        }
    }
    Ok(())
}

fn read_value(cursor: &mut Cursor<&[u8]>, value_mode: ValueMode) -> Result<StoredValue> {
    match read_u8(cursor)? {
        0 => Ok(StoredValue::Inline(InlineValue(read_bytes(cursor)?))),
        1 => {
            if matches!(value_mode, ValueMode::InlineOnly) {
                return Err(Error::Unsupported("yykv object-backed value requires an object store"));
            }
            Ok(StoredValue::Object(ObjectRef {
                hash_hex: String::from_utf8(read_bytes(cursor)?).map_err(|_| corrupt(ReasonCode::UnknownTag, "shard object hash utf8"))?,
            }))
        }
        _ => Err(corrupt(ReasonCode::UnknownTag, "invalid shard value kind")),
    }
}

/// FNV-1a over a body or frame payload (integrity, not security).
pub fn checksum(body: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in body {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn write_count(bytes: &mut Vec<u8>, count: usize) -> Result<()> {
    let count = u32::try_from(count).map_err(|_| Error::Unsupported("too many shard records"))?;
    bytes.write_all(&count.to_le_bytes())?;
    Ok(())
}

fn write_bytes(bytes: &mut Vec<u8>, value: &[u8]) -> Result<()> {
    let length = u32::try_from(value.len()).map_err(|_| Error::Unsupported("shard value too large"))?;
    bytes.write_all(&length.to_le_bytes())?;
    bytes.write_all(value)?;
    Ok(())
}

fn read_u64_at(bytes: &[u8], offset: usize) -> Result<u64> {
    let end = offset + 8;
    let slice = bytes.get(offset..end).ok_or_else(|| corrupt(ReasonCode::Truncated, "unexpected shard eof"))?;
    Ok(u64::from_le_bytes(slice.try_into().unwrap()))
}

fn read_u8(cursor: &mut Cursor<&[u8]>) -> Result<u8> {
    let mut value = [0_u8; 1];
    read_exact(cursor, &mut value)?;
    Ok(value[0])
}

fn read_u32(cursor: &mut Cursor<&[u8]>) -> Result<u32> {
    let mut value = [0_u8; 4];
    read_exact(cursor, &mut value)?;
    Ok(u32::from_le_bytes(value))
}

fn read_u64(cursor: &mut Cursor<&[u8]>) -> Result<u64> {
    let mut value = [0_u8; 8];
    read_exact(cursor, &mut value)?;
    Ok(u64::from_le_bytes(value))
}

fn read_bytes(cursor: &mut Cursor<&[u8]>) -> Result<Vec<u8>> {
    let length = read_u32(cursor)? as usize;
    let position = cursor.position() as usize;
    let end = position.checked_add(length).ok_or_else(|| corrupt(ReasonCode::Truncated, "shard length overflow"))?;
    if end > cursor.get_ref().len() {
        return Err(corrupt(ReasonCode::Truncated, "unexpected shard eof"));
    }
    let mut value = vec![0_u8; length];
    read_exact(cursor, &mut value)?;
    Ok(value)
}

fn read_exact(cursor: &mut Cursor<&[u8]>, bytes: &mut [u8]) -> Result<()> {
    cursor.read_exact(bytes).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            corrupt(ReasonCode::Truncated, "unexpected shard eof")
        }
        else {
            Error::Io(error)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wraps a version 3 body as a complete version 3 file.
    fn wrap_v3(body: Vec<u8>) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(PREVIOUS_MAGIC);
        bytes.extend_from_slice(&checksum(&body).to_le_bytes());
        bytes.extend_from_slice(&body);
        bytes
    }

    /// Wraps a version 3 body as the base section of a version 4 file.
    fn wrap_v4(body: Vec<u8>, delta: Vec<u8>) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&(body.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&checksum(&body).to_le_bytes());
        bytes.extend_from_slice(&body);
        bytes.extend_from_slice(&delta);
        bytes
    }

    fn legacy_body(next_revision: u64, revision: Option<u64>) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&next_revision.to_le_bytes());
        write_count(&mut body, usize::from(revision.is_some())).expect("count");
        if let Some(revision) = revision {
            let key = Key::new("ns", b"k".to_vec());
            write_bytes(&mut body, key.namespace.0.as_bytes()).expect("namespace");
            write_bytes(&mut body, &key.bytes).expect("key");
            body.extend_from_slice(&revision.to_le_bytes());
            body.extend_from_slice(&0_u64.to_le_bytes());
            body.push(0);
            write_bytes(&mut body, b"v").expect("value");
        }
        body
    }

    fn record(key: &[u8], value: &[u8], revision: u64) -> Record {
        Record { key: Key::new("ns", key.to_vec()), value: StoredValue::Inline(InlineValue(value.to_vec())), revision, expires_at: None }
    }

    #[test]
    fn legacy_empty_shard_zero_becomes_one() {
        let snapshot = decode_snapshot(&wrap_v3(legacy_body(0, None))).expect("decode");
        assert_eq!(snapshot.next_revision, 1);
    }

    #[test]
    fn legacy_last_allocated_revision_is_bumped() {
        let snapshot = decode_snapshot(&wrap_v3(legacy_body(1, Some(1)))).expect("decode");
        assert_eq!(snapshot.next_revision, 2);
    }

    #[test]
    fn new_format_next_revision_is_kept() {
        let snapshot = decode_snapshot(&wrap_v3(legacy_body(2, Some(1)))).expect("decode");
        assert_eq!(snapshot.next_revision, 2);
    }

    #[test]
    fn version_three_files_decode_as_a_base_only_image() {
        let bytes = wrap_v3(legacy_body(2, Some(1)));
        let image = decode_shard_image(&bytes).expect("decode");
        assert_eq!(image.base_len, bytes.len());
        assert_eq!(image.delta_len, 0);
        assert!(!image.torn_tail);
    }

    #[test]
    fn delta_frames_replay_on_top_of_the_base() {
        let mut records = BTreeMap::new();
        let first = record(b"a", b"1", 1);
        records.insert(first.key.clone(), first.clone());
        let base = encode_snapshot(&records, 2).expect("encode");
        let mut bytes = base.clone();
        bytes.extend_from_slice(&encode_delta_frame(3, &[DeltaOp::Put(record(b"b", b"2", 2))]).expect("frame"));
        bytes.extend_from_slice(&encode_delta_frame(3, &[DeltaOp::Delete(first.key.clone())]).expect("frame"));

        let image = decode_shard_image(&bytes).expect("decode");
        assert_eq!(image.next_revision, 3);
        assert_eq!(image.records.len(), 1);
        assert_eq!(image.records[&Key::new("ns", b"b".to_vec())].value, StoredValue::Inline(InlineValue(b"2".to_vec())));
        assert!(image.delta_len > 0);
    }

    #[test]
    fn interrupted_delta_tail_is_reported_not_rejected() {
        let base = encode_snapshot(&BTreeMap::new(), 1).expect("encode");
        let frame = encode_delta_frame(2, &[DeltaOp::Put(record(b"k", b"v", 1))]).expect("frame");
        let mut bytes = base;
        bytes.extend_from_slice(&frame[..frame.len() - 3]);
        let image = decode_shard_image(&bytes).expect("decode");
        assert!(image.torn_tail);
        assert_eq!(image.delta_len, 0);
        assert!(image.records.is_empty());
    }

    #[test]
    fn complete_frame_with_a_bad_checksum_is_corruption() {
        let base = encode_snapshot(&BTreeMap::new(), 1).expect("encode");
        let mut frame = encode_delta_frame(2, &[DeltaOp::Put(record(b"k", b"v", 1))]).expect("frame");
        let last = frame.len() - 1;
        frame[last] ^= 0xff;
        let mut bytes = base;
        bytes.extend_from_slice(&frame);
        assert!(matches!(decode_shard_image(&bytes), Err(Error::CorruptReason { reason: ReasonCode::ChecksumMismatch, .. })));
    }

    #[test]
    fn delta_revisions_must_not_move_backwards() {
        let base = encode_snapshot(&BTreeMap::new(), 4).expect("encode");
        let mut bytes = base;
        bytes.extend_from_slice(&encode_delta_frame(4, &[DeltaOp::Delete(Key::new("ns", b"k".to_vec()))]).expect("frame"));
        assert!(decode_shard_image(&bytes).is_ok());
        bytes.extend_from_slice(&encode_delta_frame(3, &[DeltaOp::Delete(Key::new("ns", b"k".to_vec()))]).expect("frame"));
        assert!(decode_shard_image(&bytes).is_err());
    }

    fn push_record(body: &mut Vec<u8>, namespace: &str, key: &[u8], revision: u64, tag: u8, value: &[u8]) {
        write_bytes(body, namespace.as_bytes()).expect("namespace");
        write_bytes(body, key).expect("key");
        body.extend_from_slice(&revision.to_le_bytes());
        body.extend_from_slice(&0_u64.to_le_bytes());
        body.push(tag);
        if tag == 0 || tag == 1 {
            write_bytes(body, value).expect("value");
        }
    }

    fn reason_of(bytes: &[u8]) -> ReasonCode {
        match decode_snapshot_strict(bytes) {
            Err(error) => error.reason().expect("decoder must classify the failure"),
            Ok(_) => panic!("strict decode must reject corrupted bytes"),
        }
    }

    #[test]
    fn strict_decode_rejects_legacy_layout() {
        assert_eq!(reason_of(&wrap_v3(legacy_body(0, None))), ReasonCode::LegacyVersion);
        assert_eq!(reason_of(&wrap_v3(legacy_body(1, Some(1)))), ReasonCode::LegacyVersion);
        assert_eq!(reason_of(&wrap_v4(legacy_body(0, None), Vec::new())), ReasonCode::LegacyVersion);
    }

    #[test]
    fn strict_decode_classifies_bad_magic() {
        assert_eq!(reason_of(b"XXXX\x03not-a-shard"), ReasonCode::BadMagic);
    }

    #[test]
    fn strict_decode_classifies_legacy_magic() {
        let mut bytes = LEGACY_MAGIC.to_vec();
        bytes.extend_from_slice(&[0; 8]);
        assert_eq!(reason_of(&bytes), ReasonCode::LegacyVersion);
    }

    #[test]
    fn strict_decode_classifies_truncation() {
        assert_eq!(reason_of(MAGIC), ReasonCode::Truncated);
        let mut body = Vec::new();
        body.extend_from_slice(&2_u64.to_le_bytes());
        assert_eq!(reason_of(&wrap_v3(body)), ReasonCode::Truncated);
    }

    #[test]
    fn strict_decode_classifies_a_checksum_mismatch() {
        let mut bytes = wrap_v3(legacy_body(2, None));
        let body_start = PREVIOUS_MAGIC.len() + 8;
        bytes[body_start] ^= 0xff;
        assert_eq!(reason_of(&bytes), ReasonCode::ChecksumMismatch);
    }

    #[test]
    fn strict_decode_classifies_unknown_tags() {
        let mut body = Vec::new();
        body.extend_from_slice(&2_u64.to_le_bytes());
        write_count(&mut body, 1).expect("count");
        push_record(&mut body, "ns", b"k", 1, 9, b"");
        assert_eq!(reason_of(&wrap_v3(body)), ReasonCode::UnknownTag);
    }

    #[test]
    fn strict_decode_classifies_duplicate_keys() {
        let mut body = Vec::new();
        body.extend_from_slice(&3_u64.to_le_bytes());
        write_count(&mut body, 2).expect("count");
        push_record(&mut body, "ns", b"k", 1, 0, b"a");
        push_record(&mut body, "ns", b"k", 2, 0, b"b");
        assert_eq!(reason_of(&wrap_v3(body)), ReasonCode::DuplicateKey);
    }

    #[test]
    fn strict_decode_classifies_trailing_bytes() {
        let mut body = legacy_body(2, None);
        body.push(0);
        assert_eq!(reason_of(&wrap_v3(body)), ReasonCode::TrailingBytes);
    }

    #[test]
    fn inline_only_decode_rejects_object_values() {
        let mut body = Vec::new();
        body.extend_from_slice(&2_u64.to_le_bytes());
        write_count(&mut body, 1).expect("count");
        push_record(&mut body, "ns", b"k", 1, 1, b"hash");
        assert!(matches!(decode_snapshot_inline_only(&wrap_v3(body)), Err(Error::Unsupported(_))));
    }
}
