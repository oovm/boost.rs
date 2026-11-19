//! MP4 encoding: H.264/AAC through the Media Foundation that ships with
//! Windows, and a portable MJPEG/PCM backend written by this crate alone on
//! every other platform.
//!
//! Video enters as NV12 samples; on Windows the MF sink writer encodes them
//! to H.264, elsewhere the crate's own JPEG encoder packs them into the
//! MP4 muxer. Audio enters as interleaved PCM16 (with the
//! WAVEFORMATEXTENSIBLE tag MF expects inside uncompressed audio samples)
//! and becomes AAC on Windows or `sowt` PCM elsewhere. No external encoder
//! is involved on either path.

use media_types::Error;

/// The encoder for one output file, backed by the platform's best path.
pub struct Encoder {
    #[cfg(windows)]
    inner: WindowsEncoder,
    #[cfg(not(windows))]
    inner: PortableEncoder,
}

impl Encoder {
    /// Opens `out` for H.264 video at `fps` and optional audio at `sample_rate`.
    ///
    /// The sample rate is an input contract: the container declares exactly
    /// the rate of the PCM the caller will feed in. The audio codec is
    /// negotiated: AAC is preferred, and machines without the AAC encoder MFT
    /// registered fall back to FLAC.
    pub fn open(
        out: &std::path::Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        sample_rate: u32,
        with_audio: bool,
    ) -> Result<Self, Error> {
        Self::open_with_audio_codec(out, width, height, fps, bitrate, sample_rate, with_audio, None)
    }

    /// Opens `out` requiring one audio codec instead of accepting a fallback.
    ///
    /// `required` is `None` to negotiate as [`Encoder::open`] does, or a codec
    /// name (`aac`, `flac`, `pcm`). When a name is given and the machine cannot
    /// provide it, this is an error: a delivery profile that promises AAC must
    /// not quietly produce FLAC. Off Windows the portable backend writes
    /// `pcm`, so only `None`/`"pcm"` succeed there.
    pub fn open_with_audio_codec(
        out: &std::path::Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        sample_rate: u32,
        with_audio: bool,
        required: Option<&str>,
    ) -> Result<Self, Error> {
        #[cfg(windows)]
        {
            Ok(Self { inner: WindowsEncoder::open(out, width, height, fps, bitrate, sample_rate, with_audio, required)? })
        }
        #[cfg(not(windows))]
        {
            Ok(Self { inner: PortableEncoder::open(out, width, height, fps, bitrate, sample_rate, with_audio, required)? })
        }
    }

    /// Name of the negotiated audio codec (`aac`, `flac`, `pcm`, or empty
    /// when silent).
    #[must_use]
    pub fn audio_codec(&self) -> &str {
        self.inner.audio_codec()
    }

    /// Writes one NV12 frame (1.5 bytes per pixel, stride = width).
    pub fn write_video(&mut self, nv12: &[u8], frame: u32, fps: u32) -> Result<(), Error> {
        self.inner.write_video(nv12, frame, fps)
    }

    /// Writes one interleaved s16 stereo chunk starting at `start_sample`.
    pub fn write_audio(&mut self, pcm: &[i16], start_sample: u64) -> Result<(), Error> {
        self.inner.write_audio(pcm, start_sample)
    }

    /// Flushes and closes the container.
    pub fn finish(self) -> Result<(), Error> {
        #[cfg(windows)]
        {
            let mut inner = self.inner;
            inner.finish()
        }
        #[cfg(not(windows))]
        {
            self.inner.finish()
        }
    }
}

#[cfg(windows)]
mod windows_encoder {
    use std::path::Path;

