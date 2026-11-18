//! Motion curves for geometry: easing, keyed camera curves and envelopes.
#![warn(missing_docs)]

pub mod curve;
pub mod ease;

pub use curve::{CameraKey, CameraPose, camera_at};
pub use ease::{Ease, apply, fade, smoothstep};
