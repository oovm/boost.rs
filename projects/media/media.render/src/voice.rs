//! Voice export: the authoritative singing-voice-plus-codec composition.
//!
//! A song is rendered by `media.voice`, quantized to PCM16 and written by
//! `media.codec`. The sample rate is an explicit input contract; file IO is
//! gated behind the `voice-export` feature, mirroring `sfx-export`.

use media_codec::write_wav;
use media_types::Error;
use media_voice::VoiceSpec;

/// Renders `spec` into interleaved stereo float frames without writing a file.
///
/// This is the single synthesis entry for exports and for hosts that need the
/// samples themselves (level metering, DSP checks); [`write_voice`] reuses it,
/// so no caller renders the same song twice.
///
/// # Errors
/// An invalid spec or a zero rate is `invalid_input`.
pub fn voice_samples(spec: &VoiceSpec, sample_rate: u32) -> Result<Vec<[f32; 2]>, Error> {
    if sample_rate == 0 {
        return Err(Error::invalid_input("voice export: sample rate must be positive"));
    }
    media_voice::render(spec, sample_rate as f32)
}

/// Writes already-rendered frames as a PCM16 stereo WAV.
///
/// # Errors
/// A zero rate is `invalid_input`; write failures keep `io`.
pub fn write_voice_frames(frames: &[[f32; 2]], path: &str, sample_rate: u32) -> Result<(), Error> {
    if sample_rate == 0 {
        return Err(Error::invalid_input("voice export: sample rate must be positive"));
    }
    let pcm: Vec<i16> = frames.iter().flat_map(|frame| frame.iter().map(|sample| (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)).collect();
    write_wav(path, &pcm, sample_rate)
}

/// Renders `spec` at `sample_rate` and writes it as a PCM16 stereo WAV.
///
/// # Errors
/// An invalid spec or rate is `invalid_input`; write failures keep `io`.
pub fn write_voice(spec: &VoiceSpec, path: &str, sample_rate: u32) -> Result<(), Error> {
    let samples = voice_samples(spec, sample_rate)?;
    write_voice_frames(&samples, path, sample_rate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use media_codec::parse_wav_pcm16;
    use media_voice::{Syllable, VoiceSpec, Vowel};

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("media-render-voice-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&path).expect("temp dir");
        path
    }

    #[test]
    fn writes_a_decodable_wav_at_the_declared_rate() {
        let dir = temp_dir("single");
        let path = dir.join("song.wav");
        let spec = VoiceSpec::new(vec![
            Syllable { onset: None, vowel: Vowel::A, pitch: 440.0, beats: 1.0 },
            Syllable { onset: None, vowel: Vowel::O, pitch: 330.0, beats: 1.0 },
        ]);
        write_voice(&spec, path.to_str().unwrap(), 48_000).expect("write");
        let wav = parse_wav_pcm16(&std::fs::read(&path).expect("read")).expect("decode");
        assert_eq!(wav.sample_rate, 48_000);
        assert_eq!(wav.frames.len(), (spec.duration_seconds() * 48_000.0).ceil() as usize);
        let peak = wav.frames.iter().flat_map(|f| f.iter()).fold(0.0f32, |a, b| a.max(b.abs()));
        assert!(peak > 0.02, "song too quiet: {peak}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_spec_reproduces_byte_identically_and_invalid_inputs_reject() {
        let dir = temp_dir("repro");
        let spec = VoiceSpec::new(vec![Syllable { onset: None, vowel: Vowel::E, pitch: 392.0, beats: 0.5 }]);
        let a = dir.join("a.wav");
        let b = dir.join("b.wav");
        write_voice(&spec, a.to_str().unwrap(), 44_100).expect("a");
        write_voice(&spec, b.to_str().unwrap(), 44_100).expect("b");
        assert_eq!(std::fs::read(&a).expect("a"), std::fs::read(&b).expect("b"));
        let mut invalid = spec.clone();
        invalid.syllables.clear();
        assert_eq!(write_voice(&invalid, "unused.wav", 44_100).expect_err("empty lyric").code(), "invalid_input");
        assert_eq!(write_voice(&spec, "unused.wav", 0).expect_err("zero rate").code(), "invalid_input");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One synthesis pass feeds both the meter and the file: writing the same
    /// frames must equal the one-shot export byte for byte.
    #[test]
    fn frames_and_one_shot_export_agree() {
        let dir = temp_dir("frames");
        let spec = VoiceSpec::new(vec![Syllable { onset: None, vowel: Vowel::U, pitch: 520.0, beats: 0.5 }]);
        let frames = voice_samples(&spec, 44_100).expect("samples");
        assert_eq!(frames.len(), (spec.duration_seconds() * 44_100.0).ceil() as usize);
        let direct = dir.join("direct.wav");
        let from_frames = dir.join("frames.wav");
        write_voice(&spec, direct.to_str().unwrap(), 44_100).expect("direct");
        write_voice_frames(&frames, from_frames.to_str().unwrap(), 44_100).expect("from frames");
        assert_eq!(std::fs::read(&direct).expect("direct"), std::fs::read(&from_frames).expect("frames"));
        assert_eq!(voice_samples(&spec, 0).expect_err("zero rate").code(), "invalid_input");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