    use windows::{
        Win32::{
            Media::MediaFoundation as mf,
            System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx},
        },
        core::{GUID, PCWSTR},
    };

    use media_types::Error;

    /// MFInterlaceMode_Progressive
    const INTERLACE_PROGRESSIVE: u32 = 2;
    /// MFNominalRange_0_255 (full range)
    const NOMINAL_RANGE_FULL: u32 = 1;

    fn encode_error(context: &str, error: windows::core::Error) -> Error {
        Error::encode(format!("{context}: {error}"))
    }

    fn wide(path: &Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }

    unsafe fn set_guid(mt: &mf::IMFMediaType, key: &GUID, value: &GUID) -> Result<(), windows::core::Error> {
        unsafe { mt.SetGUID(key as *const GUID, value as *const GUID) }
    }

    unsafe fn set_u32(mt: &mf::IMFMediaType, key: &GUID, value: u32) -> Result<(), windows::core::Error> {
        unsafe { mt.SetUINT32(key as *const GUID, value) }
    }

    unsafe fn set_u64(mt: &mf::IMFMediaType, key: &GUID, value: u64) -> Result<(), windows::core::Error> {
        unsafe { mt.SetUINT64(key as *const GUID, value) }
    }

    const fn pack32(hi: u32, lo: u32) -> u64 {
        ((hi as u64) << 32) | lo as u64
    }

    pub(super) struct WindowsEncoder {
        writer: mf::IMFSinkWriter,
        video_index: u32,
        audio_index: Option<u32>,
        width: u32,
        height: u32,
        sample_rate: u32,
        started: bool,
        audio_codec: String,
        /// Whether the negotiated input accepts 32-bit float (FLAC) instead of
        /// 16-bit PCM (AAC).
        audio_float: bool,
    }

    impl WindowsEncoder {
        pub fn open(
            out: &Path,
            width: u32,
            height: u32,
            fps: u32,
            bitrate: u32,
            sample_rate: u32,
            with_audio: bool,
            required: Option<&str>,
        ) -> Result<Self, Error> {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                mf::MFStartup(mf::MF_VERSION, mf::MFSTARTUP_FULL).map_err(|e| encode_error("MFStartup", e))?;
            }
            let candidates: &[(&windows::core::GUID, &str)] = if with_audio {
                &[(&mf::MFAudioFormat_AAC, "aac"), (&mf::MFAudioFormat_FLAC, "flac")]
            }
            else {
                &[(&mf::MFAudioFormat_AAC, "none")]
            };
            // A required codec removes the fallback instead of reordering it:
            // the caller asked for one output, not for the closest available.
            let candidates: Vec<(&windows::core::GUID, &str)> = match required {
                Some(name) => {
                    let selected: Vec<_> = candidates.iter().copied().filter(|(_, codec)| *codec == name).collect();
                    if selected.is_empty() {
                        return Err(Error::unsupported(format!("audio codec `{name}` is not an encoder option")));
                    }
                    selected
                }
                None => candidates.to_vec(),
            };
            let mut last_error = None;
            for (subtype, name) in &candidates {
                match Self::try_open(out, width, height, fps, bitrate, sample_rate, with_audio, *subtype, *name) {
                    Ok(encoder) => return Ok(encoder),
                    Err(error) => last_error = Some(error),
                }
            }
            Err(last_error.unwrap_or_else(|| Error::encode("no audio codec negotiated")))
        }

        fn try_open(
            out: &Path,
            width: u32,
            height: u32,
            fps: u32,
            bitrate: u32,
            sample_rate: u32,
            with_audio: bool,
            audio_subtype: &windows::core::GUID,
            codec_name: &str,
        ) -> Result<Self, Error> {
            let wide_path = wide(out);
            // The container follows the file extension. Asserting one here
            // would pin FLAC to an MP4 header it cannot carry, so the sink
            // writer keeps choosing it.
            let writer: mf::IMFSinkWriter = unsafe {
                mf::MFCreateSinkWriterFromURL(PCWSTR(wide_path.as_ptr()), None::<&mf::IMFByteStream>, None::<&mf::IMFAttributes>)
                    .map_err(|e| encode_error("MFCreateSinkWriterFromURL", e))?
            };

            // --- video stream ---
            let video_out = unsafe { mf::MFCreateMediaType().map_err(|e| encode_error("MFCreateMediaType(video out)", e))? };
            unsafe {
                set_guid(&video_out, &mf::MF_MT_MAJOR_TYPE, &mf::MFMediaType_Video).map_err(|e| encode_error("video major", e))?;
                set_guid(&video_out, &mf::MF_MT_SUBTYPE, &mf::MFVideoFormat_H264).map_err(|e| encode_error("video subtype", e))?;
                set_u32(&video_out, &mf::MF_MT_AVG_BITRATE, bitrate).map_err(|e| encode_error("video bitrate", e))?;
                set_u64(&video_out, &mf::MF_MT_FRAME_SIZE, pack32(width, height)).map_err(|e| encode_error("frame size", e))?;
                set_u64(&video_out, &mf::MF_MT_FRAME_RATE, pack32(fps, 1)).map_err(|e| encode_error("frame rate", e))?;
                set_u64(&video_out, &mf::MF_MT_PIXEL_ASPECT_RATIO, pack32(1, 1)).map_err(|e| encode_error("pixel aspect", e))?;
                set_u32(&video_out, &mf::MF_MT_INTERLACE_MODE, INTERLACE_PROGRESSIVE).map_err(|e| encode_error("interlace", e))?;
            }
            let video_in = unsafe { mf::MFCreateMediaType().map_err(|e| encode_error("MFCreateMediaType(video in)", e))? };
            unsafe {
                set_guid(&video_in, &mf::MF_MT_MAJOR_TYPE, &mf::MFMediaType_Video).map_err(|e| encode_error("video in major", e))?;
                set_guid(&video_in, &mf::MF_MT_SUBTYPE, &mf::MFVideoFormat_NV12).map_err(|e| encode_error("video in subtype", e))?;
                set_u64(&video_in, &mf::MF_MT_FRAME_SIZE, pack32(width, height)).map_err(|e| encode_error("frame size in", e))?;
                set_u64(&video_in, &mf::MF_MT_FRAME_RATE, pack32(fps, 1)).map_err(|e| encode_error("frame rate in", e))?;
                set_u64(&video_in, &mf::MF_MT_PIXEL_ASPECT_RATIO, pack32(1, 1)).map_err(|e| encode_error("pixel aspect in", e))?;
                set_u32(&video_in, &mf::MF_MT_INTERLACE_MODE, INTERLACE_PROGRESSIVE).map_err(|e| encode_error("interlace in", e))?;
                set_u32(&video_in, &mf::MF_MT_DEFAULT_STRIDE, width).map_err(|e| encode_error("stride", e))?;
                set_u32(&video_in, &mf::MF_MT_VIDEO_NOMINAL_RANGE, NOMINAL_RANGE_FULL).map_err(|e| encode_error("nominal range", e))?;
            }
            let video_index = unsafe { writer.AddStream(&video_out).map_err(|e| encode_error("AddStream(video)", e))? };
            unsafe {
                writer
                    .SetInputMediaType(video_index, &video_in, None::<&mf::IMFAttributes>)
                    .map_err(|e| encode_error("SetInputMediaType(video)", e))?
            };

            // --- audio stream ---
            let mut audio_index = None;
            if with_audio {
                let audio_out = unsafe { mf::MFCreateMediaType().map_err(|e| encode_error("MFCreateMediaType(audio out)", e))? };
                unsafe {
                    set_guid(&audio_out, &mf::MF_MT_MAJOR_TYPE, &mf::MFMediaType_Audio).map_err(|e| encode_error("audio major", e))?;
                    set_guid(&audio_out, &mf::MF_MT_SUBTYPE, audio_subtype).map_err(|e| encode_error("audio subtype", e))?;
                    set_u32(&audio_out, &mf::MF_MT_AUDIO_NUM_CHANNELS, 2).map_err(|e| encode_error("audio channels", e))?;
                    set_u32(&audio_out, &mf::MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate).map_err(|e| encode_error("audio rate", e))?;
                    set_u32(&audio_out, &mf::MF_MT_AUDIO_BITS_PER_SAMPLE, 16).map_err(|e| encode_error("audio bits", e))?;
                    if *audio_subtype == mf::MFAudioFormat_AAC {
                        // The AAC output type is the encoder's configuration, and
                        // the Microsoft AAC MFT rejects the whole media type with
                        // MF_E_INVALIDMEDIATYPE (0xC00D36B4) unless the source
                        // bit depth and an exact bitrate are both present. The
                        // rate is not decorative: 128 kbit/s stereo is one of
                        // the bitrates the MFT itself advertises.
                        set_u32(&audio_out, &mf::MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 16_000).map_err(|e| encode_error("audio bitrate", e))?;
                    }
                }
                let audio_in = unsafe { mf::MFCreateMediaType().map_err(|e| encode_error("MFCreateMediaType(audio in)", e))? };
                // The AAC encoder MFT accepts 16-bit PCM input; FLAC accepts
                // 32-bit float. Picking the wrong one makes SetInputMediaType
                // fail with a generic "unsupported" instead of saying which
                // side mismatched, so the choice is made from the codec here.
                let aac = *audio_subtype == mf::MFAudioFormat_AAC;
                let (in_subtype, bits, block_align) =
                    if aac { (&mf::MFAudioFormat_PCM, 16u32, 4u32) } else { (&mf::MFAudioFormat_Float, 32u32, 8u32) };
                unsafe {
                    set_guid(&audio_in, &mf::MF_MT_MAJOR_TYPE, &mf::MFMediaType_Audio).map_err(|e| encode_error("audio in major", e))?;
                    set_guid(&audio_in, &mf::MF_MT_SUBTYPE, in_subtype).map_err(|e| encode_error("audio in subtype", e))?;
                    set_u32(&audio_in, &mf::MF_MT_AUDIO_NUM_CHANNELS, 2).map_err(|e| encode_error("audio in channels", e))?;
                    set_u32(&audio_in, &mf::MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate).map_err(|e| encode_error("audio in rate", e))?;
                    set_u32(&audio_in, &mf::MF_MT_AUDIO_BITS_PER_SAMPLE, bits).map_err(|e| encode_error("audio in bits", e))?;
                    set_u32(&audio_in, &mf::MF_MT_AUDIO_BLOCK_ALIGNMENT, block_align).map_err(|e| encode_error("audio block align", e))?;
                    set_u32(&audio_in, &mf::MF_MT_AUDIO_AVG_BYTES_PER_SECOND, sample_rate * block_align)
                        .map_err(|e| encode_error("audio in avg bytes", e))?;
                    if aac {
                        // Stereo: front left and front right. The AAC encoder
                        // refuses an input type without a channel layout.
                        set_u32(&audio_in, &mf::MF_MT_AUDIO_CHANNEL_MASK, 0x3).map_err(|e| encode_error("audio channel mask", e))?;
                    }
                }
                audio_index = Some(unsafe { writer.AddStream(&audio_out).map_err(|e| encode_error("AddStream(audio)", e))? });
                unsafe {
                    writer
                        .SetInputMediaType(audio_index.unwrap(), &audio_in, None::<&mf::IMFAttributes>)
                        .map_err(|e| encode_error("SetInputMediaType(audio)", e))?;
                }
            }

            Ok(Self {
                writer,
                video_index,
                audio_index,
                width,
                height,
                sample_rate,
                started: false,
                audio_codec: codec_name.to_string(),
                audio_float: codec_name != "aac",
            })
        }

        pub fn write_video(&mut self, nv12: &[u8], frame: u32, fps: u32) -> Result<(), Error> {
            if !self.started {
                unsafe { self.writer.BeginWriting().map_err(|e| encode_error("BeginWriting", e))? };
                self.started = true;
            }
            let expected = (self.width * self.height * 3 / 2) as usize;
            if nv12.len() != expected {
                return Err(Error::encode(format!("NV12 buffer {} bytes, expected {expected}", nv12.len())));
            }
            let unit = 10_000_000i64;
            unsafe {
                let sample = mf::MFCreateSample().map_err(|e| encode_error("MFCreateSample", e))?;
                let buffer = mf::MFCreateMemoryBuffer(nv12.len() as u32).map_err(|e| encode_error("MFCreateMemoryBuffer", e))?;
                let mut ptr: *mut u8 = std::ptr::null_mut();
                buffer.Lock(&mut ptr, None, None).map_err(|e| encode_error("Lock", e))?;
                std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
                buffer.Unlock().map_err(|e| encode_error("Unlock", e))?;
                buffer.SetCurrentLength(nv12.len() as u32).map_err(|e| encode_error("SetCurrentLength", e))?;
                sample.AddBuffer(&buffer).map_err(|e| encode_error("AddBuffer", e))?;
                sample.SetSampleTime(frame as i64 * unit / fps as i64).map_err(|e| encode_error("SetSampleTime", e))?;
                sample.SetSampleDuration(unit / fps as i64).map_err(|e| encode_error("SetSampleDuration", e))?;
                self.writer.WriteSample(self.video_index, &sample).map_err(|e| encode_error("WriteSample(video)", e))?;
            }
            Ok(())
        }

        pub fn write_audio(&mut self, pcm: &[i16], start_sample: u64) -> Result<(), Error> {
            let Some(index) = self.audio_index
            else {
                return Ok(());
            };
            if !self.started {
                unsafe { self.writer.BeginWriting().map_err(|e| encode_error("BeginWriting", e))? };
                self.started = true;
            }
            // AAC takes the interleaved 16-bit samples as-is; FLAC takes float.
            let (bytes, payload): (Vec<u8>, usize) = if self.audio_float {
                let floats: Vec<f32> = pcm.iter().map(|s| *s as f32 / 32768.0).collect();
                let bytes = floats.iter().flat_map(|value| value.to_ne_bytes()).collect();
                (bytes, floats.len() * 4)
            }
            else {
                let bytes = pcm.iter().flat_map(|value| value.to_le_bytes()).collect();
                (bytes, pcm.len() * 2)
            };
            unsafe {
                let sample = mf::MFCreateSample().map_err(|e| encode_error("MFCreateSample(audio)", e))?;
                let buffer = mf::MFCreateMemoryBuffer(payload as u32).map_err(|e| encode_error("MFCreateMemoryBuffer(audio)", e))?;
                let mut ptr: *mut u8 = std::ptr::null_mut();
                buffer.Lock(&mut ptr, None, None).map_err(|e| encode_error("Lock(audio)", e))?;
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, payload);
                buffer.Unlock().map_err(|e| encode_error("Unlock(audio)", e))?;
                buffer.SetCurrentLength(payload as u32).map_err(|e| encode_error("SetCurrentLength(audio)", e))?;
                sample.AddBuffer(&buffer).map_err(|e| encode_error("AddBuffer(audio)", e))?;
                let rate = self.sample_rate as i64;
                sample.SetSampleTime(start_sample as i64 * 10_000_000 / rate).map_err(|e| encode_error("SetSampleTime(audio)", e))?;
                sample.SetSampleDuration(pcm.len() as i64 * 10_000_000 / 2 / rate).map_err(|e| encode_error("SetSampleDuration(audio)", e))?;
                self.writer.WriteSample(index, &sample).map_err(|e| encode_error("WriteSample(audio)", e))?;
            }
            Ok(())
        }

        pub(super) fn audio_codec(&self) -> &str {
            &self.audio_codec
        }

        pub fn finish(&mut self) -> Result<(), Error> {
            unsafe {
                self.writer.Finalize().map_err(|e| encode_error("Finalize", e))?;
                mf::MFShutdown().map_err(|e| encode_error("MFShutdown", e))?;
            }
            Ok(())
        }
    }
}

