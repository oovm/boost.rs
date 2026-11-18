//! `.yykv` 分片快照编解码与记录模型。

mod key;
mod kv;
mod record;
mod value;

pub use key::Key;
pub use kv::{
    BASE_HEADER_BYTES, DeltaOp, FILE_HEADER_BYTES, FRAME_HEADER_BYTES, LEGACY_MAGIC as KV_LEGACY_MAGIC, MAGIC as KV_MAGIC, MAX_FRAME_BYTES,
    PREVIOUS_MAGIC as KV_PREVIOUS_MAGIC, ShardImage, Snapshot, decode_snapshot, decode_snapshot_inline_only, decode_snapshot_strict,
    decode_shard_image, encode_delta_frame, encode_snapshot,
};
pub use record::Record;
pub use value::{InlineValue, ObjectRef, StoredValue};

/// 分片快照字节编解码（磁盘后缀见 `database_types::cluster::SHARD_SUFFIX`）。
pub mod kv_format {
    pub use super::kv::*;
}
