//! Reopening a delivered file and reporting what it actually contains.
//!
//! A render report describes what the renderer *intended* to write. This module
//! opens the delivered file again with the decoder and reports facts: the frame
//! size, whether sampled frames decode, how bright they are, and the measured
//! audio level. It exists so a delivery can be checked without trusting the
//! process that produced it.
//!
//! Limits that are stated instead of hidden:
//!
//! - Frames are *sampled* at evenly spaced times. This proves the file decodes
//!   at those points; it is not a full frame count. The audio track gives an
//!   exact duration when one exists.
//! - A near-black sample and a frozen pair are reported as suspects, not as
//!   automatic failures: a dark scene and a held shot are legitimate.
//! - Nothing here re-encodes or repairs the file.

use std::path::Path;

use media_codec::{DecodeLimits, MediaLibrary};
use media_types::Error;
use serde::{Deserialize, Serialize};

/// One sampled frame.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SampleReport {
    /// Sample index, `0..probes`.
    pub index: usize,
    /// Time in seconds the sample was taken at.
    pub seconds: f64,
    /// Mean luminance in `0..=1`.
    pub mean_luma: f32,
    /// Whether two neighbouring samples are byte-identical.
    pub frozen: bool,
}

/// Measured audio facts of the delivered file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AudioReport {
    /// Decoded sample rate.
    pub sample_rate: u32,
    /// Interleaved stereo frames measured.
    pub frames: u64,
    /// Exact duration of the decoded audio.
    pub seconds: f64,
    /// Peak level in dBFS.
    pub peak_db: f32,
    /// RMS level in dBFS.
    pub rms_db: f32,
    /// Samples at or beyond full scale.
    pub clipped: u64,
}

/// Facts read back from a delivered file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DeliveryReport {
    /// File that was inspected.
    pub path: String,
    /// Decoded frame width.
    pub width: u32,
    /// Decoded frame height.
    pub height: u32,
    /// Sampled frames.
    pub samples: Vec<SampleReport>,
    /// Audio facts, when the file carries a decodable track.
    pub audio: Option<AudioReport>,
    /// Duration the samples were spread over. `None` means the file has no
    /// audio track, so the duration is unknown and only the opening seconds
    /// were sampled.
    pub span_seconds: Option<f64>,
    /// Human-readable suspects; empty means nothing looked wrong.
    pub suspects: Vec<String>,
}

impl DeliveryReport {
    /// Whether nothing was flagged.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.suspects.is_empty()
    }
}

/// Explicit thresholds used when flagging a delivery.
///
/// They are constants rather than hidden magic numbers so a report can be read
/// against the same numbers that produced it.
pub mod threshold {
    /// A sampled frame darker than this is reported as near black.
    pub const BLACK_LUMA: f32 = 0.02;
    /// An audio track quieter than this across the whole file is reported as silent.
    pub const SILENCE_RMS_DB: f32 = -60.0;
    /// Peak above this leaves no headroom for lossy re-encoding downstream.
    pub const HEADROOM_PEAK_DB: f32 = -0.1;
}

/// Reopens `path` and reports what the decoder sees.
///
/// `probes` frames are taken evenly across the audio duration when a track
/// exists, and across `probes` one-second steps otherwise. A file that cannot
/// be decoded at all is an error: silence about a broken delivery is worse than
/// a failure.
pub fn inspect_delivery(path: &Path, probes: usize, sample_rate: u32) -> Result<DeliveryReport, Error> {
    let display = path.to_string_lossy().into_owned();
    let probes = probes.max(2);
    let mut library = MediaLibrary::new();
    let limits = DecodeLimits::unbounded();

    let audio = match library.audio_pcm(&display, sample_rate, limits) {
        Ok(pcm) if !pcm.is_empty() => {
            let interleaved: Vec<f32> = pcm.iter().flat_map(|frame| frame.iter().copied()).collect();
            let level = media_harness::measure_f32(&interleaved);
            let frames = pcm.len() as u64;
            Some(AudioReport {
                sample_rate,
                frames,
                seconds: frames as f64 / sample_rate as f64,
                peak_db: level.peak_db,
                rms_db: level.rms_db,
                clipped: level.clipped as u64,
            })
        }
        _ => None,
    };

    // A video-only file gives no duration: the decoder clamps a late request to
    // the last frame instead of failing, so walking forward would report a fake
    // span. Sample the opening seconds at a fixed step and say so.
    let span = audio.as_ref().map(|audio| audio.seconds.max(0.001));
    let mut samples = Vec::with_capacity(probes);
    let mut previous: Option<Vec<u8>> = None;
    let (mut width, mut height) = (0u32, 0u32);
    for index in 0..probes {
        let seconds = match span {
            Some(span) => span * (index as f64 + 0.5) / probes as f64,
            None => 0.25 + index as f64 * 0.25,
        };
        let frame = library.video_frame(&display, seconds, limits)?;
        width = frame.width;
        height = frame.height;
        let mean_luma = mean_luma(&frame.rgba);
        let frozen = previous.as_ref().is_some_and(|prev| prev == &frame.rgba);
        previous = Some(frame.rgba.clone());
        samples.push(SampleReport { index, seconds, mean_luma, frozen });
    }
    if width == 0 || height == 0 {
        return Err(Error::media(format!("{display} decoded no frame")));
    }

    let mut suspects = Vec::new();
    for sample in &samples {
        if sample.mean_luma < threshold::BLACK_LUMA {
            suspects.push(format!("sample {} at {:.2}s is near black", sample.index, sample.seconds));
        }
        if sample.frozen {
            suspects.push(format!(
                "sample {} at {:.2}s is byte-identical to the previous one; check for a frozen shot",
                sample.index, sample.seconds
            ));
        }
    }
    if let Some(audio) = &audio {
        if audio.rms_db < threshold::SILENCE_RMS_DB {
            suspects.push(format!(
                "audio averages {:.1} dBFS, below the {:.0} dBFS silence threshold",
                audio.rms_db,
                threshold::SILENCE_RMS_DB
            ));
        }
        if audio.clipped > 0 {
            suspects.push(format!("audio clips on {} samples", audio.clipped));
        }
        if audio.peak_db > threshold::HEADROOM_PEAK_DB {
            suspects.push(format!("audio peak {:.1} dBFS leaves no delivery headroom", audio.peak_db));
        }
    }

    Ok(DeliveryReport { path: display, width, height, samples, audio, span_seconds: span, suspects })
}

