//! Bounds-checked reads over one container image.
//!
//! Every public entry point takes the whole image, so a read at an absolute
//! offset is an ordinary slice. The helpers keep the failure uniform: a range
//! that leaves the image answers `binary/range` with the span it wanted.

use crate::{ContainerError, Result, Span};

/// Returns `image[start..start + length]`, or a `binary/range` failure that
/// names the wanted span.
pub(crate) fn window(image: &[u8], start: u64, length: u64) -> Result<&[u8]> {
    let end = start.checked_add(length).ok_or_else(|| {
        ContainerError::range(format!("the read at {start} of {length} bytes overflows")).with_span(Span::new(start, u64::MAX))
    })?;
    let from = usize::try_from(start)
        .map_err(|_| ContainerError::range(format!("offset {start} does not fit a platform index")).at(start, end))?;
    let to =
        usize::try_from(end).map_err(|_| ContainerError::range(format!("offset {end} does not fit a platform index")).at(start, end))?;
    image
        .get(from..to)
        .ok_or_else(|| ContainerError::range(format!("the read at {start}..{end} runs past the end of the image")).at(start, end))
}
