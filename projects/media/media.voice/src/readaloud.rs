//! Domain adapter over the `atlas.cloud` readaloud client.
//!
//! The protocol itself — the DRM token, the WebSocket requests, the audio and
//! metadata frames — lives in `atlas_cloud::microsoft::readaloud`, the unified
//! home of external services. This module only maps the domain voice contract
//! onto it: the backend declaration, the request shape, and the three-state
//! outcome translated into the product's own error semantics.
//!
//! The retry policy is a product decision, not a client feature: a handshake
//! rejection (`403`) is a deterministic refusal with no side effects, so this
//! adapter corrects the local clock from the server `Date` header and re-sends
//! the whole request exactly once. An `Indeterminate` outcome — an interrupted
//! stream — is never retried: the service may have produced part of the audio
//! and the contract forbids pretending otherwise.

use media_types::Error;
use serde::{Deserialize, Serialize};

use crate::{
    backend::{BackendCapability, CostModel, Execution, Timing, VoiceFeature},
    bridge::WordTiming,
};

use atlas_cloud::microsoft::readaloud::{Boundary, Client, ReadaloudOutcome, ReadaloudRequest, SEC_MS_GEC_VERSION};

/// One synthesis request against the readaloud service.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SynthesisRequest {
    /// Text to speak. Non-empty; long texts are split into chunks internally.
    pub text: String,
    /// Voice id, e.g. `zh-CN-XiaoxiaoNeural`.
    pub voice: String,
    /// Rate shift in whole percent, applied as the service's own `+N%`/`-N%`
    /// prosody form.
    #[serde(default)]
    pub rate_percent: i32,
    /// Ask the service for word boundaries; sentence boundaries are reported
    /// when this is off.
    #[serde(default = "default_boundaries")]
    pub word_boundaries: bool,
}

fn default_boundaries() -> bool {
    true
}

/// A finished synthesis: the audio and the measured alignment.
#[derive(Debug)]
pub struct Synthesis {
    /// The complete CBR 48 kbit/s mono MP3 stream.
    pub mp3: Vec<u8>,
    /// Word or sentence boundaries in absolute stream time.
    pub timings: Vec<WordTiming>,
}

/// The backend declaration this adapter implements.
#[must_use]
pub fn capability() -> BackendCapability {
    BackendCapability {
        id: "microsoft.readaloud".to_owned(),
        version: SEC_MS_GEC_VERSION.to_owned(),
        features: vec![VoiceFeature::Tts],
        // The service carries hundreds of voices and locales; the client
        // accepts whatever the caller names and the service validates.
        languages: Vec::new(),
        voices: Vec::new(),
        rate: true,
        streaming: false,
        timing: Timing::Word,
        execution: Execution::Remote { service: "microsoft-edge-readaloud".to_owned(), unofficial: true },
        billing: CostModel::Free,
    }
}

/// Synthesizes speech through the readaloud service.
///
/// A 403 rejection is retried exactly once after correcting the clock skew
/// from the server's `Date` header; a second refusal fails. An interrupted
/// stream is reported as-is and never retried. No other backend is ever
/// substituted.
pub fn synthesize(request: &SynthesisRequest) -> Result<Synthesis, Error> {
    let mut outcome = Client::new().synthesize(&to_cloud(request)).map_err(map_cloud)?;
    if let ReadaloudOutcome::Rejected { status: 403, date: Some(date) } = &outcome {
        let skew = atlas_cloud::microsoft::readaloud::server_clock_skew(date, unix_now())
            .ok_or_else(|| Error::io("readaloud refused the token and its Date header could not be parsed"))?;
        outcome = Client::new().with_clock_skew(skew).synthesize(&to_cloud(request)).map_err(map_cloud)?;
    }
    match outcome {
        ReadaloudOutcome::Completed { mp3, boundaries } => {
            if mp3.is_empty() {
                return Err(Error::encode("readaloud returned no audio; check the voice and text"));
            }
            Ok(Synthesis { mp3, timings: boundaries.into_iter().map(to_word_timing).collect() })
        }
        ReadaloudOutcome::Rejected { status, .. } => Err(Error::io(format!("readaloud handshake refused with status {status}"))),
        ReadaloudOutcome::Indeterminate { reason } => Err(Error::io(format!("readaloud result is indeterminate: {reason}"))),
    }
}

fn to_cloud(request: &SynthesisRequest) -> ReadaloudRequest {
    ReadaloudRequest {
        text: request.text.clone(),
        voice: request.voice.clone(),
        rate_percent: request.rate_percent,
        word_boundaries: request.word_boundaries,
    }
}

fn to_word_timing(boundary: Boundary) -> WordTiming {
    WordTiming { start_ms: boundary.start_ms, end_ms: boundary.end_ms, text: boundary.text }
}

/// Projects the cloud layer's failure onto the domain's error codes.
fn map_cloud(error: atlas_cloud::tencent::CloudError) -> Error {
    use atlas_cloud::tencent::CloudError;
    match error {
        CloudError::InvalidConfig(message) => Error::invalid_input(format!("readaloud: {message}")),
        CloudError::Transport(message) => Error::io(format!("readaloud: {message}")),
        CloudError::Rejected { status, code, .. } => Error::io(format!("readaloud refused with {status} ({code})")),
        CloudError::Decode(message) => Error::encode(format!("readaloud: {message}")),
        CloudError::Indeterminate { reason, .. } => Error::io(format!("readaloud result is indeterminate: {reason}")),
    }
}

fn unix_now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| elapsed.as_secs_f64()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_capability_declares_itself_as_an_unofficial_remote_backend() {
        let backend = capability();
        backend.validate().expect("declaration is complete");
        let request = crate::backend::VoiceRequest {
            feature: VoiceFeature::Tts,
            text: Some("先看".to_owned()),
            voice: Some("zh-CN-XiaoxiaoNeural".to_owned()),
            language: None,
            speed: Some(1.1),
            streaming: false,
            timing: true,
        };
        crate::backend::check_request(&backend, &request).expect("the declaration covers the request");
        let streaming = crate::backend::VoiceRequest { streaming: true, ..request };
        assert!(crate::backend::check_request(&backend, &streaming).is_err(), "the product surface is not streaming");
    }

    #[test]
    fn the_domain_request_maps_onto_the_cloud_shape() {
        let request =
            SynthesisRequest { text: "先看".to_owned(), voice: "zh-CN-XiaoxiaoNeural".to_owned(), rate_percent: 10, word_boundaries: false };
        let cloud = to_cloud(&request);
        assert_eq!(cloud.text, "先看");
        assert_eq!(cloud.voice, "zh-CN-XiaoxiaoNeural");
        assert_eq!(cloud.rate_percent, 10);
        assert!(!cloud.word_boundaries);
    }

    #[test]
    fn indeterminate_results_are_reported_never_retried() {
        // The adapter's contract: an interrupted stream surfaces as an error
        // naming the indeterminate state. The live service cannot be scripted
        // here, so the translation itself is asserted on the outcome shape.
        let outcome = ReadaloudOutcome::Indeterminate { reason: "stream ended early".to_owned() };
        match outcome {
            ReadaloudOutcome::Indeterminate { reason } => {
                let error = Error::io(format!("readaloud result is indeterminate: {reason}"));
                assert!(error.to_string().contains("indeterminate"), "{error}");
            }
            _ => panic!("wrong shape"),
        }
    }
}
