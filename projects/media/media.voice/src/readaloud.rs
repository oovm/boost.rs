//! Native client for the Microsoft Edge readaloud synthesis service.
//!
//! The synthesis algorithm runs on the service; this module owns only its
//! protocol: the opening handshake, the speech-config and SSML requests, the
//! audio frames, and the word-boundary metadata. Everything here is Rust on
//! the workspace network stack (`boost.net` WebSocket over TLS) — no Python,
//! no external binary, and no silent fallback to another voice.
//!
//! The service is an unofficial interface: text is sent to Microsoft's
//! endpoint, and availability or terms can change without notice. The
//! [`capability`] declaration says exactly that (`unofficial: true`), and
//! [`sec_ms_gec`] reproduces the time-windowed token the endpoint requires.
//!
//! Word boundaries arrive as 100-nanosecond ticks relative to the chunk that
//! produced them. Long texts are concatenated into one CBR 48 kbit/s MP3, so
//! a chunk's boundaries are shifted by the audio bytes already collected —
//! computed from the byte count, not from the service's own offset bookkeeping.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use boost_net::ws::{WebSocket, WsConfig, WsMessage};
use media_types::Error;
use serde::{Deserialize, Serialize};

use crate::backend::{BackendCapability, CostModel, Execution, Timing, VoiceFeature};
use crate::bridge::WordTiming;

/// Token the Edge client presents; part of the hashed DRM input.
pub const TRUSTED_CLIENT_TOKEN: &str = "6A5AA1D4EAFF4E9FB37E23D68491D6F4";
/// Protocol version the DRM token is computed for.
pub const SEC_MS_GEC_VERSION: &str = "2-143.0.3650.75";
const WSS_BASE: &str = "wss://speech.platform.bing.com/consumer/speech/synthesize/readaloud/edge/v1";
const ORIGIN: &str = "chrome-extension://jdiccldimpdaibmpdkjnbmckianbfold";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36 Edg/143.0.0.0";
const OUTPUT_FORMAT: &str = "audio-24khz-48kbitrate-mono-mp3";
/// Bits per second of the declared output format; the CBR assumption behind
/// the offset compensation.
const MP3_BITRATE_BPS: u64 = 48_000;
/// Ticks per second in the service's 100-nanosecond timing unit.
const TICKS_PER_SECOND: u64 = 10_000_000;
/// Maximum escaped bytes per SSML request, matching the service's limit.
const MAX_CHUNK_BYTES: usize = 4096;
/// Seconds between the Unix epoch and the Windows FILETIME epoch.
const WIN_EPOCH: f64 = 11_644_473_600.0;

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

/// Why opening the synthesis connection failed.
#[derive(Debug)]
pub enum TransportError {
    /// The service refused the DRM token. The server `Date` header, when it
    /// was sent, is the input for clock-skew correction.
    Forbidden {
        /// Value of the `Date` response header, when present.
        date: Option<String>,
    },
    /// Any other failure, already described.
    Failed(Error),
}

/// The backend declaration this client implements.
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

/// Current time as fractional Unix seconds.
fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs_f64()).unwrap_or_default()
}

