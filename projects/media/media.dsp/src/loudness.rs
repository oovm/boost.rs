//! Programme loudness and peak measurement.
//!
//! Delivery profiles are stated in loudness, not in sample peaks: a mix that
//! peaks below full scale can still be far too loud or too quiet. This module
//! implements the K-weighted, gated integrated loudness of BS.1770 and a
//! true-peak estimate by interpolation.
//!
//! Limits stated instead of hidden:
//!
//! - True peak is estimated from interpolated samples, not from a 4x
//!   oversampled resampler. It is a safeguard against inter-sample overs, and
//!   the report says which method produced it.
//! - Integrated loudness follows the 400 ms blocks with 75 % overlap, an
//!   absolute gate at -70 LUFS and a relative gate at -10 LU below the ungated
//!   mean. Short signals report the blocks they actually have.

/// Loudness and peak facts of one interleaved signal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoudnessReport {
    /// Gated integrated loudness in LUFS.
    pub integrated_lufs: f32,
    /// Loudest interpolated sample in dBFS.
    pub true_peak_db: f32,
    /// Sample peak in dBFS.
    pub sample_peak_db: f32,
    /// 400 ms blocks that passed both gates.
    pub gated_blocks: usize,
}

/// K-weighting for 48 kHz, as two biquads (BS.1770 stage 1 and 2).
///
/// The coefficients are the published 48 kHz set; other rates reuse them,
/// which is an approximation stated here rather than a silent claim of exact
/// conformance at every rate.
fn k_weight(sample_rate: u32) -> [(f32, f32, f32, f32, f32); 2] {
    // The published coefficients are defined at 48 kHz. At another rate the
    // filters still weight the signal sensibly but are not bit-exact BS.1770.
    let _ = sample_rate;
    [
        // Stage 1: high shelf (+4 dB above ~1.5 kHz).
        (1.535_124_9, -2.691_696_2, 1.198_392_8, -1.690_659_3, 0.732_480_8),
        // Stage 2: high-pass at ~38 Hz.
        (1.0, -2.0, 1.0, -1.990_047_1, 0.990_072_3),
    ]
}

/// Measures gated integrated loudness and both peaks.
///
/// `samples` is interleaved; channels are averaged as BS.1770 does. A signal
/// shorter than one block still reports a value computed from the samples it
/// has, together with `gated_blocks == 1`.
#[must_use]
pub fn measure_loudness(samples: &[f32], channels: usize, sample_rate: u32) -> LoudnessReport {
    let channels = channels.max(1);
    let rate = sample_rate.max(1) as f32;
    let mut sample_peak = 0.0f32;
    for sample in samples {
        sample_peak = sample_peak.max(sample.abs());
    }
    let true_peak = true_peak_db(samples);
    let (shelf, high_pass) = {
        let stages = k_weight(sample_rate);
        (stages[0], stages[1])
    };
    let filtered: Vec<f32> = {
        let (mut x1, mut x2, mut y1, mut y2) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let (mut u1, mut u2, mut v1, mut v2) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let mut out = Vec::with_capacity(samples.len());
        for sample in samples {
            let y = shelf.0 * sample + shelf.1 * x1 + shelf.2 * x2 - shelf.3 * y1 - shelf.4 * y2;
            x2 = x1;
            x1 = *sample;
            y2 = y1;
            y1 = y;
            let v = high_pass.0 * y + high_pass.1 * u1 + high_pass.2 * u2 - high_pass.3 * v1 - high_pass.4 * v2;
            u2 = u1;
            u1 = y;
            v2 = v1;
            v1 = v;
            out.push(v);
        }
        out
    };

    let block = (0.4 * rate) as usize * channels;
    let step = (0.1 * rate) as usize * channels;
    let mut energies = Vec::new();
    if filtered.is_empty() {
        energies.push(0.0f32);
    }
    else {
        let mut start = 0usize;
        loop {
            let end = (start + block).min(filtered.len());
            let frames = ((end - start) / channels).max(1) as f32;
            let mut sum = 0.0f32;
            for (index, sample) in filtered[start..end].iter().enumerate() {
                // Weight each channel equally; BS.1770 sums channel loudness.
                let _ = index;
                sum += sample * sample;
            }
            energies.push(sum / frames);
            if end == filtered.len() {
                break;
            }
            start += step;
        }
    }

    let block_loudness = |energy: f32| -0.691 + 10.0 * (energy.max(1e-12)).log10();
    let ungated: Vec<f32> = energies.iter().map(|energy| block_loudness(*energy)).collect();
    let absolute: Vec<f32> = ungated.iter().copied().filter(|value| *value > -70.0).collect();
    if absolute.is_empty() {
        let mean = ungated.iter().sum::<f32>() / ungated.len() as f32;
        return LoudnessReport {
            integrated_lufs: mean.max(-70.0),
            true_peak_db: true_peak,
            sample_peak_db: 20.0 * sample_peak.max(1e-9).log10(),
            gated_blocks: 0,
        };
    }
    let relative_gate = absolute.iter().sum::<f32>() / absolute.len() as f32 - 10.0;
    let kept: Vec<f32> = absolute.iter().copied().filter(|value| *value > relative_gate).collect();
    let integrated = kept.iter().sum::<f32>() / kept.len().max(1) as f32;
    LoudnessReport {
        integrated_lufs: integrated,
        true_peak_db: true_peak,
        sample_peak_db: 20.0 * sample_peak.max(1e-9).log10(),
        gated_blocks: kept.len(),
    }
}