#[cfg(windows)]
use windows_encoder::WindowsEncoder;

#[cfg(not(windows))]
use portable_encoder::PortableEncoder;

/// The encoding path every platform has: the crate's own JPEG encoder feeds
/// the crate's own MP4 muxer, with audio kept as uncompressed PCM16.
///
/// This is a fallback for correctness, not a competitor with a hardware
/// H.264 encoder — frames are large and the container declares `jpeg`/`sowt`
/// sample entries. Platforms with a system encoder (Windows today, macOS
/// next) use that instead.
#[cfg(not(windows))]
mod portable_encoder {
    use std::path::Path;

    use media_types::Error;

    use crate::{
        jpeg::{DEFAULT_QUALITY, encode_nv12},
        mp4::{AudioParams, Muxer, VIDEO_TIMESCALE, VideoParams},
    };

    pub(super) struct PortableEncoder {
        muxer: Muxer,
        width: u32,
        height: u32,
        silent: bool,
    }

    impl PortableEncoder {
        pub fn open(
            out: &Path,
            width: u32,
            height: u32,
            fps: u32,
            bitrate: u32,
            sample_rate: u32,
            with_audio: bool,
            required: Option<&str>,
        ) -> Result<Self, Error> {
            let _ = bitrate; // MJPEG is quality-based; the target bitrate has no meaning here.
            if width == 0 || height == 0 || width > 65_535 || height > 65_535 {
                return Err(Error::invalid_input(format!("portable: frame {width}x{height} is outside 1..=65535 per side")));
            }
            if width % 2 == 1 || height % 2 == 1 {
                return Err(Error::invalid_input(format!("portable: frame {width}x{height} needs even dimensions for 4:2:0")));
            }
            if fps == 0 {
                return Err(Error::invalid_input("portable: frame rate must be positive"));
            }
            match required {
                None | Some("pcm") => {}
                Some(name) => {
                    return Err(Error::unsupported(format!(
                        "no system `{name}` encoder on this platform; the portable backend writes PCM (`pcm`) only"
                    )));
                }
            }
            let audio = with_audio.then(|| AudioParams {
                fourcc: *b"sowt",
                sample_rate: sample_rate.max(1),
                channels: 2,
                bits: 16,
                entry_children: Vec::new(),
            });
            let muxer = Muxer::open(
                out,
                VideoParams { fourcc: *b"jpeg", width: width as u16, height: height as u16, entry_children: Vec::new() },
                audio,
            )?;
            Ok(Self { muxer, width, height, silent: !with_audio })
        }