/// Hexadecimal of `bytes`, uppercase.
fn hex_upper(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

/// The `Sec-MS-GEC` token for one Unix timestamp, as the service computes it.
///
/// The token is `<cs64>-<sha256>` over the ASCII text
/// `{filetime_ticks}{TRUSTED_CLIENT_TOKEN}`, where the tick count is the time
/// rounded down to a five-minute window in 100-nanosecond intervals since the
/// Windows epoch. The SHA-256 half is a plain hash of that text behind a
/// 57-byte zero prefix; the CS64 half is a keyed universal hash over the text
/// and the digest's first eight bytes.
#[must_use]
pub fn sec_ms_gec(unix_seconds: f64) -> String {
    let mut ticks = unix_seconds + WIN_EPOCH;
    ticks -= ticks % 300.0;
    ticks *= 1e7;
    let message = format!("{ticks:.0}{TRUSTED_CLIENT_TOKEN}");
    let bytes = message.as_bytes();
    let mut preimage = vec![0u8; 57];
    preimage.extend_from_slice(bytes);
    let digest = boost_crypto::hash::sha256(&preimage);
    let cs64 = cs64_digest(bytes, &digest);
    format!("{}-{}", hex_upper(&cs64), hex_upper(&digest))
}

fn rol16(value: u32) -> u32 {
    value.rotate_left(16)
}

/// Little-endian 32-bit words of `data`, zero-padded to an even count.
fn words_le4_even(data: &[u8]) -> Vec<u32> {
    let mut padded = data.to_vec();
    while padded.len() % 4 != 0 {
        padded.push(0);
    }
    let mut words: Vec<u32> = padded.chunks_exact(4).map(|chunk| u32::from_le_bytes(chunk.try_into().expect("four-byte chunk"))).collect();
    if words.len() % 2 == 1 {
        words.push(0);
    }
    words
}

const CS64_COEFFICIENTS: [(u32, u32); 14] = [
    (0xE82E_91B1, 0xB327_00D0),
    (0x44DE_24C6, 0x88DF_F51D),
    (0x3BEB_65BC, 0x2951_4038),
    (0xC89C_5933, 0xB328_CBDF),
    (0x8D1E_C211, 0xD405_B4D5),
    (0x314E_F5D5, 0x26E5_5F3C),
    (0xE924_91B1, 0x6078_01C6),
    (0xF0A5_B9D5, 0x82B2_C1FB),
    (0x0606_4CF3, 0xFA51_A3DA),
    (0xDA79_FA60, 0x207C_C464),
    (0xBED2_0560, 0x3C3C_74F3),
    (0x89F9_C9F8, 0xD2D1_4B61),
    (0x35FA_38C6, 0xBB04_A2C1),
    (0xCB49_E3C6, 0xC562_6167),
];
const CS64_FACTOR: u32 = 0x8289_3933;
const CS64_ADJUSTMENT0: u32 = 0xF2F9_0B1A;
const CS64_ADJUSTMENT1: u32 = 0x0B1A_0000;

/// The CS64 half of the token: eight bytes, as the service derives them.
fn cs64_digest(data: &[u8], digest: &[u8; 32]) -> [u8; 8] {
    let key_a = u32::from_le_bytes(digest[0..4].try_into().expect("four-byte key")) | 1;
    let key_b = u32::from_le_bytes(digest[4..8].try_into().expect("four-byte key")) | 1;
    let factors = [key_a.wrapping_mul(CS64_FACTOR), key_b.wrapping_mul(CS64_FACTOR)];
    let mut terms: Vec<u32> = CS64_COEFFICIENTS.iter().enumerate().map(|(index, (coefficient, _))| coefficient.wrapping_mul(factors[index % 2])).collect();
    let mut offsets: Vec<u32> = CS64_COEFFICIENTS.iter().map(|(_, offset)| *offset).collect();
    let words = words_le4_even(&[&[0u8][..], data].concat());
    for (index, word) in words.iter().enumerate() {
        let mut term = word.wrapping_mul(if index % 2 == 0 { key_a } else { key_b });
        let mut offset = rol16(*word).wrapping_mul(if index % 2 == 0 { 0xEF05_69FB } else { 0xC317_13DB });
        if index == 0 {
            term = term.wrapping_add(CS64_ADJUSTMENT0.wrapping_mul(factors[0]));
            offset = offset.wrapping_add(CS64_ADJUSTMENT1);
        }
        terms.push(term);
        offsets.push(offset);
    }
    let mut swap_acc = 0u32;
    let mut swap_last = 0u32;
    let mut state = 0u32;
    let mut acc = 0u32;
    for pair in 0..terms.len() / 2 {
        let i = pair * 2;
        let (x, y) = (terms[i], terms[i + 1]);
        let t = rol16(offsets[i].wrapping_add(x));
        let q = t.wrapping_mul(0x76F5_0F34);
        swap_acc = swap_acc.wrapping_add(rol16(q).wrapping_mul(0xC3EF_EA97).wrapping_add(q));
        let t = rol16(offsets[i + 1].wrapping_add(y));
        let q = t.wrapping_mul(0x1881_CE3C);
        swap_last = rol16(q).wrapping_mul(0x35BD_1EC9).wrapping_add(q);
        swap_acc = swap_acc.wrapping_add(swap_last);

        state = state.wrapping_mul(key_a).wrapping_add(x);
        state = rol16(state).wrapping_mul(0xCF98_B111);
        state = rol16(state).wrapping_mul(0x8708_5B9F);
        state = rol16(state).wrapping_mul(0x12CE_B96D);
        state = rol16(state).wrapping_mul(0x257E_1D83);
        acc = acc.wrapping_add(state);
        let mut t = state.wrapping_mul(key_b).wrapping_add(y);
        t = rol16(t).wrapping_mul(0xA274_16F5);
        t = rol16(t).wrapping_mul(0xD383_96FF);
        state = rol16(rol16(t).wrapping_mul(0x7C93_2B89)).wrapping_mul(0xBFA4_9F69);
        acc = acc.wrapping_add(state);
    }
    let mut out = [0u8; 8];
    out[0..4].copy_from_slice(&(swap_last ^ state).to_le_bytes());
    out[4..8].copy_from_slice(&(swap_acc ^ acc).to_le_bytes());
    out
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Calendar fields of a day count since the Unix epoch (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Inverse of [`civil_from_days`].
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The service's `X-Timestamp` form, as JavaScript renders it in UTC.
fn js_date(unix_seconds: f64) -> String {
    let secs = unix_seconds.floor() as i64;
    let days = secs.div_euclid(86_400);
    let clock = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let weekday = WEEKDAYS[(((days % 7) + 11) % 7) as usize];
    format!(
        "{weekday} {} {day:02} {year:04} {:02}:{:02}:{:02} GMT+0000 (Coordinated Universal Time)",
        MONTHS[(month - 1) as usize],
        clock / 3600,
        (clock % 3600) / 60,
        clock % 60
    )
}

/// Parses the `Date` header of a rejection into Unix seconds.
fn parse_http_date(value: &str) -> Option<f64> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = MONTHS.iter().position(|name| name.eq_ignore_ascii_case(parts[2]))? as i64 + 1;
    let year: i64 = parts[3].parse().ok()?;
    let clock: Vec<i64> = parts[4].split(':').map(str::parse).collect::<Result<_, _>>().ok()?;
    if clock.len() != 3 {
        return None;
    }
    Some(days_from_civil(year, month, day) as f64 * 86_400.0 + (clock[0] * 3600 + clock[1] * 60 + clock[2]) as f64)
}

/// Strips control ranges the service rejects and escapes XML markup.
pub fn escape_text(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| {
            let code = c as u32;
            if code <= 8 || (11..=12).contains(&code) || (14..=31).contains(&code) { ' ' } else { c }
        })
        .collect();
    cleaned.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Reverses the XML entities the service wraps around boundary text.
fn unescape_xml(text: &str) -> String {
    text.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// The service's prosody rate form for a whole-percent shift.
fn rate_string(percent: i32) -> String {
    if percent < 0 { format!("{percent}%") } else { format!("+{percent}%") }
}

/// Builds the SSML document for one chunk of already-escaped text.
#[must_use]
pub fn build_ssml(voice: &str, rate_percent: i32, escaped_text: &str) -> String {
    format!(
        "<speak version='1.0' xmlns='http://www.w3.org/2001/10/synthesis' xml:lang='en-US'>\
         <voice name='{voice}'><prosody pitch='+0Hz' rate='{}' volume='+0%'>{escaped_text}</prosody></voice>\
         </speak>",
        rate_string(rate_percent)
    )
}

/// The speech-config request that selects the format and boundary granularity.
#[must_use]
pub fn speech_config(word_boundaries: bool, timestamp: &str) -> String {
    let (wd, sq) = if word_boundaries { ("true", "false") } else { ("false", "true") };
    format!(
        "X-Timestamp:{timestamp}\r\nContent-Type:application/json; charset=utf-8\r\nPath:speech.config\r\n\r\n\
         {{\"context\":{{\"synthesis\":{{\"audio\":{{\"metadataoptions\":\
         {{\"sentenceBoundaryEnabled\":\"{sq}\",\"wordBoundaryEnabled\":\"{wd}\"}},\
         \"outputFormat\":\"{OUTPUT_FORMAT}\"}}}}}}}}\r\n"
    )
}

/// Wraps an SSML document in the request headers the service expects.
#[must_use]
pub fn ssml_request(request_id: &str, timestamp: &str, ssml: &str) -> String {
    format!("X-RequestId:{request_id}\r\nContent-Type:application/ssml+xml\r\nX-Timestamp:{timestamp}Z\r\nPath:ssml\r\n\r\n{ssml}")
}

/// Splits escaped text into chunks of at most `byte_length` bytes, preferring
/// line and word boundaries, never cutting a UTF-8 sequence or an XML entity.
pub fn split_chunks(escaped: &str, byte_length: usize) -> Result<Vec<String>, Error> {
    if byte_length == 0 {
        return Err(Error::invalid_input("chunk byte length must be positive"));
    }
    let mut text = escaped.as_bytes();
    let mut chunks = Vec::new();
    while text.len() > byte_length {
        let boundary = last_byte_within(text, b'\n', byte_length)
            .or_else(|| last_byte_within(text, b' ', byte_length))
            .unwrap_or_else(|| utf8_safe_split(text, byte_length));
        let split_at = adjust_for_entity(text, boundary);
        if split_at == 0 {
            return Err(Error::invalid_input("text cannot be split within the service byte limit"));
        }
        let chunk = String::from_utf8_lossy(&text[..split_at]).trim().to_owned();
        if !chunk.is_empty() {
            chunks.push(chunk);
        }
        text = &text[split_at..];
    }
    let tail = String::from_utf8_lossy(text).trim().to_owned();
    if !tail.is_empty() {
        chunks.push(tail);
    }
    Ok(chunks)
}

fn last_byte_within(text: &[u8], needle: u8, limit: usize) -> Option<usize> {
    text[..limit.min(text.len())].iter().rposition(|byte| *byte == needle)
}

/// The largest index ≤ `limit` that does not cut a UTF-8 sequence.
fn utf8_safe_split(text: &[u8], limit: usize) -> usize {
    let mut end = limit.min(text.len());
    while end > 0 && matches!(text.get(end), Some(&byte) if (byte & 0xC0) == 0x80) {
        end -= 1;
    }
    end
}

/// Moves a split point before an unterminated `&` so entities stay whole.
fn adjust_for_entity(text: &[u8], mut split_at: usize) -> usize {
    while split_at > 0 {
        let Some(ampersand) = text[..split_at].iter().rposition(|byte| *byte == b'&') else { break };
        if text[ampersand..split_at].contains(&b';') {
            break;
        }
        split_at = ampersand;
    }
    split_at
}

/// One header block plus payload from a service message.
#[derive(Debug)]
pub struct ServiceMessage {
    /// The `Path:` header value.
    pub path: String,
    /// Everything after the header separator.
    pub payload: String,
}

/// Parses a text frame into its path and body.
pub fn parse_text_message(message: &str) -> Result<ServiceMessage, Error> {
    let (head, payload) = message.split_once("\r\n\r\n").ok_or_else(|| Error::encode("readaloud text message has no header separator"))?;
    let path = header_value(head, "Path").ok_or_else(|| Error::encode("readaloud text message has no Path header"))?;
    Ok(ServiceMessage { path: path.to_owned(), payload: payload.trim_start().to_owned() })
}

/// Parses a binary frame into its headers and payload.
pub fn parse_binary_message(bytes: &[u8]) -> Result<(Vec<(String, String)>, Vec<u8>), Error> {
    if bytes.len() < 2 {
        return Err(Error::encode("readaloud binary frame is missing its header length"));
    }
    let header_length = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
    if bytes.len() < 2 + header_length {
        return Err(Error::encode("readaloud binary header exceeds the frame"));
    }
    let head = std::str::from_utf8(&bytes[2..2 + header_length]).map_err(|_| Error::encode("readaloud binary header is not UTF-8"))?;
    let headers = head
        .split("\r\n")
        .filter_map(|line| line.split_once(':').map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned())))
        .collect();
    Ok((headers, bytes[2 + header_length..].to_vec()))
}

fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.split("\r\n").find_map(|line| line.split_once(':').filter(|(key, _)| *key == name).map(|(_, value)| value.trim()))
}

