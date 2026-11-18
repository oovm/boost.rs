//! Subtitle cues: from a production book to SRT and WebVTT.
//!
//! Cues are authored per shot (`Cue.range` is local to its scene), so the first
//! step is always the same: resolve each cue against the project timeline once.
//! Only then do line breaking, reading-speed checks and serialization run, and
//! every reader sees the same absolute frames the player receives.
//!
//! Rules that keep the output honest:
//!
//! - A cue that is too fast to read is an error naming the cue, not a silent
//!   retiming. The author either shortens the line or accepts a longer range.
//! - Line breaking is deterministic and character-width based; the text itself
//!   is never rewritten.
//! - Both writers share one formatter for timestamps, so SRT and WebVTT cannot
//!   disagree about when a cue starts.

use media_types::{Error, TimeBase};
use serde::{Deserialize, Serialize};

use crate::production::{Cue, CueId, Production};
use crate::scene::Project;

/// Line-breaking and reading-speed limits.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SubtitleStyle {
    /// Maximum characters per line.
    pub max_chars: usize,
    /// Maximum lines per cue.
    pub max_lines: usize,
    /// Shortest cue duration in frames.
    pub min_frames: u32,
    /// Maximum reading speed in characters per second.
    pub max_cps: f32,
}

impl Default for SubtitleStyle {
    fn default() -> Self {
        Self { max_chars: 18, max_lines: 2, min_frames: 12, max_cps: 9.0 }
    }
}

/// One cue resolved to absolute project frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbsoluteCue {
    /// Handle the cue came from.
    pub id: CueId,
    /// Absolute start frame (inclusive).
    pub start: u32,
    /// Absolute end frame (exclusive).
    pub end: u32,
    /// Text as displayed, already line-broken.
    pub lines: Vec<String>,
}

/// Resolves every cue against the project timeline.
///
/// A cue whose shot or scene is missing never reaches this point: the book has
/// to validate first, and this function reports the same problem if called on
/// an unvalidated book.
pub fn absolute_cues(project: &Project, production: &Production, style: &SubtitleStyle) -> Result<Vec<AbsoluteCue>, Error> {
    let mut resolved = Vec::with_capacity(production.cues.len());
    for cue in &production.cues {
        let Some(shot) = production.shots.iter().find(|shot| shot.id == cue.shot) else {
            return Err(Error::invalid_input(format!("cue `{}` references unknown shot `{}`", cue.id, cue.shot)));
        };
        let Some(scene) = project.scenes.iter().find(|scene| scene.id.as_deref() == Some(shot.scene.as_str())) else {
            return Err(Error::invalid_input(format!("shot `{}` references unknown scene `{}`", shot.id, shot.scene)));
        };
        let start = scene.start + cue.range[0];
        let end = scene.start + cue.range[1];
        let lines = wrap_text(&cue.text, style.max_chars, style.max_lines)?;
        resolved.push(AbsoluteCue { id: cue.id.clone(), start, end, lines });
    }
    resolved.sort_by_key(|cue| (cue.start, cue.end));
    Ok(resolved)
}

/// Breaks `text` into at most `max_lines` lines of `max_chars` characters.
///
/// Breaks prefer the punctuation marks a subtitle reader expects (`,，、。；;:
/// and spaces) and fall back to a hard break. More lines than `max_lines` is an
/// error: the caller shortens the line instead of shrinking the type.
pub fn wrap_text(text: &str, max_chars: usize, max_lines: usize) -> Result<Vec<String>, Error> {
    let max_chars = max_chars.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if current.chars().count() < max_chars {
            continue;
        }
        if let Some(cut) = preferred_break(&current) {
            let (head, tail) = current.split_at(cut);
            lines.push(head.trim_end().to_owned());
            current = tail.trim_start().to_owned();
        }
        else {
            lines.push(std::mem::take(&mut current));
        }
    }
    if !current.trim().is_empty() {
        lines.push(current.trim().to_owned());
    }
    if lines.is_empty() {
        return Err(Error::invalid_input("a subtitle cue needs text"));
    }
    if lines.len() > max_lines {
        return Err(Error::invalid_input(format!(
            "`{text}` needs {} lines at {max_chars} characters, more than {max_lines}",
            lines.len()
        )));
    }
    Ok(lines)
}

/// Byte index of a preferred break inside `text`, if one exists.
fn preferred_break(text: &str) -> Option<usize> {
    let mut best = None;
    for (index, ch) in text.char_indices() {
        if matches!(ch, '，' | ',' | '、' | '。' | '；' | ';' | ':' | '：' | ' ') {
            best = Some(index + ch.len_utf8());
        }
    }
    best.filter(|cut| *cut < text.len())
}

