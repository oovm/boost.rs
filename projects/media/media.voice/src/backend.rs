//! Voice backends as declared plugins.
//!
//! A backend is anything that can produce speech or analysis: the built-in
//! formant engine, a remote service, a local neural model, or a bridged library
//! with its own runtime. The product does not care which; it cares that the
//! backend says what it can do before the work starts.
//!
//! Three rules keep this honest:
//!
//! - Capabilities are declared per feature (`tts`, `svs`, `svc`, `align`).
//!   A backend is never assumed to support everything, and an undeclared
//!   feature, language, voice or option is an explicit error.
//! - Execution location and cost model are part of the declaration. A remote
//!   service states its endpoint identity; a bridged engine states the runtime
//!   it needs so an installer can decide about it.
//! - A request carries a fingerprint of everything that changes the output.
//!   Caching compares fingerprints; a changed text, voice or engine version is
//!   a different result, not a cache hit.
//!
//! Nothing here falls back. An unavailable backend fails with the backend's own
//! name instead of switching to another voice or a paid API.

use media_types::{Error, digest};
use serde::{Deserialize, Serialize};

/// What a backend can be asked to do.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum VoiceFeature {
    /// Text to speech.
    Tts,
    /// Singing voice synthesis from notes.
    Svs,
    /// Singing voice conversion from an existing take.
    Svc,
    /// Forced alignment between text and audio.
    Align,
}

impl VoiceFeature {
    /// Kebab-case name used in declarations and errors.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tts => "tts",
            Self::Svs => "svs",
            Self::Svc => "svc",
            Self::Align => "align",
        }
    }

    /// Parses the declaration spelling.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "tts" => Some(Self::Tts),
            "svs" => Some(Self::Svs),
            "svc" => Some(Self::Svc),
            "align" => Some(Self::Align),
            _ => None,
        }
    }
}

/// Where the work actually runs.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Execution {
    /// In this process, no network.
    Local,
    /// A service the text or audio is sent to.
    Remote {
        /// Service identity, e.g. a vendor name. Not a URL carrying secrets.
        service: String,
        /// Whether the service is reached through an unofficial interface.
        #[serde(default)]
        unofficial: bool,
    },
    /// A library run through a bridge process.
    Bridged {
        /// Declared runtime, e.g. `python>=3.10` or `node>=20`.
        runtime: String,
    },
}

/// How the backend is paid for.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CostModel {
    /// No per-use charge known to the product.
    Free,
    /// Charged per declared unit, e.g. `character` or `second`.
    Metered {
        /// Billing unit.
        unit: String,
    },
    /// The backend cannot state its pricing; a caller decides whether to use it.
    Unknown,
}

/// How precisely a backend can report timing.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum Timing {
    /// No timing information.
    None,
    /// Sentence boundaries only.
    Sentence,
    /// Per-word marks.
    Word,
    /// Per-phoneme marks.
    Phoneme,
}

/// What one backend declares about itself.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BackendCapability {
    /// Stable backend id, e.g. `openvocaloid.formant` or a declared remote service.
    pub id: String,
    /// Engine or library version the declaration refers to.
    pub version: String,
    /// Features this backend implements.
    pub features: Vec<VoiceFeature>,
    /// BCP-47-ish language tags the backend accepts.
    #[serde(default)]
    pub languages: Vec<String>,
    /// Voice ids the backend accepts.
    #[serde(default)]
    pub voices: Vec<String>,
    /// Whether speaking rate can be set.
    #[serde(default)]
    pub rate: bool,
    /// Whether results can be streamed.
    #[serde(default)]
    pub streaming: bool,
    /// Best timing granularity the backend reports.
    #[serde(default = "Timing::none")]
    pub timing: Timing,
    /// Where the work runs.
    pub execution: Execution,
    /// How it is billed.
    pub billing: CostModel,
}

impl Timing {
    fn none() -> Self {
        Self::None
    }
}