#[derive(Deserialize)]
struct MetadataEnvelope {
    #[serde(rename = "Metadata")]
    items: Vec<MetadataItem>,
}

#[derive(Deserialize)]
struct MetadataItem {
    #[serde(rename = "Type")]
    kind: String,
    #[serde(rename = "Data", default)]
    data: Option<MetadataData>,
}

#[derive(Deserialize)]
struct MetadataData {
    #[serde(rename = "Offset")]
    offset: u64,
    #[serde(rename = "Duration")]
    duration: u64,
    #[serde(rename = "text")]
    text: MetadataText,
}

#[derive(Deserialize)]
struct MetadataText {
    #[serde(rename = "Text")]
    text: String,
}

/// Converts one `audio.metadata` body into stream-absolute timings.
///
/// `compensation_ticks` shifts chunk-local offsets onto the concatenated
/// stream: the audio bytes already collected from previous chunks.
pub fn parse_metadata(body: &str, compensation_ticks: u64) -> Result<Vec<WordTiming>, Error> {
    let envelope: MetadataEnvelope = serde_json::from_str(body).map_err(|error| Error::encode(format!("readaloud metadata is not valid JSON: {error}")))?;
    let mut timings = Vec::new();
    for item in envelope.items {
        match item.kind.as_str() {
            "WordBoundary" | "SentenceBoundary" => {
                let data = item.data.ok_or_else(|| Error::encode("readaloud boundary carries no data"))?;
                let start_ms = (data.offset + compensation_ticks) / (TICKS_PER_SECOND / 1000);
                let end_ms = (data.offset + data.duration + compensation_ticks) / (TICKS_PER_SECOND / 1000);
                timings.push(WordTiming { start_ms, end_ms: end_ms.max(start_ms + 1), text: unescape_xml(&data.text.text) });
            }
            "SessionEnd" => {}
            other => return Err(Error::encode(format!("unknown readaloud metadata type `{other}`"))),
        }
    }
    if timings.is_empty() {
        return Err(Error::encode("readaloud metadata carried no boundary"));
    }
    Ok(timings)
}