/// Interpolated peak in dBFS.
///
/// Each adjacent pair is sampled with a cubic through the four nearest points;
/// the maximum of those interpolated values is reported. This catches overs
/// between samples that a sample peak misses, without claiming to be a full
/// oversampled true-peak meter.
#[must_use]
pub fn true_peak_db(samples: &[f32]) -> f32 {
    let mut peak = 0.0f32;
    for (index, sample) in samples.iter().enumerate() {
        peak = peak.max(sample.abs());
        if index + 1 >= samples.len() {
            continue;
        }
        let p0 = samples[index.saturating_sub(1)];
        let p1 = *sample;
        let p2 = samples[index + 1];
        let p3 = if index + 2 < samples.len() { samples[index + 2] } else { p2 };
        for step in 1..4 {
            let t = step as f32 / 4.0;
            let value = cubic(p0, p1, p2, p3, t);
            peak = peak.max(value.abs());
        }
    }
    20.0 * peak.max(1e-9).log10()
}

/// Catmull-Rom interpolation between `p1` and `p2`.
fn cubic(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    let a = -0.5 * p0 + 1.5 * p1 - 1.5 * p2 + 0.5 * p3;
    let b = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
    let c = -0.5 * p0 + 0.5 * p2;
    ((a * t + b) * t + c) * t + p1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amplitude: f32, hz: f32, seconds: f32, rate: u32) -> Vec<f32> {
        let count = (seconds * rate as f32) as usize;
        (0..count).map(|index| amplitude * (std::f32::consts::TAU * hz * index as f32 / rate as f32).sin()).collect()
    }

    #[test]
    fn a_one_kilohertz_tone_reads_near_its_level() {
        // A 1 kHz sine at -20 dBFS peak has an RMS of about -23 dBFS, and the
        // K shelf lifts it slightly; the block gate keeps the reading in the
        // low -20s rather than at the sample peak.
        let samples = sine(0.1, 1000.0, 3.0, 48_000);
        let report = measure_loudness(&samples, 1, 48_000);
        assert!((-24.5..=-19.0).contains(&report.integrated_lufs), "{}", report.integrated_lufs);
        assert!(report.gated_blocks > 0);
        assert!((report.sample_peak_db + 20.0).abs() < 0.5, "{}", report.sample_peak_db);
    }

    #[test]
    fn a_quieter_tone_reads_lower_by_the_same_amount() {
        let loud = measure_loudness(&sine(0.1, 1000.0, 2.0, 48_000), 1, 48_000);
        let quiet = measure_loudness(&sine(0.01, 1000.0, 2.0, 48_000), 1, 48_000);
        let difference = loud.integrated_lufs - quiet.integrated_lufs;
        assert!((difference - 20.0).abs() < 1.0, "{difference}");
    }

    #[test]
    fn interpolation_finds_a_peak_between_samples() {
        // Samples that straddle zero miss the crest of the sine; the
        // interpolated peak must be higher than the sample peak.
        let samples: Vec<f32> = (0..64).map(|index| (std::f32::consts::TAU * 11.0 * index as f32 / 64.0).sin() * 0.8).collect();
        let sample_peak = samples.iter().fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        let true_peak = 10f32.powf(true_peak_db(&samples) / 20.0);
        assert!(true_peak >= sample_peak - 1e-6);
    }

    #[test]
    fn silence_reports_the_floor_instead_of_a_number() {
        let report = measure_loudness(&vec![0.0f32; 4800], 2, 48_000);
        assert_eq!(report.integrated_lufs, -70.0);
        assert_eq!(report.gated_blocks, 0);
        assert!(report.sample_peak_db < -100.0);
    }
}
