//! Audio level measurement used when recording level events.
//!
//! The harness owns the measurement so every host records the same numbers in
//! the same units. It does not mix, normalize or gate anything.

/// Peak, RMS and clipping facts for one buffer.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LevelStats {
    /// Peak level in dBFS.
    pub peak_db: f32,
    /// RMS level in dBFS.
    pub rms_db: f32,
    /// Number of samples at or beyond full scale.
    pub clipped: usize,
    /// Number of measured samples.
    pub samples: usize,
}

/// Digital silence floor used instead of negative infinity, so a level can be
/// serialized and compared without special cases.
pub const SILENCE_FLOOR_DB: f32 = -160.0;

/// Measures interleaved PCM16 samples at full scale `i16::MAX`.
#[must_use]
pub fn measure_pcm16(samples: &[i16]) -> LevelStats {
    let mut peak = 0.0f32;
    let mut sum_squares = 0.0f64;
    let mut clipped = 0usize;
    for sample in samples {
        let magnitude = f32::from(sample.unsigned_abs()) / 32_768.0;
        peak = peak.max(magnitude);
        sum_squares += (f64::from(*sample) / 32_768.0).powi(2);
        if sample.unsigned_abs() >= 32_767 {
            clipped += 1;
        }
    }
    finish(peak, sum_squares, clipped, samples.len())
}

/// Measures interleaved float samples whose nominal full scale is 1.0.
#[must_use]
pub fn measure_f32(samples: &[f32]) -> LevelStats {
    let mut peak = 0.0f32;
    let mut sum_squares = 0.0f64;
    let mut clipped = 0usize;
    for sample in samples {
        let magnitude = sample.abs();
        peak = peak.max(magnitude);
        sum_squares += f64::from(*sample).powi(2);
        if magnitude >= 1.0 {
            clipped += 1;
        }
    }
    finish(peak, sum_squares, clipped, samples.len())
}

fn finish(peak: f32, sum_squares: f64, clipped: usize, samples: usize) -> LevelStats {
    let rms = if samples == 0 { 0.0 } else { (sum_squares / samples as f64).sqrt() as f32 };
    LevelStats { peak_db: to_db(peak), rms_db: to_db(rms), clipped, samples }
}

fn to_db(amplitude: f32) -> f32 {
    if amplitude <= 0.0 {
        return SILENCE_FLOOR_DB;
    }
    (20.0 * amplitude.log10()).max(SILENCE_FLOOR_DB)
}