/// One frame received from the service.
#[derive(Debug)]
pub enum Incoming {
    /// A text frame.
    Text(String),
    /// A binary frame.
    Binary(Vec<u8>),
}

/// The transport the synthesis loop drives; a seam so the protocol is testable
/// without a network.
pub trait Transport {
    /// Sends one text message.
    fn send(&mut self, message: &str) -> Result<(), Error>;
    /// Reads the next audio or text frame.
    fn recv(&mut self) -> Result<Incoming, Error>;
}

/// The live transport: a `boost.net` WebSocket over TLS.
pub struct WsTransport {
    socket: WebSocket,
}

impl WsTransport {
    /// Opens one synthesis connection with a token minted at
    /// `unix_now() + skew_seconds`.
    fn open(skew_seconds: f64) -> Result<Self, TransportError> {
        let token = sec_ms_gec(unix_now() + skew_seconds);
        let connection_id = match random_hex(false) {
            Ok(id) => id,
            Err(error) => return Err(TransportError::Failed(error)),
        };
        let url = format!(
            "{WSS_BASE}?TrustedClientToken={TRUSTED_CLIENT_TOKEN}&ConnectionId={connection_id}&Sec-MS-GEC={token}&Sec-MS-GEC-Version={SEC_MS_GEC_VERSION}"
        );
        let parsed = match boost_net::Url::parse(&url) {
            Ok(parsed) => parsed,
            Err(error) => return Err(TransportError::Failed(Error::io(format!("cannot parse the readaloud URL: {error}")))),
        };
        let mut config = WsConfig::default();
        config.timeout = Duration::from_secs(60);
        let muid = match random_hex(true) {
            Ok(muid) => muid,
            Err(error) => return Err(TransportError::Failed(error)),
        };
        config.headers = vec![
            ("User-Agent".to_owned(), USER_AGENT.to_owned()),
            ("Accept-Language".to_owned(), "en-US,en;q=0.9".to_owned()),
            ("Pragma".to_owned(), "no-cache".to_owned()),
            ("Cache-Control".to_owned(), "no-cache".to_owned()),
            ("Origin".to_owned(), ORIGIN.to_owned()),
            ("Cookie".to_owned(), format!("muid={muid};")),
        ];
        match WebSocket::connect(&parsed, config) {
            Ok(socket) => Ok(Self { socket }),
            Err(boost_net::NetError::Rejected { status: 403, date }) => Err(TransportError::Forbidden { date }),
            Err(error) => Err(TransportError::Failed(Error::io(format!("readaloud transport: {error}")))),
        }
    }
}

