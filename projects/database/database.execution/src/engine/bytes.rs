//! Host side of the `bytes` / `vector<N>` contract: the inline-versus-object
//! boundary, the placement intent a write carries, and row decoding of
//! content-addressed references.
//!
//! VOS names the two placements but never picks a threshold, so the boundary
//! belongs here: [`YydbBytesPolicy`] declares it from `INLINE_BYTES_MAX` and
//! [`YydbVectorPolicy`] applies the same design to `vector<N>` columns. The
//! query layer only produces pure values; [`Placement`] is the execution-side
//! port that decides where an out-of-row payload lands. Planning uses
//! [`Placement::Preview`], which derives the same content-addressed reference
//! the executor will commit through [`Placement::Commit`] without ever writing
//! the object, so a failed plan, an `EXPLAIN` or a repeated plan cannot leave
//! writes or allocated ids behind.

use std::sync::Arc;

use database_formats::yy::row::ObjectResolver;
use database_formats::yy::yydx::blob_chunk_hash;
use database_types::{HashAlgo, INLINE_BYTES_MAX, ObjectKind, ObjectRef, Result, Value, Vector};
use schema_types::{BytesPlacement, BytesPlacementPolicy};

use crate::engine::ObjectStore;

/// 本地引擎's own inline-versus-object boundary for `bytes` values.
///
/// Octets at or below [`INLINE_BYTES_MAX`] stay in the row; larger payloads
/// become a content-addressed object the row references. The threshold is a
/// physical property of this store: row storage hex-encodes octets inside a
/// single leaf cell, so a larger inline payload would not fit a stored row and
/// is not part of the VOS contract.
///
/// A single-file `.yydb` embeds the referenced objects as records in its main
/// image, so the placement is supported on both layouts. It is only a size
/// recommendation: embedding large payloads inflates the single file and makes
/// every snapshot and backup rewrite those bytes.
pub struct YydbBytesPolicy;

impl YydbBytesPolicy {
    /// Builds the policy. The threshold is a store constant, so no handle is
    /// needed; the parameter keeps the construction site explicit.
    pub fn new(_store: Arc<ObjectStore>) -> Self {
        Self
    }
}

impl BytesPlacementPolicy for YydbBytesPolicy {
    fn placement_for(&self, octets: &[u8]) -> BytesPlacement {
        if octets.len() <= INLINE_BYTES_MAX { BytesPlacement::Inline } else { BytesPlacement::ObjectReference }
    }
}

/// 本地引擎's own inline-versus-object boundary for `vector<N>` values.
///
/// Small vectors stay in the row's tagged JSON form (`{"$database":"vector",...}`);
/// larger ones become a `VectorPayload` object reference. The boundary is
/// derived from [`INLINE_BYTES_MAX`]: a vector's inline JSON occupies
/// roughly `53 + 8*N` bytes, so the safe threshold is around **120**
/// dimensions.
const INLINE_VECTOR_DIM_MAX: u32 = 120;

/// Host-side vector placement policy.
///
/// Mirrors [`YydbBytesPolicy`]: dimensions at or below
/// [`INLINE_VECTOR_DIM_MAX`] stay inline; above that the payload becomes a
/// `VectorPayload` reference in the same CAS. Both layouts support it, with
/// the same single-file size recommendation as `bytes`.
pub struct YydbVectorPolicy;

impl YydbVectorPolicy {
    /// Builds the policy. The threshold is a store constant, so no handle is
    /// needed; the parameter keeps the construction site explicit.
    pub fn new(_store: Arc<ObjectStore>) -> Self {
        Self
    }

    /// Whether `vector` fits the row's inline JSON budget.
    pub fn fits_inline(vector: &Vector) -> bool {
        vector.dim <= INLINE_VECTOR_DIM_MAX
    }
}

/// What one written value intends to store outside the row.
///
/// The intent is computed purely from the value: the object reference is the
/// content-addressed hash of the payload, so planning and execution derive the
/// identical row value while only execution writes the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementIntent {
    /// The value stays inline in the row.
    Inline,
    /// The row will reference this object; `payload` is its content.
    Object {
        /// CAS object kind (`Blob` for `bytes`, `VectorPayload` for vectors).
        kind: ObjectKind,
        /// The octets the object stores.
        payload: Vec<u8>,
    },
}

/// Decides, purely, whether one coerced value belongs outside the row.
pub fn placement_intent(value: &Value) -> PlacementIntent {
    match value {
        Value::Bytes(octets) if octets.len() > INLINE_BYTES_MAX => PlacementIntent::Object { kind: ObjectKind::Blob, payload: octets.clone() },
        Value::Vector(vector) if !YydbVectorPolicy::fits_inline(vector) => {
            PlacementIntent::Object { kind: ObjectKind::VectorPayload, payload: vector.to_le_bytes() }
        }
        _ => PlacementIntent::Inline,
    }
}

/// The content-addressed reference `payload` will keep, without storing it.
///
/// Identical to what [`ObjectStore::put_chunk`] returns for the same octets,
/// which is what lets [`Placement::Preview`] agree with [`Placement::Commit`].
pub fn object_reference(kind: ObjectKind, payload: &[u8]) -> ObjectRef {
    ObjectRef { algo: HashAlgo::Blake3, hash: blob_chunk_hash(payload), size: payload.len() as u64, kind }
}

/// Where a write program places its out-of-row values.
///
/// The query layer never decides this: coercion produces pure values and the
/// execution side applies one of the three modes at the row-build choke point.
/// A connection that plans with [`Placement::Preview`] must execute the same
/// statement with [`Placement::Commit`] over the same store, because both
/// derive the row value from the content hash alone.
#[derive(Clone, Copy)]
pub enum Placement<'a> {
    /// No object store is attached: every value stays inline in the row.
    Inline,
    /// Planning: large values become their deterministic content-addressed
    /// reference; nothing is written and no id is allocated durably.
    Preview,
    /// Execution: large payloads are written through the object store and the
    /// row keeps the reference.
    Commit(&'a ObjectStore),
}

impl<'a> Placement<'a> {
    /// Places one coerced value, returning the value the row will hold.
    pub fn place(&self, value: Value) -> Result<Value> {
        match placement_intent(&value) {
            PlacementIntent::Inline => Ok(value),
            PlacementIntent::Object { kind, payload } => match self {
                Placement::Inline => Ok(value),
                Placement::Preview => Ok(Value::Object(object_reference(kind, &payload))),
                Placement::Commit(store) => store.put_chunk(kind, &payload).map(Value::Object),
            },
        }
    }
}

/// Resolves a row's `bytes` object reference through the connection object store.
pub struct ObjectStoreResolver {
    store: Arc<ObjectStore>,
}

impl ObjectStoreResolver {
    /// Builds a resolver over `store`.
    pub fn new(store: Arc<ObjectStore>) -> Self {
        Self { store }
    }
}

impl ObjectResolver for ObjectStoreResolver {
    fn resolve(&self, object: &ObjectRef) -> Result<Vec<u8>> {
        Ok(self.store.get_object(object)?.to_vec())
    }
}