/// Mean luminance in `0..=1`.
fn mean_luma(rgba: &[u8]) -> f32 {
    let pixels = rgba.len() / 4;
    if pixels == 0 {
        return 0.0;
    }
    let mut sum = 0.0f32;
    for chunk in rgba.chunks_exact(4) {
        sum += 0.2126 * chunk[0] as f32 + 0.7152 * chunk[1] as f32 + 0.0722 * chunk[2] as f32;
    }
    sum / (pixels as f32 * 255.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{RenderOptions, render_project_with};

    fn render_fixture(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("opencut-delivery-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join(name);
        let film = serde_json::json!({
            "width": 160, "height": 90, "fps": 15, "duration_frames": 30,
            "scenes": [{ "start": 0, "duration": 30,
                "background": { "kind": "solid", "color": "#203040" }, "items": [] }],
            "audio": null
        });
        render_project_with(&film.to_string(), &out, &RenderOptions::default(), &mut |_, _| {}).expect("render");
        out
    }

    #[test]
    fn a_delivered_file_reopens_with_its_real_frame_size() {
        let out = render_fixture("inspect-clean.mp4");
        let report = inspect_delivery(&out, 4, 44_100).expect("inspect");
        assert_eq!((report.width, report.height), (160, 90));
        assert_eq!(report.samples.len(), 4);
        assert!(report.audio.is_none(), "a film without audio has no track");
        assert!(report.span_seconds.is_none(), "without audio the duration is unknown instead of invented");
        assert!(report.samples.iter().all(|sample| sample.seconds < 2.0), "samples stay near the start of a video-only file");
        assert!(report.samples.iter().all(|sample| sample.mean_luma > 0.0), "the frames are decoded, not blank");
    }

    #[test]
    fn a_static_scene_is_reported_as_frozen_instead_of_silently_passing() {
        let out = render_fixture("inspect-static.mp4");
        let report = inspect_delivery(&out, 4, 44_100).expect("inspect");
        assert!(!report.is_clean(), "a still scene is reported, not hidden");
        assert!(report.suspects.iter().any(|suspect| suspect.contains("byte-identical")), "{:?}", report.suspects);
    }

    #[test]
    fn mean_luma_reads_the_expected_range() {
        let black = vec![0u8; 4 * 4];
        let white = vec![255u8; 4 * 4];
        assert_eq!(mean_luma(&black), 0.0);
        assert!((mean_luma(&white) - 1.0).abs() < 0.001);
    }

    #[test]
    fn a_silent_track_is_reported_against_the_stated_threshold() {
        let dir = std::env::temp_dir().join("opencut-delivery-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("silence.wav");
        let zeros = vec![0i16; media_types::SAMPLE_RATE as usize * 2];
        media_codec::write_wav(wav.to_str().unwrap(), &zeros, media_types::SAMPLE_RATE).expect("wav");
        let out = dir.join("inspect-silent.mp4");
        let film = serde_json::json!({
            "width": 160, "height": 90, "fps": 15, "duration_frames": 30,
            "scenes": [{ "start": 0, "duration": 30,
                "background": { "kind": "solid", "color": "#203040" }, "items": [] }],
            "audio": { "clips": [{ "src": wav.to_str().unwrap(), "at": 0, "volume": 1.0 }] }
        });
        render_project_with(&film.to_string(), &out, &RenderOptions::default(), &mut |_, _| {}).expect("render");

        let report = inspect_delivery(&out, 4, media_types::SAMPLE_RATE).expect("inspect");
        assert!(report.audio.is_some(), "the silent track is still a track");
        assert!(report.span_seconds.is_some(), "a track gives the samples a real span");
        assert!(
            report.suspects.iter().any(|suspect| suspect.contains("silence threshold")),
            "{:?}",
            report.suspects
        );
    }
}