impl Transport for WsTransport {
    fn send(&mut self, message: &str) -> Result<(), Error> {
        self.socket.send(WsMessage::Text(message.to_owned())).map_err(|error| Error::io(format!("readaloud transport: {error}")))
    }

    fn recv(&mut self) -> Result<Incoming, Error> {
        loop {
            match self.socket.recv() {
                Ok(WsMessage::Text(text)) => return Ok(Incoming::Text(text)),
                Ok(WsMessage::Binary(binary)) => return Ok(Incoming::Binary(binary)),
                Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) => continue,
                Ok(WsMessage::Close { code, reason }) => return Err(Error::io(format!("readaloud socket closed with {code:?}: {reason}"))),
                Err(error) => return Err(Error::io(format!("readaloud transport: {error}"))),
            }
        }
    }
}

fn random_hex(uppercase: bool) -> Result<String, Error> {
    let mut bytes = [0u8; 16];
    boost_random::Secure::system().fill(&mut bytes).map_err(|error| Error::io(format!("cannot read the system random source: {error}")))?;
    let hex = hex_upper(&bytes);
    Ok(if uppercase { hex } else { hex.to_lowercase() })
}

/// Synthesizes with the live service.
///
/// A 403 rejection is retried exactly once after correcting the clock skew
/// from the server's `Date` header; anything else fails. No other backend is
/// ever substituted.
pub fn synthesize(request: &SynthesisRequest) -> Result<Synthesis, Error> {
    synthesize_with(request, WsTransport::open)
}