        pub fn write_video(&mut self, nv12: &[u8], frame: u32, fps: u32) -> Result<(), Error> {
            let _ = frame; // the muxer derives timing from durations, not absolute sample numbers
            let expected = (self.width * self.height * 3 / 2) as usize;
            if nv12.len() != expected {
                return Err(Error::encode(format!("NV12 buffer {} bytes, expected {expected}", nv12.len())));
            }
            let jpeg = encode_nv12(self.width, self.height, nv12, DEFAULT_QUALITY)?;
            let duration = VIDEO_TIMESCALE / fps.max(1);
            self.muxer.write_video_sample(&jpeg, duration, true)
        }

        pub fn write_audio(&mut self, pcm: &[i16], start_sample: u64) -> Result<(), Error> {
            let _ = start_sample; // chunks are appended in order; the muxer places them
            if self.silent {
                return Ok(());
            }
            if pcm.len() % 2 != 0 {
                return Err(Error::invalid_input(format!("portable: stereo PCM needs an even sample count, got {}", pcm.len())));
            }
            let bytes: Vec<u8> = pcm.iter().flat_map(|value| value.to_le_bytes()).collect();
            let frames = (pcm.len() / 2) as u32;
            self.muxer.write_audio_sample(&bytes, frames)
        }

        pub(super) fn audio_codec(&self) -> &'static str {
            if self.silent { "" } else { "pcm" }
        }

