//! What the codecs of this host can actually do.
//!
//! A render pipeline must not discover a missing codec by watching a file
//! come out empty, so the platform declares its abilities up front and every
//! other entry point keeps the promise: anything `false` fails with
//! [`Error::unsupported`](media_types::Error::unsupported) instead of
//! quietly succeeding.

/// The encoding and decoding abilities of the current platform.
///
/// The struct is the honest answer to "can I run here": a caller checks the
/// fields it needs once, and the rest of the crate never surprises it.
/// Fields describe what the *library build* wires up, not whether a
/// particular machine happens to have a driver — a Windows host without the
/// Media Foundation H.264 encoder still reports the MF path and surfaces the
/// concrete failure when the encoder is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Still images can be decoded to RGBA8 ([`Error::unsupported`] otherwise).
    pub image_decode: bool,
    /// Video frames can be decoded to RGBA8.
    pub video_decode: bool,
    /// Audio files can be decoded to stereo f32.
    pub audio_decode: bool,
    /// H.264 video can be encoded through a system codec (Windows Media
    /// Foundation, macOS VideoToolbox).
    pub h264_encode: bool,
    /// AAC audio can be encoded through a system codec.
    pub aac_encode: bool,
    /// The always-available portable MP4 muxer: MJPEG video and PCM16 audio,
    /// written by this crate alone, on every platform.
    pub portable_encode: bool,
}

impl Capabilities {
    /// The abilities of the platform this binary was built for.
    #[must_use]
    pub const fn of_current_platform() -> Self {
        Self {
            #[cfg(windows)]
            image_decode: true,
            #[cfg(not(windows))]
            image_decode: false,
            #[cfg(windows)]
            video_decode: true,
            #[cfg(not(windows))]
            video_decode: false,
            #[cfg(windows)]
            audio_decode: true,
            #[cfg(not(windows))]
            audio_decode: false,
            #[cfg(windows)]
            h264_encode: true,
            #[cfg(not(windows))]
            h264_encode: false,
            #[cfg(windows)]
            aac_encode: true,
            #[cfg(not(windows))]
            aac_encode: false,
            portable_encode: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Capabilities;

    #[test]
    fn the_portable_muxer_is_declared_on_every_platform() {
        assert!(Capabilities::of_current_platform().portable_encode);
    }

    #[test]
    #[cfg(windows)]
    fn windows_declares_the_media_foundation_paths() {
        let caps = Capabilities::of_current_platform();
        assert!(caps.image_decode && caps.video_decode && caps.audio_decode);
        assert!(caps.h264_encode && caps.aac_encode);
    }

    #[test]
    #[cfg(not(windows))]
    fn hosts_without_a_system_codec_do_not_claim_one() {
        let caps = Capabilities::of_current_platform();
        assert!(!caps.h264_encode);
        assert!(!caps.aac_encode);
    }
}