impl BackendCapability {
    /// Checks the declaration itself: an id, a version, and at least one
    /// feature are mandatory, plus whatever the execution mode implies.
    pub fn validate(&self) -> Result<(), Error> {
        if self.id.trim().is_empty() {
            return Err(Error::invalid_input("backend id must not be empty"));
        }
        if self.version.trim().is_empty() {
            return Err(Error::invalid_input(format!("backend `{}` declares no version", self.id)));
        }
        if self.features.is_empty() {
            return Err(Error::invalid_input(format!("backend `{}` declares no feature", self.id)));
        }
        match &self.execution {
            Execution::Remote { service, .. } if service.trim().is_empty() => {
                return Err(Error::invalid_input(format!("backend `{}` declares a remote execution without a service name", self.id)));
            }
            Execution::Bridged { runtime } if runtime.trim().is_empty() => {
                return Err(Error::invalid_input(format!("backend `{}` declares a bridge without a runtime", self.id)));
            }
            _ => {}
        }
        if let CostModel::Metered { unit } = &self.billing {
            if unit.trim().is_empty() {
                return Err(Error::invalid_input(format!("backend `{}` declares metered billing without a unit", self.id)));
            }
        }
        Ok(())
    }
}

/// One request against a backend.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VoiceRequest {
    /// Feature being asked for.
    pub feature: VoiceFeature,
    /// Text to speak or align. Never sent to a backend that did not declare the
    /// feature.
    #[serde(default)]
    pub text: Option<String>,
    /// Voice id.
    #[serde(default)]
    pub voice: Option<String>,
    /// Language tag.
    #[serde(default)]
    pub language: Option<String>,
    /// Speaking rate multiplier, `1.0` being the backend's own normal speed.
    #[serde(default)]
    pub speed: Option<f32>,
    /// Ask for a streamed result.
    #[serde(default)]
    pub streaming: bool,
    /// Ask for timing marks.
    #[serde(default)]
    pub timing: bool,
}

/// Checks a request against a declaration without running anything.
///
/// Every mismatch names the backend: unsupported feature, undeclared language
/// or voice, or an option the backend did not claim. This is the only place
/// where "use a different backend instead" could be tempting, and it is not
/// done: the caller decides whether to try another backend.
pub fn check_request(backend: &BackendCapability, request: &VoiceRequest) -> Result<(), Error> {
    backend.validate()?;
    if !backend.features.contains(&request.feature) {
        return Err(Error::unsupported(format!("backend `{}` does not declare `{}`", backend.id, request.feature.as_str())));
    }
    if request.feature == VoiceFeature::Tts {
        if request.text.as_deref().is_none_or(|text| text.trim().is_empty()) {
            return Err(Error::invalid_input(format!("backend `{}` needs text for `tts`", backend.id)));
        }
    }
    if let Some(language) = &request.language {
        if !backend.languages.is_empty() && !backend.languages.iter().any(|declared| declared.eq_ignore_ascii_case(language)) {
            return Err(Error::unsupported(format!("backend `{}` does not declare language `{language}`", backend.id)));
        }
    }
    if let Some(voice) = &request.voice {
        if !backend.voices.is_empty() && !backend.voices.iter().any(|declared| declared == voice) {
            return Err(Error::unsupported(format!("backend `{}` does not declare voice `{voice}`", backend.id)));
        }
    }
    if request.speed.is_some() && !backend.rate {
        return Err(Error::unsupported(format!("backend `{}` does not declare rate control", backend.id)));
    }
    if request.streaming && !backend.streaming {
        return Err(Error::unsupported(format!("backend `{}` does not declare streaming", backend.id)));
    }
    if request.timing && backend.timing == Timing::None {
        return Err(Error::unsupported(format!("backend `{}` reports no timing", backend.id)));
    }
    if let Some(speed) = request.speed {
        if !(0.25..=4.0).contains(&speed) {
            return Err(Error::invalid_input(format!("speed {speed} is outside 0.25..=4.0")));
        }
    }
    Ok(())
}

/// Fingerprint of everything that changes the produced audio.
///
/// Backend id and version are part of it on purpose: upgrading the engine
/// invalidates cached audio even when the request text is unchanged.
#[must_use]
pub fn request_fingerprint(backend: &BackendCapability, request: &VoiceRequest) -> String {
    let mut canonical = String::with_capacity(128);
    canonical.push_str(&backend.id);
    canonical.push('\u{1f}');
    canonical.push_str(&backend.version);
    canonical.push('\u{1f}');
    canonical.push_str(request.feature.as_str());
    canonical.push('\u{1f}');
    canonical.push_str(request.text.as_deref().unwrap_or(""));
    canonical.push('\u{1f}');
    canonical.push_str(request.voice.as_deref().unwrap_or(""));
    canonical.push('\u{1f}');
    canonical.push_str(request.language.as_deref().unwrap_or(""));
    canonical.push('\u{1f}');
    if let Some(speed) = request.speed {
        canonical.push_str(&format!("{speed:.4}"));
    }
    canonical.push('\u{1f}');
    canonical.push(if request.timing { '1' } else { '0' });
    digest(canonical.as_bytes())
}