        pub fn finish(self) -> Result<(), Error> {
            self.muxer.finish()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::PortableEncoder;

        fn frame(width: u32, height: u32, shift: u8) -> Vec<u8> {
            let (w, h) = (width as usize, height as usize);
            let mut nv12 = vec![0u8; w * h + w * h / 2];
            for (i, byte) in nv12[..w * h].iter_mut().enumerate() {
                *byte = (i as u8).wrapping_mul(3).wrapping_add(shift);
            }
            for byte in &mut nv12[w * h..] {
                *byte = 128;
            }
            nv12
        }

        #[test]
        fn writes_a_parseable_mjpeg_pcm_file() {
            let dir = std::env::temp_dir().join("media-codec-portable-tests");
            std::fs::create_dir_all(&dir).expect("temp dir");
            let path = dir.join("portable-round-trip.mp4");

            let mut encoder = PortableEncoder::open(&path, 32, 32, 30, 0, 48_000, true, None).expect("open");
            assert_eq!(encoder.audio_codec(), "pcm");
            encoder.write_video(&frame(32, 32, 0), 0, 30).expect("frame 0");
            encoder.write_video(&frame(32, 32, 40), 1, 30).expect("frame 1");
            encoder.write_audio(&[0i16; 960], 0).expect("audio");
            encoder.finish().expect("finish");

            let bytes = std::fs::read(&path).expect("read back");
            assert_eq!(&bytes[4..8], b"ftyp");
            // The mdat carries the two JPEG frames: each starts with SOI.
            let soi = u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as usize;
            assert_eq!(&bytes[soi + 16..soi + 18], &[0xFF, 0xD8], "first sample is a JPEG");
            assert!(bytes.windows(4).any(|w| w == b"moov"), "moov written after mdat");
            assert!(bytes.windows(4).any(|w| w == b"jpeg"), "video sample entry");
            assert!(bytes.windows(4).any(|w| w == b"sowt"), "audio sample entry");
            std::fs::remove_file(&path).ok();
        }

        #[test]
        fn a_promised_system_codec_is_not_silently_dropped() {
            let path = std::env::temp_dir().join("rejected.mp4");
            match PortableEncoder::open(&path, 16, 16, 30, 0, 48_000, true, Some("aac")) {
                Err(error) => assert_eq!(error.code(), "unsupported_platform"),
                Ok(_) => panic!("a promised AAC encoder must be refused, not silently downgraded"),
            }
        }
    }
}