/// Synthesizes through an injected transport factory, one connection per
/// chunk, with the same retry and compensation rules as the live path.
pub fn synthesize_with<F, T>(request: &SynthesisRequest, mut open: F) -> Result<Synthesis, Error>
where
    F: FnMut(f64) -> Result<T, TransportError>,
    T: Transport,
{
    if request.text.trim().is_empty() {
        return Err(Error::invalid_input("readaloud synthesis needs text"));
    }
    if request.voice.is_empty() || !request.voice.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(Error::invalid_input(format!("voice `{}` is not a valid readaloud voice id", request.voice)));
    }
    if !(-100..=100).contains(&request.rate_percent) {
        return Err(Error::invalid_input(format!("rate {}% is outside -100..=100", request.rate_percent)));
    }
    let chunks = split_chunks(&escape_text(&request.text), MAX_CHUNK_BYTES)?;
    if chunks.is_empty() {
        return Err(Error::invalid_input("the text produced nothing to speak"));
    }

    let mut mp3 = Vec::new();
    let mut timings = Vec::new();
    for chunk in &chunks {
        let mut transport = open_connection(&mut open)?;
        let mut chunk_audio: u64 = 0;
        let timestamp = js_date(unix_now());
        transport.send(&speech_config(request.word_boundaries, &timestamp))?;
        let request_id = random_hex(false)?;
        transport.send(&ssml_request(&request_id, &timestamp, &build_ssml(&request.voice, request.rate_percent, chunk)))?;
        loop {
            match transport.recv()? {
                Incoming::Binary(bytes) => {
                    let (headers, payload) = parse_binary_message(&bytes)?;
                    let path = headers.iter().find(|(key, _)| key == "Path").map(|(_, value)| value.as_str());
                    if path != Some("audio") {
                        return Err(Error::encode("readaloud sent a binary frame that is not audio"));
                    }
                    match headers.iter().find(|(key, _)| key == "Content-Type").map(|(_, value)| value.as_str()) {
                        None if payload.is_empty() => continue,
                        None => return Err(Error::encode("readaloud audio frame without a content type still carried data")),
                        Some("audio/mpeg") if payload.is_empty() => return Err(Error::encode("readaloud audio frame carries no audio")),
                        Some("audio/mpeg") => {
                            chunk_audio += payload.len() as u64;
                            mp3.extend_from_slice(&payload);
                        }
                        Some(other) => return Err(Error::encode(format!("readaloud sent audio as `{other}`"))),
                    }
                }
                Incoming::Text(message) => {
                    let parsed = parse_text_message(&message)?;
                    match parsed.path.as_str() {
                        "audio.metadata" => {
                            let previous_bytes = mp3.len() as u64 - chunk_audio;
                            let compensation = previous_bytes * 8 * TICKS_PER_SECOND / MP3_BITRATE_BPS;
                            timings.extend(parse_metadata(&parsed.payload, compensation)?);
                        }
                        "turn.end" => break,
                        "turn.start" | "response" => {}
                        other => return Err(Error::encode(format!("unknown readaloud path `{other}`"))),
                    }
                }
            }
        }
    }
    if mp3.is_empty() {
        return Err(Error::encode("readaloud returned no audio; check the voice and text"));
    }
    Ok(Synthesis { mp3, timings })
}