/// One produced artifact, with the provenance a review page needs.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VoiceArtifact {
    /// Backend id that produced it.
    pub backend: String,
    /// Backend version.
    pub version: String,
    /// Request fingerprint the artifact belongs to.
    pub fingerprint: String,
    /// Output path, relative to the production root.
    pub path: String,
    /// Content digest of the written file.
    pub digest: String,
    /// Length in frames at the stated sample rate, when the backend reports it.
    #[serde(default)]
    pub duration_frames: Option<u32>,
    /// Sample rate the backend produced.
    #[serde(default)]
    pub sample_rate: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridged() -> BackendCapability {
        BackendCapability {
            id: "openvocaloid.bridged".to_owned(),
            version: "1.0.0".to_owned(),
            features: vec![VoiceFeature::Tts],
            languages: vec!["zh-CN".to_owned(), "en-US".to_owned()],
            voices: vec!["zh-CN-Standard".to_owned()],
            rate: true,
            streaming: true,
            timing: Timing::Word,
            execution: Execution::Bridged { runtime: "engine>=1.0".to_owned() },
            billing: CostModel::Free,
        }
    }

    fn tts() -> VoiceRequest {
        VoiceRequest {
            feature: VoiceFeature::Tts,
            text: Some("瑞利散射".to_owned()),
            voice: Some("zh-CN-Standard".to_owned()),
            language: Some("zh-CN".to_owned()),
            speed: Some(1.0),
            streaming: false,
            timing: true,
        }
    }

    #[test]
    fn a_declared_request_passes() {
        check_request(&bridged(), &tts()).expect("the declaration covers the request");
    }

    #[test]
    fn an_undeclared_feature_is_refused_by_name() {
        let mut request = tts();
        request.feature = VoiceFeature::Svs;
        let error = check_request(&bridged(), &request).expect_err("svs is not declared");
        assert_eq!(error.code(), "unsupported_platform");
        assert!(error.message.contains("openvocaloid.bridged") && error.message.contains("svs"), "{}", error.message);
    }

    #[test]
    fn undeclared_language_voice_and_options_are_refused() {
        let backend = bridged();
        let mut language = tts();
        language.language = Some("ja-JP".to_owned());
        assert!(check_request(&backend, &language).expect_err("language").message.contains("ja-JP"));

        let mut voice = tts();
        voice.voice = Some("someone-else".to_owned());
        assert!(check_request(&backend, &voice).expect_err("voice").message.contains("someone-else"));

        let mut quiet = backend.clone();
        quiet.rate = false;
        assert!(check_request(&quiet, &tts()).expect_err("rate").message.contains("rate control"));

        let mut still = backend.clone();
        still.timing = Timing::None;
        assert!(check_request(&still, &tts()).expect_err("timing").message.contains("no timing"));
    }

    #[test]
    fn tts_needs_text() {
        let mut request = tts();
        request.text = Some("   ".to_owned());
        assert!(check_request(&bridged(), &request).expect_err("blank text").message.contains("needs text"));
    }

    #[test]
    fn a_bridge_without_a_runtime_is_invalid() {
        let mut backend = bridged();
        backend.execution = Execution::Bridged { runtime: String::new() };
        let error = backend.validate().expect_err("bridge needs a runtime");
        assert!(error.message.contains("without a runtime"), "{}", error.message);
    }

    #[test]
    fn the_fingerprint_covers_text_voice_and_engine_version() {
        let backend = bridged();
        let base = request_fingerprint(&backend, &tts());

        let mut other_text = tts();
        other_text.text = Some("瑞利散射与波长".to_owned());
        assert_ne!(base, request_fingerprint(&backend, &other_text));

        let mut other_voice = tts();
        other_voice.voice = Some("zh-CN-Warm".to_owned());
        assert_ne!(base, request_fingerprint(&backend, &other_voice));

        let mut upgraded = backend.clone();
        upgraded.version = "1.0.1".to_owned();
        assert_ne!(base, request_fingerprint(&upgraded, &tts()), "an engine upgrade invalidates cached audio");

        assert_eq!(base, request_fingerprint(&backend, &tts()), "the same request is stable");
    }
}