/// Checks reading speed and minimum duration, naming the first offending cue.
pub fn check_reading(cues: &[AbsoluteCue], style: &SubtitleStyle, time_base: TimeBase) -> Result<(), Error> {
    for cue in cues {
        let frames = cue.end.saturating_sub(cue.start);
        if frames < style.min_frames {
            return Err(Error::invalid_input(format!(
                "cue `{}` lasts {frames} frames, shorter than the {} frame minimum",
                cue.id, style.min_frames
            )));
        }
        let seconds = time_base.to_seconds(frames).max(0.001) as f32;
        let characters = cue.lines.iter().map(|line| line.chars().count()).sum::<usize>() as f32;
        let cps = characters / seconds;
        if cps > style.max_cps {
            return Err(Error::invalid_input(format!(
                "cue `{}` reads at {cps:.1} characters per second, above the {} limit",
                cue.id, style.max_cps
            )));
        }
    }
    Ok(())
}

/// Serializes cues as SubRip (`.srt`).
pub fn to_srt(cues: &[AbsoluteCue], time_base: TimeBase) -> String {
    let mut out = String::with_capacity(cues.len() * 96);
    for (index, cue) in cues.iter().enumerate() {
        out.push_str(&(index + 1).to_string());
        out.push('\n');
        out.push_str(&format!("{} --> {}\n", srt_time(cue.start, time_base), srt_time(cue.end, time_base)));
        for line in &cue.lines {
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// Serializes cues as WebVTT (`.vtt`).
pub fn to_webvtt(cues: &[AbsoluteCue], time_base: TimeBase) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for cue in cues {
        out.push_str(&format!("{} --> {}\n", vtt_time(cue.start, time_base), vtt_time(cue.end, time_base)));
        for line in &cue.lines {
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// One SRT timestamp, e.g. `00:00:12,480`.
fn srt_time(frames: u32, time_base: TimeBase) -> String {
    let (hours, minutes, seconds, millis) = split_time(frames, time_base);
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}

/// One WebVTT timestamp, e.g. `00:00:12.480`.
fn vtt_time(frames: u32, time_base: TimeBase) -> String {
    let (hours, minutes, seconds, millis) = split_time(frames, time_base);
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

fn split_time(frames: u32, time_base: TimeBase) -> (u64, u64, u64, u64) {
    let millis = (time_base.to_seconds(frames) * 1000.0).round() as u64;
    (millis / 3_600_000, millis / 60_000 % 60, millis / 1000 % 60, millis % 1000)
}

/// Convenience: resolve, check and serialize in one call.
pub fn export(project: &Project, production: &Production, style: &SubtitleStyle, format: SubtitleFormat) -> Result<String, Error> {
    production.validate()?;
    let cues = absolute_cues(project, production, style)?;
    check_reading(&cues, style, project.time_base())?;
    Ok(match format {
        SubtitleFormat::Srt => to_srt(&cues, project.time_base()),
        SubtitleFormat::WebVtt => to_webvtt(&cues, project.time_base()),
    })
}

/// Supported subtitle files.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SubtitleFormat {
    /// SubRip.
    Srt,
    /// WebVTT.
    WebVtt,
}

/// Recomputes cue text after an edit, keeping the identity and timing.
///
/// Editing a line is the common local revision; this returns the new cue so a
/// caller can decide whether the range still fits instead of editing in place
/// and losing the ability to diff.
#[must_use]
pub fn retimed(cue: &Cue, text: String) -> Cue {
    Cue { text, ..cue.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time_base() -> TimeBase {
        TimeBase::from_fps(30)
    }

    #[test]
    fn lines_break_on_punctuation() {
        let lines = wrap_text("先看请求怎么进来，再看它怎么失败", 12, 2).expect("two lines");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with('，'), "{lines:?}");
    }

    #[test]
    fn too_many_lines_is_an_error_instead_of_smaller_type() {
        let error = wrap_text("这是一段明显超过两行容量的中文长句，用来验证换行上限", 8, 2).expect_err("too long");
        assert!(error.message.contains("more than 2"), "{}", error.message);
    }

    #[test]
    fn srt_and_vtt_share_the_same_time_split() {
        let cues = vec![AbsoluteCue { id: CueId::new("c1").unwrap(), start: 0, end: 30, lines: vec!["你好".to_owned()] }];
        assert_eq!(to_srt(&cues, time_base()), "1\n00:00:00,000 --> 00:00:01,000\n你好\n\n");
        assert!(to_webvtt(&cues, time_base()).contains("00:00:00.000 --> 00:00:01.000"));
    }

    #[test]
    fn a_fast_cue_is_refused_with_its_handle() {
        let cues = vec![AbsoluteCue {
            id: CueId::new("crowded").unwrap(),
            start: 0,
            end: 6,
            lines: vec!["这一句太快了根本读不完".to_owned()],
        }];
        let error = check_reading(&cues, &SubtitleStyle::default(), time_base()).expect_err("too fast");
        assert!(error.message.contains("crowded"), "{}", error.message);
    }
}
