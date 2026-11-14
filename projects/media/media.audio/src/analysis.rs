//! Song analysis: read a PCM WAV song into a deterministic profile.
//!
//! This is the neutral home of the structure a music-video flow starts from:
//! short-time energy, onset strength (energy rising edges), autocorrelation
//! tempo detection, beat phase and bar-energy segmentation. Everything is
//! deterministic — no random source and no trained model — and decoding goes
//! through `media.codec`, so every host gets the same answer for the same file.
//!
//! Supported inputs are PCM16, PCM24 and 32-bit float RIFF/WAVE. Other
//! containers are an explicit error: convert first, never guess.

use media_codec::{parse_wav_f32, parse_wav_pcm16, parse_wav_pcm24};
use media_types::Error;
use serde::{Deserialize, Serialize};

const HOP: usize = 512;
const WINDOW: usize = 1024;
const MIN_BPM: f64 = 60.0;
const MAX_BPM: f64 = 180.0;
const BEATS_PER_BAR: u32 = 4;

/// A section type derived from energy and position, not from annotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SectionKind {
    /// Opening low-energy section.
    Intro,
    /// Main low/mid-energy section.
    Verse,
    /// High-energy section, or the relative loudest one.
    Chorus,
    /// Closing low-energy section.
    Outro,
}

/// One detected section: bar-aligned start/end and its peak energy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SongSection {
    /// Stable identifier in section order, `s1`, `s2`, …
    pub id: String,
    /// Derived kind.
    pub kind: SectionKind,
    /// Start in seconds relative to the audio start.
    pub start: f64,
    /// End in seconds relative to the audio start.
    pub end: f64,
    /// Mean energy of the loudest 10% frames, normalized to 0..=1.
    pub energy: f64,
    /// Whole bars covered by this section.
    pub bars: u32,
}

/// The first-hand facts an MV flow consumes: tempo, beat phase and sections.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SongProfile {
    /// Detected tempo in beats per minute, one decimal place.
    pub bpm: f64,
    /// Beats per bar the analysis assumes (4).
    pub beats_per_bar: u32,
    /// Audio length in seconds.
    pub duration_seconds: f64,
    /// Sample rate declared by the file.
    pub sample_rate: u32,
    /// Offset of the first beat from the audio start, in seconds.
    pub beat_offset_seconds: f64,
    /// Detected sections in time order.
    pub sections: Vec<SongSection>,
}

/// Analyzes a PCM WAV song on disk.
///
/// A missing file, an unsupported container/format or an audio shorter than one
/// second is an explicit `invalid_input` error.
pub fn analyze_wav(path: &str) -> Result<SongProfile, Error> {
    let bytes = std::fs::read(path).map_err(|error| Error::invalid_input(format!("wav: cannot read {path}: {error}")))?;
    let (sample_rate, samples) =
        decode_mono(&bytes).ok_or_else(|| Error::invalid_input(format!("wav: {path} is not a PCM16, PCM24 or float32 RIFF/WAVE file")))?;
    analyze_mono(&samples, sample_rate)
}

/// Analyzes already-decoded mono samples at their real sample rate.
///
/// `sample_rate` is the input contract: nothing is resampled, and a rate of 0
/// or audio shorter than one second is rejected instead of guessed around.
pub fn analyze_mono(samples: &[f32], sample_rate: u32) -> Result<SongProfile, Error> {
    if sample_rate == 0 {
        return Err(Error::invalid_input("song: sample rate must be positive"));
    }
    let rate = f64::from(sample_rate);
    let duration_seconds = samples.len() as f64 / rate;
    if duration_seconds < 1.0 {
        return Err(Error::invalid_input(format!("song: need at least 1 second of audio, got {duration_seconds:.2}s")));
    }

    let frames = if samples.len() >= WINDOW { (samples.len() - WINDOW) / HOP + 1 } else { 1 };
    let mut energy = vec![0.0f64; frames];
    for (frame, slot) in energy.iter_mut().enumerate() {
        let start = frame * HOP;
        let mut sum = 0.0f64;
        for offset in 0..WINDOW {
            let sample = f64::from(samples.get(start + offset).copied().unwrap_or(0.0));
            sum += sample * sample;
        }
        *slot = (sum / WINDOW as f64).sqrt();
    }

    let mut onset = vec![0.0f64; frames];
    for frame in 1..frames {
        onset[frame] = (energy[frame] - energy[frame - 1]).max(0.0);
    }

    let (lag, bpm) = detect_tempo(&onset, rate);
    let beat_offset_frames = beat_phase(&onset, lag);
    let bar_frames = lag * BEATS_PER_BAR as usize;
    let sections = segment(&energy, frames, bar_frames, HOP as f64 / rate, duration_seconds, beat_offset_frames);

    Ok(SongProfile {
        bpm,
        beats_per_bar: BEATS_PER_BAR,
        duration_seconds,
        sample_rate,
        beat_offset_seconds: beat_offset_frames as f64 * HOP as f64 / rate,
        sections,
    })
}