fn open_connection<F, T>(open: &mut F) -> Result<T, Error>
where
    F: FnMut(f64) -> Result<T, TransportError>,
{
    match open(0.0) {
        Ok(transport) => Ok(transport),
        Err(TransportError::Forbidden { date: Some(date) }) => {
            let server_seconds =
                parse_http_date(&date).ok_or_else(|| Error::io("readaloud refused the token and its Date header could not be parsed"))?;
            let skew = server_seconds - unix_now();
            match open(skew) {
                Ok(transport) => Ok(transport),
                Err(TransportError::Forbidden { .. }) => Err(Error::io("readaloud refused the token even after clock-skew correction")),
                Err(TransportError::Failed(error)) => Err(error),
            }
        }
        Err(TransportError::Forbidden { date: None }) => Err(Error::io("readaloud refused the token (403) without a server date")),
        Err(TransportError::Failed(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    #[test]
    fn the_sha_half_of_the_token_matches_an_independent_digest() {
        // For unix 1_750_000_000 the rounded FILETIME is 133944735000000000;
        // the 57-zero-prefixed SHA-256 of its ASCII text plus the trusted
        // client token was computed outside this crate.
        let token = sec_ms_gec(1_750_000_000.0);
        let (cs64, sha) = token.split_once('-').expect("cs64-sha form");
        assert_eq!(sha, "8079ED27E1BF2707B0ADD0AE3C35D810C9E08778384B7AFE242854C49E57A4FE");
        assert_eq!(cs64.len(), 16);
        assert!(cs64.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
    }

    #[test]
    fn the_token_rounds_to_the_five_minute_window() {
        let base = sec_ms_gec(1_750_000_000.0);
        assert_eq!(base, sec_ms_gec(1_750_000_099.0), "the same window must produce the same token");
        assert_ne!(base, sec_ms_gec(1_750_000_200.0), "the next window must produce a new one");
    }

    #[test]
    fn the_service_date_forms_round_trip() {
        assert_eq!(js_date(1_795_166_130.0), "Fri Nov 20 2026 09:15:30 GMT+0000 (Coordinated Universal Time)");
        assert_eq!(parse_http_date("Fri, 20 Nov 2026 09:15:30 GMT"), Some(1_795_166_130.0));
    }

    #[test]
    fn control_ranges_become_spaces_and_markup_is_escaped() {
        assert_eq!(escape_text("a\u{0B}b"), "a b");
        assert_eq!(escape_text("a<b>&c"), "a&lt;b&gt;&amp;c");
    }

    #[test]
    fn the_requests_are_built_exactly_as_the_service_expects() {
        let ssml = build_ssml("zh-CN-XiaoxiaoNeural", 10, "先看");
        assert_eq!(
            ssml,
            "<speak version='1.0' xmlns='http://www.w3.org/2001/10/synthesis' xml:lang='en-US'>\
             <voice name='zh-CN-XiaoxiaoNeural'><prosody pitch='+0Hz' rate='+10%' volume='+0%'>先看</prosody></voice>\
             </speak>"
        );
        assert_eq!(rate_string(-5), "-5%");
        let config = speech_config(true, "T");
        assert!(config.starts_with("X-Timestamp:T\r\nContent-Type:application/json; charset=utf-8\r\nPath:speech.config\r\n\r\n"));
        assert!(config.contains("\"wordBoundaryEnabled\":\"true\""));
        assert!(config.contains("\"sentenceBoundaryEnabled\":\"false\""));
        assert!(config.contains(OUTPUT_FORMAT));
        assert!(config.ends_with("\r\n"));
        let request = ssml_request("abc", "T", "<speak/>");
        assert_eq!(request, "X-RequestId:abc\r\nContent-Type:application/ssml+xml\r\nX-Timestamp:TZ\r\nPath:ssml\r\n\r\n<speak/>");
    }

    #[test]
    fn chunks_respect_the_byte_limit_without_cutting_text() {
        let spaced = "x".repeat(2000) + " " + &"y".repeat(2000) + " " + &"z".repeat(2000);
        let chunks = split_chunks(&spaced, 4096).expect("chunks");
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 4096));

        let cjk = "词".repeat(3000); // 9000 bytes, no spaces
        let chunks = split_chunks(&cjk, 4096).expect("cjk chunks");
        assert!(chunks.iter().all(|chunk| chunk.len() <= 4096 && chunk.chars().all(|c| c == '词')));
        assert_eq!(chunks.concat(), cjk);

        let entity = format!("{}&amp;", "w".repeat(4094));
        let chunks = split_chunks(&entity, 4096).expect("entity chunks");
        assert!(chunks.iter().all(|chunk| !chunk.ends_with('&') && !chunk.ends_with("&am")), "{chunks:?}");
        assert_eq!(chunks.concat(), entity);
    }

    fn audio_frame(payload: &[u8]) -> Vec<u8> {
        let head = "Path:audio\r\nContent-Type:audio/mpeg\r\n";
        let mut frame = (head.len() as u16).to_be_bytes().to_vec();
        frame.extend_from_slice(head.as_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn binary_frames_split_into_headers_and_payload() {
        let frame = audio_frame(b"mp3bytes");
        let (headers, payload) = parse_binary_message(&frame).expect("frame");
        assert_eq!(headers.iter().find(|(key, _)| key == "Path").map(|(_, value)| value.as_str()), Some("audio"));
        assert_eq!(payload, b"mp3bytes");
        assert!(parse_binary_message(&[0x00]).is_err());
        assert!(parse_binary_message(&[0x00, 0x40, b'x']).is_err());
    }

    #[test]
    fn text_messages_expose_their_path_and_body() {
        let message = parse_text_message("Path:audio.metadata\r\n\r\n{\"Metadata\":[]}").expect("message");
        assert_eq!(message.path, "audio.metadata");
        assert_eq!(message.payload, "{\"Metadata\":[]}");
        assert!(parse_text_message("no separator").is_err());
    }

    #[test]
    fn boundaries_convert_ticks_and_honor_compensation() {
        let body = r#"{"Metadata":[{"Type":"WordBoundary","Data":{"Offset":1500000,"Duration":2000000,"text":{"Text":"请 &amp; 求"}}}]}"#;
        let timings = parse_metadata(body, 10_000_000).expect("timings");
        assert_eq!(timings[0].start_ms, 1150);
        assert_eq!(timings[0].end_ms, 1350);
        assert_eq!(timings[0].text, "请 & 求");

        let session = r#"{"Metadata":[{"Type":"SessionEnd","Offset":1,"Duration":1}]}"#;
        assert!(parse_metadata(session, 0).is_err(), "a message without a boundary is not an alignment");
        let unknown = r#"{"Metadata":[{"Type":"PhonemeBoundary","Offset":1,"Duration":1,"Data":{}}]}"#;
        assert!(parse_metadata(unknown, 0).is_err());
    }

    struct Scripted {
        sent: Vec<String>,
        replies: VecDeque<Incoming>,
    }

    impl Scripted {
        fn new(replies: Vec<Incoming>) -> Self {
            Self { sent: Vec::new(), replies: replies.into() }
        }
    }

    impl Transport for Scripted {
        fn send(&mut self, message: &str) -> Result<(), Error> {
            self.sent.push(message.to_owned());
            Ok(())
        }

        fn recv(&mut self) -> Result<Incoming, Error> {
            self.replies.pop_front().ok_or_else(|| Error::io("script ended"))
        }
    }

    fn request(text: &str) -> SynthesisRequest {
        SynthesisRequest { text: text.to_owned(), voice: "zh-CN-XiaoxiaoNeural".to_owned(), rate_percent: 0, word_boundaries: true }
    }

    #[test]
    fn a_single_chunk_synthesizes_audio_and_timings() {
        let synthesis = synthesize_with(&request("先看请求"), |_| {
            Ok(Scripted::new(vec![
                Incoming::Binary(audio_frame(b"AAAA")),
                Incoming::Text("Path:audio.metadata\r\n\r\n{\"Metadata\":[{\"Type\":\"WordBoundary\",\"Data\":{\"Offset\":0,\"Duration\":10000000,\"text\":{\"Text\":\"先看\"}}}]}".to_owned()),
                Incoming::Text("Path:turn.end\r\n\r\n".to_owned()),
            ]))
        })
        .expect("synthesis");
        assert_eq!(synthesis.mp3, b"AAAA");
        assert_eq!(synthesis.timings.len(), 1);
        assert_eq!(synthesis.timings[0].text, "先看");
    }

    #[test]
    fn the_second_chunk_is_shifted_by_the_first_chunks_audio() {
        use std::{cell::Cell, rc::Rc};
        let long = format!("{} {}", "a".repeat(4000), "b".repeat(4000));
        let connections = Rc::new(Cell::new(0usize));
        let counted = Rc::clone(&connections);
        let synthesis = synthesize_with(&request(&long), move |_| {
            counted.set(counted.get() + 1);
            Ok(Scripted::new(vec![
                Incoming::Binary(audio_frame(&vec![b'A'; 20_000])),
                Incoming::Text(format!(
                    "Path:audio.metadata\r\n\r\n{{\"Metadata\":[{{\"Type\":\"WordBoundary\",\"Data\":{{\"Offset\":0,\"Duration\":10000000,\"text\":{{\"Text\":\"c{}\"}}}}}}]}}",
                    counted.get()
                )),
                Incoming::Text("Path:turn.end\r\n\r\n".to_owned()),
            ]))
        })
        .expect("two-chunk synthesis");
        assert_eq!(connections.get(), 2, "each chunk gets its own connection");
        assert_eq!(synthesis.timings.len(), 2);
        // 20000 audio bytes at 48 kbit/s CBR = 20000 * 8 / 48000 s = 10/3 s = 3333 ms.
        assert_eq!(synthesis.timings[0].start_ms, 0);
        assert_eq!(synthesis.timings[1].start_ms, 3333);
    }

    #[test]
    fn silence_without_audio_is_a_failure_not_an_empty_success() {
        let error = synthesize_with(&request("hello"), |_| Ok(Scripted::new(vec![Incoming::Text("Path:turn.end\r\n\r\n".to_owned())])))
            .expect_err("no audio");
        assert!(error.to_string().contains("no audio"), "{error}");
    }

    #[test]
    fn a_forbidden_open_is_retried_once_with_the_server_skew() {
        use std::{cell::Cell, rc::Rc};
        let opens = Rc::new(Cell::new(0usize));
        let counted = Rc::clone(&opens);
        let synthesis = synthesize_with(&request("hello"), move |skew| {
            let attempt = counted.get() + 1;
            counted.set(attempt);
            if attempt == 1 {
                assert_eq!(skew, 0.0);
                return Err(TransportError::Forbidden { date: Some("Fri, 20 Nov 2026 09:15:30 GMT".to_owned()) });
            }
            assert_ne!(skew, 0.0, "the retry must carry the corrected clock");
            Ok(Scripted::new(vec![
                Incoming::Binary(audio_frame(b"CCCC")),
                Incoming::Text("Path:turn.end\r\n\r\n".to_owned()),
            ]))
        })
        .expect("retry succeeds");
        assert_eq!(synthesis.mp3, b"CCCC");
        assert_eq!(opens.get(), 2);
    }

    #[test]
    fn a_second_refusal_without_correction_stops_instead_of_looping() {
        let error = synthesize_with(&request("hello"), |_| Err::<Scripted, TransportError>(TransportError::Forbidden { date: None }))
            .expect_err("403");
        assert!(error.to_string().contains("without a server date"), "{error}");
    }

    #[test]
    fn invalid_requests_are_refused_before_any_connection() {
        assert!(synthesize_with(&request("  "), |_| -> Result<Scripted, TransportError> { panic!("must not connect") }).is_err());
        let bad_voice = SynthesisRequest { voice: "bad voice".to_owned(), ..request("hi") };
        assert!(synthesize_with(&bad_voice, |_| -> Result<Scripted, TransportError> { panic!("must not connect") }).is_err());
        let bad_rate = SynthesisRequest { rate_percent: 500, ..request("hi") };
        assert!(synthesize_with(&bad_rate, |_| -> Result<Scripted, TransportError> { panic!("must not connect") }).is_err());
    }

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
}
