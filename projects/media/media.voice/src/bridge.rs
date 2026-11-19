//! Running a declared bridged engine as a child process.
//!
//! A bridged backend is an engine with its own host process. This module owns
//! the parts that are the same for every such engine: the command line shape,
//! the word timings the engine writes, and the failure contract.
//!
//! It deliberately does not hide the process: the caller states the program and
//! the arguments. Nothing is downloaded, installed or retried here, and a failing
//! engine returns an error instead of falling back to another voice.
//!
//! Word timings arrive as SRT cues from engines that report them. They are the
//! measured alignment; a cue that only has estimated timing must not claim these
//! numbers.

use media_types::Error;
use serde::{Deserialize, Serialize};

/// One resolved word or phrase boundary reported by the engine.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct WordTiming {
    /// Start in milliseconds from the beginning of the audio.
    pub start_ms: u64,
    /// End in milliseconds.
    pub end_ms: u64,
    /// Text the engine aligned.
    pub text: String,
}

impl WordTiming {
    /// Absolute frame range at `fps`, rounded to the nearest frame.
    #[must_use]
    pub fn frame_range(&self, fps: u32) -> [u32; 2] {
        let fps = u64::from(fps.max(1));
        let start = (self.start_ms * fps + 500) / 1000;
        let end = (self.end_ms * fps + 500) / 1000;
        [start as u32, end.max(start + 1) as u32]
    }
}

/// One program invocation: the executable and its arguments, nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeCommand {
    /// Program to start.
    pub program: String,
    /// Arguments in order.
    pub args: Vec<String>,
}

/// Parses the SRT the engine writes for word boundaries.
///
/// Only the timing and text are read; styling and cue numbering are ignored.
/// A block without a valid `start --> end` line is an error, because a partial
/// alignment that silently drops words would look like a successful run.
pub fn parse_word_srt(body: &str) -> Result<Vec<WordTiming>, Error> {
    let mut timings = Vec::new();
    for block in body.split("\n\n") {
        let mut lines = block.lines().filter(|line| !line.trim().is_empty());
        let Some(header) = lines.find(|line| line.contains("-->")) else {
            continue;
        };
        let (start, end) = header.split_once("-->").ok_or_else(|| Error::invalid_input("word subtitle header is not `start --> end`"))?;
        let start = parse_srt_time(start.trim())?;
        let end = parse_srt_time(end.trim())?;
        let text: String = lines.collect::<Vec<_>>().join(" ");
        if text.trim().is_empty() {
            continue;
        }
        if end <= start {
            return Err(Error::invalid_input(format!("word subtitle `{}` ends before it starts", text.trim())));
        }
        timings.push(WordTiming { start_ms: start, end_ms: end, text: text.trim().to_owned() });
    }
    Ok(timings)
}

/// Parses `HH:MM:SS,mmm` (or `.mmm`) into milliseconds.
fn parse_srt_time(value: &str) -> Result<u64, Error> {
    let normalized = value.replace('.', ",");
    let (clock, millis) = normalized
        .split_once(',')
        .ok_or_else(|| Error::invalid_input(format!("time `{value}` has no milliseconds")))?;
    let mut parts = clock.split(':');
    let (h, m, s) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(m), Some(s), None) => (h, m, s),
        _ => return Err(Error::invalid_input(format!("time `{value}` is not `HH:MM:SS,mmm`"))),
    };
    let parse = |part: &str| part.parse::<u64>().map_err(|_| Error::invalid_input(format!("time `{value}` has a non-numeric field")));
    let millis = millis.parse::<u64>().map_err(|_| Error::invalid_input(format!("time `{value}` has non-numeric milliseconds")))?;
    if millis >= 1000 {
        return Err(Error::invalid_input(format!("time `{value}` has more than 999 milliseconds")));
    }
    Ok(parse(h)? * 3_600_000 + parse(m)? * 60_000 + parse(s)? * 1000 + millis)
}

/// Runs the command and requires the engine to exit successfully.
///
/// Output files are read by the caller with `media.codec`; this function only
/// owns the process: a non-zero exit is an `encode` error carrying the exit
/// code and stderr, and nothing is retried.
pub fn run_bridge(command: &BridgeCommand) -> Result<(), Error> {
    let output = std::process::Command::new(&command.program)
        .args(&command.args)
        .output()
        .map_err(|error| Error::io(format!("cannot start `{}`: {error}", command.program)))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::encode(format!(
            "`{}` exited with {}: {}",
            command.program,
            output.status.code().unwrap_or(-1),
            stderr.trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_timings_come_from_the_engine_srt() {
        let srt = "1\n00:00:00,000 --> 00:00:00,320\n先看\n\n2\n00:00:00,320 --> 00:00:00,860\n请求怎么进来\n\n";
        let timings = parse_word_srt(srt).expect("timings");
        assert_eq!(timings.len(), 2);
        assert_eq!(timings[0].text, "先看");
        assert_eq!(timings[1].end_ms, 860);
        assert_eq!(timings[1].frame_range(30), [10, 26], "0.32s..0.86s at 30 fps");
    }

    #[test]
    fn a_broken_word_cue_is_an_error_instead_of_a_silent_gap() {
        let backwards = "1\n00:00:01,000 --> 00:00:00,500\nx\n\n";
        assert!(parse_word_srt(backwards).is_err());
        let bad_clock = "1\nq --> 00:00:00,500\nx\n\n";
        assert!(parse_word_srt(bad_clock).is_err());
    }

    #[test]
    fn a_failed_engine_reports_its_own_exit_instead_of_falling_back() {
        let command = BridgeCommand { program: "definitely-not-a-real-program-open-cut".to_owned(), args: Vec::new() };
        let error = run_bridge(&command).expect_err("missing program is an error");
        assert!(error.to_string().contains("cannot start"), "{error}");
    }
}
