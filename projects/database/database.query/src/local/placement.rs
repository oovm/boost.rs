//! Host placement hooks for values that may leave the row inline.
//!
//! `bytes` and `vector<N>` are the two value kinds a host may keep outside the
//! row: the threshold that decides between inline storage and an object
//! reference is a physical property of the store, so the query layer owns only
//! the port. A host installs a hook for the duration of one write execution and
//! receives either the original inline value or a reference it created itself.

use std::{cell::RefCell, sync::Arc};

use database_types::{Result, Value, Vector};

/// Storage hook that decides where a written `bytes` value is kept.
///
/// The octet threshold is a physical property of the host, so the query layer
/// never picks one: the host installs a hook and receives either the original
/// inline octets or an object reference it created itself.
pub trait BytesPlacementHook: Send + Sync {
    /// Returns the value to store: inline octets or a reference into the host object store.
    fn place(&self, octets: Vec<u8>) -> Result<Value>;
}

/// Storage hook that decides where a written `vector<N>` value is kept.
///
/// Small vectors stay inline in the row's tagged form; larger ones become a
/// content-addressed object reference (`ObjectKind::VectorPayload`). The
/// dimension threshold belongs to the host (same design as `bytes`), so the
/// query layer never picks one.
pub trait VectorPlacementHook: Send + Sync {
    /// Returns the value to store: inline `Value::Vector` or `Value::Object`
    /// referencing a `VectorPayload` in the host object store.
    fn place(&self, vector: Vector) -> Result<Value>;
}

type SharedPlacementHook = Arc<dyn BytesPlacementHook>;
type SharedVectorHook = Arc<dyn VectorPlacementHook>;

thread_local! {
    /// Placement hook installed for the duration of one write execution on this thread.
    static BYTES_PLACEMENT: RefCell<Option<SharedPlacementHook>> = RefCell::new(None);
    /// Vector placement hook installed for the duration of one write execution on this thread.
    static VECTOR_PLACEMENT: RefCell<Option<SharedVectorHook>> = RefCell::new(None);
}

struct PlacementGuard(Option<SharedPlacementHook>);

impl Drop for PlacementGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        BYTES_PLACEMENT.with(|slot| {
            slot.replace(previous);
        });
    }
}

struct VectorPlacementGuard(Option<SharedVectorHook>);

impl Drop for VectorPlacementGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        VECTOR_PLACEMENT.with(|slot| {
            slot.replace(previous);
        });
    }
}

/// Runs `body` with `hook` installed for `bytes` placement on this thread.
///
/// Without a hook every `bytes` value stays inline, which is what a host with
/// no object store can actually keep.
pub fn with_bytes_placement<T>(hook: Option<SharedPlacementHook>, body: impl FnOnce() -> T) -> T {
    let previous = BYTES_PLACEMENT.with(|slot| slot.replace(hook));
    let _guard = PlacementGuard(previous);
    body()
}

/// Runs `body` with `hook` installed for `vector<N>` placement on this thread.
///
/// Without a hook every vector stays inline in the row's tagged form, which
/// matches hosts with no object store.
pub fn with_vector_placement<T>(hook: Option<SharedVectorHook>, body: impl FnOnce() -> T) -> T {
    let previous = VECTOR_PLACEMENT.with(|slot| slot.replace(hook));
    let _guard = VectorPlacementGuard(previous);
    body()
}

/// Places one `bytes` value through the installed hook.
pub(crate) fn place_bytes(octets: Vec<u8>) -> Result<Value> {
    BYTES_PLACEMENT.with(|slot| match slot.borrow().as_ref() {
        Some(hook) => hook.place(octets),
        None => Ok(Value::Bytes(octets)),
    })
}

/// Places one `vector<N>` value through the installed hook.
///
/// Without a hook the vector stays inline as `Value::Vector`; a hook may
/// return a `Value::Object` referencing a CAS `VectorPayload`.
pub(crate) fn place_vector(vector: Vector) -> Result<Value> {
    VECTOR_PLACEMENT.with(|slot| match slot.borrow().as_ref() {
        Some(hook) => hook.place(vector),
        None => Ok(Value::Vector(vector)),
    })
}
