//! Codecs: host decode/encode through WIC and Media Foundation, a portable
//! pure-Rust JPEG encoder and MP4 muxer for platforms without a system video
//! encoder, plus the PNG and WAV file writers.
//!
//! The `svg` feature adds the SVG writer. It consumes the normalized vector
//! representation from `media.vector` one-way — the vector crate never
//! imports this codec — so both export paths read the same resolved model.
//! The `obj` feature adds the OBJ writer. It consumes the exposed-face mesh
//! view from `media.voxel` one-way, for the same reason: the artifact and
//! the isometric preview are driven by the one face list.
#![warn(missing_docs)]

pub mod capability;
pub mod decode;
pub mod encode;
pub mod jpeg;
pub mod mp4;
#[cfg(feature = "obj")]
pub mod obj;
pub mod png;
pub mod texture;
#[cfg(feature = "svg")]
pub mod svg;
pub mod wav;

pub use capability::Capabilities;
pub use decode::{DecodeLimits, MediaLibrary};
pub use encode::Encoder;
pub use jpeg::encode_nv12;
pub use mp4::{AudioParams as Mp4AudioParams, Muxer as Mp4Muxer, VIDEO_TIMESCALE as MP4_VIDEO_TIMESCALE, VideoParams as Mp4VideoParams};
#[cfg(feature = "obj")]
pub use obj::{to_obj_string, write_obj};
pub use png::{PngColor, encode_png, encode_png_with_color, write_png};
pub use texture::{decode_bc1, decode_bc7};
#[cfg(feature = "svg")]
pub use svg::{to_svg_string, write_svg};
pub use wav::{WavFloat, WavPcm16, WavPcm24, parse_wav_f32, parse_wav_pcm16, parse_wav_pcm24, write_wav};