/// Decodes a RIFF/WAVE payload into a mono envelope source.
fn decode_mono(bytes: &[u8]) -> Option<(u32, Vec<f32>)> {
    if let Some(wav) = parse_wav_pcm16(bytes) {
        return Some((wav.sample_rate, downmix(&wav.frames)));
    }
    if let Some(wav) = parse_wav_pcm24(bytes) {
        return Some((wav.sample_rate, downmix(&wav.frames)));
    }
    let wav = parse_wav_f32(bytes)?;
    Some((wav.sample_rate, downmix(&wav.frames)))
}

fn downmix(frames: &[[f32; 2]]) -> Vec<f32> {
    frames.iter().map(|[left, right]| 0.5 * (left + right)).collect()
}

fn detect_tempo(onset: &[f64], sample_rate: f64) -> (usize, f64) {
    let min_lag = ((60.0 / MAX_BPM) * sample_rate / HOP as f64).round().max(1.0) as usize;
    let max_lag = (((60.0 / MIN_BPM) * sample_rate / HOP as f64).round() as usize).max(min_lag + 1);
    let mut best_lag = min_lag;
    let mut best_score = -1.0f64;
    for candidate in min_lag..=max_lag {
        let score = autocorrelation(onset, candidate);
        if score > best_score {
            best_score = score;
            best_lag = candidate;
        }
    }
    // Autocorrelation easily lands on half speed: below 80 BPM check the double
    // tempo and take it only when it is clearly better.
    let mut bpm = refined_bpm(onset, best_lag, sample_rate);
    if bpm < 80.0 && best_lag > 2 {
        let double_lag = ((best_lag as f64 / 2.0).round() as usize).max(2);
        let double_score = autocorrelation(onset, double_lag);
        if double_score > best_score * 0.9 {
            best_lag = double_lag;
            bpm = refined_bpm(onset, best_lag, sample_rate);
        }
    }
    (best_lag, (bpm * 10.0).round() / 10.0)
}

/// Parabolic interpolation around the integer lag keeps sub-frame tempo error.
fn refined_bpm(onset: &[f64], lag: usize, sample_rate: f64) -> f64 {
    let left = if lag > 1 { autocorrelation(onset, lag - 1) } else { 0.0 };
    let centre = autocorrelation(onset, lag);
    let right = autocorrelation(onset, lag + 1);
    let denominator = left - 2.0 * centre + right;
    let delta = if denominator == 0.0 { 0.0 } else { (0.5 * (left - right) / denominator).clamp(-0.5, 0.5) };
    60.0 / ((lag as f64 + delta) * HOP as f64 / sample_rate)
}

fn autocorrelation(onset: &[f64], lag: usize) -> f64 {
    let count = onset.len().saturating_sub(lag);
    let mut sum = 0.0f64;
    for index in 0..count {
        sum += onset[index] * onset[index + lag];
    }
    sum / count.max(1) as f64
}

