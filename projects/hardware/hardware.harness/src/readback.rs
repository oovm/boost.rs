//! Reference-versus-device read-back comparison.

use crate::{event::EventKind, tracer::Tracer};

/// Records `actual` as a read-back of `resource` and flags every element that
/// disagrees with `expected`.
///
/// The read-back itself is always recorded as [`EventKind::Readback`]. Each
/// disagreeing element becomes an [`EventKind::ReadbackMismatch`] carrying both
/// values. Returns `true` only when both slices have the same length and every
/// element matches.
pub fn compare_readback(resource: u32, expected: &[f32], actual: &[f32], tracer: &mut Tracer) -> bool {
    tracer.record(EventKind::Readback { resource, bytes: actual.len() * 4 });

    let mut matched = expected.len() == actual.len();
    for (index, (want, got)) in expected.iter().zip(actual).enumerate() {
        if want != got {
            matched = false;
            tracer.record(EventKind::ReadbackMismatch {
                resource,
                index,
                expected: format!("{want:.6}"),
                actual: format!("{got:.6}"),
            });
        }
    }
    matched
}