fn beat_phase(onset: &[f64], lag: usize) -> usize {
    if lag == 0 {
        return 0;
    }
    let mut best_phase = 0;
    let mut best_score = -1.0f64;
    for phase in 0..lag {
        let mut score = 0.0f64;
        let mut index = phase;
        while index < onset.len() {
            score += onset[index];
            index += lag;
        }
        if score > best_score {
            best_score = score;
            best_phase = phase;
        }
    }
    best_phase
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Bucket {
    High,
    Low,
    Mid,
}

fn bucket(value: f64) -> Bucket {
    if value > 0.72 {
        Bucket::High
    }
    else if value < 0.45 {
        Bucket::Low
    }
    else {
        Bucket::Mid
    }
}

#[derive(Clone, Copy)]
struct Run {
    from: usize,
    to: usize,
}

fn average(values: &[f64], run: Run) -> f64 {
    let mut sum = 0.0f64;
    for index in run.from..=run.to {
        sum += values.get(index).copied().unwrap_or(0.0);
    }
    sum / (run.to - run.from + 1).max(1) as f64
}

fn segment(
    energy: &[f64],
    frames: usize,
    bar_frames: usize,
    frame_seconds: f64,
    duration_seconds: f64,
    offset_frames: usize,
) -> Vec<SongSection> {
    let bar_count = ((frames as i64 - offset_frames as i64) / bar_frames as i64).max(1) as usize;
    let mut bar_energy = Vec::with_capacity(bar_count);
    for bar in 0..bar_count {
        let start = offset_frames + bar * bar_frames;
        let end = (start + bar_frames).min(frames);
        // The loudest 10% frames per bar: sparse hits are not averaged away and
        // a boundary bar is not flattened by the 50/50 split.
        let mut window: Vec<f64> = if start < end { energy[start..end].to_vec() } else { Vec::new() };
        window.sort_by(|left, right| right.partial_cmp(left).unwrap_or(std::cmp::Ordering::Equal));
        let top = ((window.len() as f64 * 0.1).floor() as usize).max(1);
        let mut sum = 0.0f64;
        for value in window.iter().take(top) {
            sum += value;
        }
        bar_energy.push(sum / top as f64);
    }
    let peak = bar_energy.iter().copied().fold(0.0f64, f64::max).max(1e-9);
    let normalized: Vec<f64> = bar_energy.iter().map(|value| value / peak).collect();

    let mut runs: Vec<Run> = Vec::new();
    for (index, value) in normalized.iter().enumerate() {
        if let Some(last) = runs.last_mut() {
            if bucket(*value) == bucket(average(&normalized, *last)) {
                last.to = index;
                continue;
            }
        }
        runs.push(Run { from: index, to: index });
    }

    // Fold internal runs shorter than two bars into the energy-closer
    // neighbour; the first and last run stay short because short intro/outro
    // sections are real.
    let mut merged = true;
    while merged && runs.len() > 1 {
        merged = false;
        let mut index = 0;
        while index < runs.len() {
            let run = runs[index];
            if run.to - run.from + 1 >= 2 || index == 0 || index == runs.len() - 1 {
                index += 1;
                continue;
            }
            let own = average(&normalized, run);
            let previous = index.checked_sub(1).map(|at| (at, (average(&normalized, runs[at]) - own).abs()));
            let next = (index + 1 < runs.len()).then(|| (index + 1, (average(&normalized, runs[index + 1]) - own).abs()));
            let target = match (previous, next) {
                (None, None) => None,
                (Some(previous), None) => Some(previous.0),
                (None, Some(next)) => Some(next.0),
                (Some(previous), Some(next)) => Some(if previous.1 <= next.1 { previous.0 } else { next.0 }),
            };
            let Some(target) = target
            else {
                break;
            };
            let (from, to) = (run.from, run.to);
            runs[target].from = runs[target].from.min(from);
            runs[target].to = runs[target].to.max(to);
            runs.remove(index);
            merged = true;
            break;
        }
    }

    let mut sections: Vec<SongSection> = runs
        .iter()
        .enumerate()
        .map(|(index, run)| {
            let start = ((offset_frames + run.from * bar_frames) as f64 * frame_seconds).max(0.0);
            let end = ((offset_frames + (run.to + 1) * bar_frames) as f64 * frame_seconds).min(duration_seconds);
            let energy_value = average(&normalized, *run);
            let kind = match bucket(energy_value) {
                Bucket::High => SectionKind::Chorus,
                Bucket::Low if index == 0 => SectionKind::Intro,
                Bucket::Low if index == runs.len() - 1 => SectionKind::Outro,
                _ => SectionKind::Verse,
            };
            SongSection {
                id: format!("s{}", index + 1),
                kind,
                start,
                end,
                energy: (energy_value * 100.0).round() / 100.0,
                bars: (run.to - run.from + 1) as u32,
            }
        })
        .collect();

    // Relative call: the clearly loudest section is the chorus even when no
    // absolute threshold was crossed.
    let mut energies: Vec<f64> = sections.iter().map(|section| section.energy).collect();
    energies.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let median = energies.get(energies.len() / 2).copied().unwrap_or(0.0);
    let mut loudest = 0usize;
    for (index, section) in sections.iter().enumerate() {
        if section.energy > sections[loudest].energy {
            loudest = index;
        }
    }
    let peak_energy = sections[loudest].energy;
    if sections[loudest].kind != SectionKind::Chorus && peak_energy > 0.5 && peak_energy > median * 1.35 {
        sections[loudest].kind = SectionKind::Chorus;
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic click track: a short decaying burst every `beat_seconds`.
    fn click_track(sample_rate: u32, seconds: f64, bpm: f64, accents: &[usize]) -> Vec<f32> {
        let total = (f64::from(sample_rate) * seconds) as usize;
        let mut samples = vec![0.0f32; total];
        let beat_samples = (60.0 / bpm * f64::from(sample_rate)) as usize;
        let mut beat = 0usize;
        while beat * beat_samples < total {
            let loud = accents.is_empty() || accents.contains(&(beat % BEATS_PER_BAR as usize));
            let amplitude = if loud { 0.9 } else { 0.35 };
            let start = beat * beat_samples;
            for offset in 0..(sample_rate as usize / 40) {
                if start + offset >= total {
                    break;
                }
                let decay = 1.0 - offset as f32 / (sample_rate as f32 / 40.0);
                let phase = (offset as f32 * 0.35).sin();
                samples[start + offset] = amplitude * decay * phase;
            }
            beat += 1;
        }
        samples
    }

    #[test]
    fn click_track_tempo_and_sections_are_detected() {
        let samples = click_track(44_100, 8.0, 120.0, &[]);
        let profile = analyze_mono(&samples, 44_100).expect("analysis succeeds");
        assert!((profile.bpm - 120.0).abs() <= 1.0, "bpm {}", profile.bpm);
        assert_eq!(profile.beats_per_bar, 4);
        assert_eq!(profile.sample_rate, 44_100);
        assert!((profile.duration_seconds - 8.0).abs() < 1e-9);
        assert!(!profile.sections.is_empty(), "sections must not be empty");
        let covered: f64 = profile.sections.iter().map(|section| section.end - section.start).sum();
        assert!(covered > 0.0 && covered <= 8.0 + 1e-6, "sections must stay inside the audio");
        for section in &profile.sections {
            assert!(section.start < section.end, "{section:?}");
            assert!((0.0..=1.0).contains(&section.energy), "{section:?}");
            assert!(section.bars >= 1, "{section:?}");
        }
    }

    #[test]
    fn short_audio_is_rejected() {
        let error = analyze_mono(&vec![0.0; 4_000], 8_000).expect_err("under one second");
        assert_eq!(error.code(), "invalid_input");
        assert!(error.message.contains("at least 1 second"), "{error}");
    }

    #[test]
    fn zero_sample_rate_is_rejected() {
        let error = analyze_mono(&vec![0.0; 8_000], 0).expect_err("no rate");
        assert_eq!(error.code(), "invalid_input");
    }

    #[test]
    fn non_wav_bytes_are_rejected() {
        let path = std::env::temp_dir().join("media-audio-analysis-not-a-wav.bin");
        std::fs::write(&path, b"not a riff file").expect("write fixture");
        let error = analyze_wav(&path.to_string_lossy()).expect_err("not a wav");
        assert_eq!(error.code(), "invalid_input");
        assert!(error.message.contains("RIFF/WAVE"), "{error}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pcm16_file_round_trips_through_the_wav_reader() {
        let samples = click_track(44_100, 4.0, 120.0, &[]);
        // `write_wav` takes interleaved stereo, so duplicate the mono envelope.
        let pcm: Vec<i16> = samples
            .iter()
            .flat_map(|sample| {
                let value = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
                [value, value]
            })
            .collect();
        let path = std::env::temp_dir().join("media-audio-analysis-click.pcm16.wav");
        media_codec::write_wav(&path.to_string_lossy(), &pcm, 44_100).expect("write wav");
        let profile = analyze_wav(&path.to_string_lossy()).expect("analysis succeeds");
        assert_eq!(profile.sample_rate, 44_100);
        assert!((profile.duration_seconds - 4.0).abs() < 0.01, "{}", profile.duration_seconds);
        assert!((profile.bpm - 120.0).abs() <= 1.0, "bpm {}", profile.bpm);
        let _ = std::fs::remove_file(&path);
    }
}
