//! Microsoft Edge readaloud 语音合成：**非官方接口（B 级）**。
//!
//! 合成算法运行在微软服务端，本模块只实现其协议：握手、`speech.config` 与 SSML 请求、
//! 二进制音频帧、词边界元数据。传输走 `boost-net` 的 WebSocket/TLS，纯 Rust，不依赖
//! 任何 Python 或外部二进制。
//!
//! # 非官方接口声明
//!
//! 这是 Edge 浏览器的内部在线接口，没有公开 API 文档、没有版本承诺、没有鉴权密钥可
//! 静态管理：文本会发送到 `speech.platform.bing.com`，可用性与条款可能随时变化。
//! 因此按云服务准入合同的 B 级条款约束行为：
//!
//! - 不承诺幂等与对账，结果里不出现「可重试」承诺；
//! - 中断、超时与结构异常一律 [`ReadaloudOutcome::Indeterminate`]，客户端**绝不**自动重试；
//! - 不得用于计费敏感路径；
//! - 接口一旦失效，删除本模块而不是加兼容分支。
//!
//! 唯一允许「再发一次」的情形是握手阶段的确定性拒绝（`Rejected`）：合成尚未开始、
//! 没有任何副作用。典型场景是本机时钟偏移导致 `Sec-MS-GEC` 令牌落在错误的时间窗——
//! 调用方可以用 [`server_clock_skew`] 从 `Date` 头校正后自行重发，这是调用方的决定，
//! 不是本客户端的行为。
//!
//! # 协议要点
//!
//! - 令牌：v2 `Sec-MS-GEC` 对「57 字节 Edge 版权行 + `{FILETIME ticks}{受信令牌}`」
//!   取标准 SHA-256，再加按小端字对、以摘要前 8 字节为 key 的 CS64 校验，形式为
//!   `<cs64>-<sha256>` 大写十六进制；ticks 向下取整到 5 分钟窗。
//! - 音频：`audio-24khz-48kbitrate-mono-mp3`，恒定 48 kbit/s。
//! - 长文本按 4096 字节分块，每块一条独立连接；块内边界是相对时间，跨块偏移按
//!   已收集音频字节数换算（CBR 下字节数即时间），不信任服务端的累计值。
//! - 词边界以 100 纳秒 tick 计，[`sec_ms_gec`] 之外全部换算成毫秒。
//!
//! ```no_run
//! use atlas_cloud::microsoft::readaloud::{Client, ReadaloudOutcome, ReadaloudRequest};
//!
//! fn run() {
//!     let request = ReadaloudRequest {
//!         text: "先看请求怎么进来".into(),
//!         voice: "zh-CN-XiaoxiaoNeural".into(),
//!         rate_percent: 0,
//!         word_boundaries: true,
//!     };
//!     match Client::new().synthesize(&request) {
//!         Ok(ReadaloudOutcome::Completed { mp3, boundaries }) => {
//!             println!("{} 字节，{} 条边界", mp3.len(), boundaries.len());
//!         }
//!         Ok(ReadaloudOutcome::Rejected { status, date }) => {
//!             println!("握手拒绝：{status}（Date: {date:?}）");
//!         }
//!         Ok(ReadaloudOutcome::Indeterminate { reason }) => {
//!             println!("结果未知，不得自动重试：{reason}");
//!         }
//!         Err(error) => println!("本地错误：{error}"),
//!     }
//! }
//! ```

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use boost_net::{
    NetError, Url,
    ws::{WebSocket, WsConfig, WsMessage},
};

use crate::tencent::CloudError;

/// Edge 客户端呈现的受信令牌；同时是 DRM 哈希输入的一部分。
pub const TRUSTED_CLIENT_TOKEN: &str = "6A5AA1D4EAFF4E9FB37E23D68491D6F4";
/// DRM 令牌所对应的协议版本。
pub const SEC_MS_GEC_VERSION: &str = "2-143.0.3650.75";
const WSS_BASE: &str = "wss://speech.platform.bing.com/consumer/speech/synthesize/readaloud/edge/v1";
const ORIGIN: &str = "chrome-extension://jdiccldimpdaibmpdkjnbmckianbfold";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36 Edg/143.0.0.0";
const OUTPUT_FORMAT: &str = "audio-24khz-48kbitrate-mono-mp3";
/// 声明输出格式的比特率；CBR 偏移补偿的前提。
const MP3_BITRATE_BPS: u64 = 48_000;
/// 服务端 100 纳秒计时单位下每秒的 tick 数。
const TICKS_PER_SECOND: u64 = 10_000_000;
/// 单条 SSML 请求的转义后字节上限，与服务端一致。
const MAX_CHUNK_BYTES: usize = 4096;
/// Unix 历元到 Windows FILETIME 历元的秒差。
const WIN_EPOCH: f64 = 11_644_473_600.0;
/// DRM 哈希真正吃进去的 Edge 版权行。
const EDGE_COPYRIGHT: &str = "Copyright (C) Microsoft Corporation. All rights reserved.";

/// 一次合成请求。
#[derive(Clone, Debug, PartialEq)]
pub struct ReadaloudRequest {
    /// 要朗读的文本。非空；超长文本由客户端内部分块。
    pub text: String,
    /// 声音 id，例如 `zh-CN-XiaoxiaoNeural`。
    pub voice: String,
    /// 语速偏移，整数百分比，以服务的 `+N%`/`-N%` 韵律形式发出。
    pub rate_percent: i32,
    /// 要求词级边界；关闭时返回句级边界。
    pub word_boundaries: bool,
}

/// 一条词或句边界，绝对流内毫秒。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Boundary {
    /// 起始毫秒。
    pub start_ms: u64,
    /// 结束毫秒。
    pub end_ms: u64,
    /// 对齐的文本。
    pub text: String,
}

/// 一次合成的三态结果。
#[derive(Debug)]
pub enum ReadaloudOutcome {
    /// 服务完整走完每个分块。`mp3` 为空表示它认为无话可说，由调用方裁决。
    Completed {
        /// 拼接后的 CBR 48 kbit/s 单声道 MP3。
        mp3: Vec<u8>,
        /// 绝对流内时间边界。
        boundaries: Vec<Boundary>,
    },
    /// 握手被状态码拒绝：合成没有开始，没有任何副作用。
    Rejected {
        /// HTTP 状态码。
        status: u16,
        /// 拒绝响应的 `Date` 头，可用于 [`server_clock_skew`]。
        date: Option<String>,
    },
    /// 无法判定结果：连接中途断开、帧结构异常、读到一半超时。
    /// 这一层**不得**自动重试；已收到的部分不会作为成功返回。
    Indeterminate {
        /// 中性描述，不透传上游正文。
        reason: String,
    },
}

/// 一条已打开的合成会话。
pub trait Session {
    /// 发送一条文本消息。
    fn send(&mut self, message: &str) -> Result<(), NetError>;
    /// 读取下一帧。
    fn recv(&mut self) -> Result<SessionMessage, NetError>;
}

/// 会话读到的一帧。
#[derive(Debug)]
pub enum SessionMessage {
    /// 文本帧。
    Text(String),
    /// 二进制帧。
    Binary(Vec<u8>),
    /// 对端关闭。
    Closed {
        /// 关闭状态码。
        code: Option<u16>,
        /// 关闭原因。
        reason: String,
    },
}

/// 建立一条会话的接缝；测试替身经 [`Client::with_connector`] 注入。
pub trait Connector {
    /// 用 `unix_now() + skew_seconds` 时刻的令牌连接。
    fn connect(&self, skew_seconds: f64) -> Result<Box<dyn Session>, ConnectFailure>;
}

/// 连接失败的分层：确定性拒绝与传输失败不能互相折叠。
#[derive(Debug)]
pub enum ConnectFailure {
    /// 握手收到状态码拒绝。
    Rejected {
        /// HTTP 状态码。
        status: u16,
        /// 拒绝响应的 `Date` 头。
        date: Option<String>,
    },
    /// 拨号、TLS 或握手本身失败。
    Transport(NetError),
}

/// 默认连接器：`boost-net` 的 WebSocket over TLS。
pub struct WsConnector;

impl Connector for WsConnector {
    fn connect(&self, skew_seconds: f64) -> Result<Box<dyn Session>, ConnectFailure> {
        let token = sec_ms_gec(unix_now() + skew_seconds);
        let connection_id = random_hex(false).map_err(|error| ConnectFailure::Transport(NetError::Transport(error)))?;
        let url = format!(
            "{WSS_BASE}?TrustedClientToken={TRUSTED_CLIENT_TOKEN}&ConnectionId={connection_id}&Sec-MS-GEC={token}&Sec-MS-GEC-Version={SEC_MS_GEC_VERSION}"
        );
        let parsed = Url::parse(&url).map_err(|error| ConnectFailure::Transport(NetError::Unsupported(error.to_string())))?;
        let mut config = WsConfig::default();
        config.timeout = Duration::from_secs(60);
        let muid = random_hex(true).map_err(|error| ConnectFailure::Transport(NetError::Transport(error)))?;
        config.headers = vec![
            ("User-Agent".to_owned(), USER_AGENT.to_owned()),
            ("Accept-Language".to_owned(), "en-US,en;q=0.9".to_owned()),
            ("Pragma".to_owned(), "no-cache".to_owned()),
            ("Cache-Control".to_owned(), "no-cache".to_owned()),
            ("Origin".to_owned(), ORIGIN.to_owned()),
            ("Cookie".to_owned(), format!("muid={muid};")),
        ];
        match WebSocket::connect(&parsed, config) {
            Ok(socket) => Ok(Box::new(WsSession { socket })),
            Err(NetError::Rejected { status, date }) => Err(ConnectFailure::Rejected { status, date }),
            Err(error) => Err(ConnectFailure::Transport(error)),
        }
    }
}

struct WsSession {
    socket: WebSocket,
}

impl Session for WsSession {
    fn send(&mut self, message: &str) -> Result<(), NetError> {
        self.socket.send(WsMessage::Text(message.to_owned()))
    }

    fn recv(&mut self) -> Result<SessionMessage, NetError> {
        loop {
            match self.socket.recv()? {
                WsMessage::Text(text) => return Ok(SessionMessage::Text(text)),
                WsMessage::Binary(binary) => return Ok(SessionMessage::Binary(binary)),
                WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
                WsMessage::Close { code, reason } => return Ok(SessionMessage::Closed { code, reason }),
            }
        }
    }
}

/// readaloud 客户端：单次发送，不内建重试。
pub struct Client {
    clock_skew_seconds: f64,
    connector: Box<dyn Connector>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// 用默认连接器构造，时钟按本机。
    #[must_use]
    pub fn new() -> Self {
        Self { clock_skew_seconds: 0.0, connector: Box::new(WsConnector) }
    }

    /// 设定令牌使用的时钟偏移（秒）。
    ///
    /// 偏移来自 [`server_clock_skew`]：本机时钟偏差导致令牌落错时间窗时，由调用方
    /// 校正后自行再发一次。
    #[must_use]
    pub fn with_clock_skew(mut self, skew_seconds: f64) -> Self {
        self.clock_skew_seconds = skew_seconds;
        self
    }

    /// 替换连接接缝（测试替身、受控回放）。
    #[must_use]
    pub fn with_connector(mut self, connector: Box<dyn Connector>) -> Self {
        self.connector = connector;
        self
    }

    /// 合成一次请求：每个分块开一条连接、单次发送；任何中断即 `Indeterminate`，绝不重试。
    pub fn synthesize(&self, request: &ReadaloudRequest) -> Result<ReadaloudOutcome, CloudError> {
        if request.text.trim().is_empty() {
            return Err(CloudError::InvalidConfig("readaloud synthesis needs text".into()));
        }
        if request.voice.is_empty() || !request.voice.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(CloudError::InvalidConfig(format!("voice `{}` is not a valid readaloud voice id", request.voice)));
        }
        if !(-100..=100).contains(&request.rate_percent) {
            return Err(CloudError::InvalidConfig(format!("rate {}% is outside -100..=100", request.rate_percent)));
        }
        let chunks = split_chunks(&escape_text(&request.text), MAX_CHUNK_BYTES)
            .map_err(|reason| CloudError::InvalidConfig(format!("text cannot be chunked: {reason}")))?;
        if chunks.is_empty() {
            return Err(CloudError::InvalidConfig("the text produced nothing to speak".into()));
        }

        let mut mp3 = Vec::new();
        let mut boundaries = Vec::new();
        for chunk in &chunks {
            let session = match self.connector.connect(self.clock_skew_seconds) {
                Ok(session) => session,
                Err(ConnectFailure::Rejected { status, date }) => return Ok(ReadaloudOutcome::Rejected { status, date }),
                Err(ConnectFailure::Transport(error)) => return Err(crate::error::map_transport_error(error)),
            };
            match self.run_chunk(session, request, chunk, &mut mp3, &mut boundaries) {
                Ok(()) => {}
                Err(reason) => return Ok(ReadaloudOutcome::Indeterminate { reason }),
            }
        }
        Ok(ReadaloudOutcome::Completed { mp3, boundaries })
    }

    fn run_chunk(
        &self,
        mut session: Box<dyn Session>,
        request: &ReadaloudRequest,
        chunk: &str,
        mp3: &mut Vec<u8>,
        boundaries: &mut Vec<Boundary>,
    ) -> Result<(), String> {
        let mut chunk_audio: u64 = 0;
        let timestamp = js_date(unix_now() + self.clock_skew_seconds);
        session.send(&speech_config(request.word_boundaries, &timestamp)).map_err(|error| error.to_string())?;
        let request_id = random_hex(false).map_err(|error| error.to_string())?;
        session
            .send(&ssml_request(&request_id, &timestamp, &build_ssml(&request.voice, request.rate_percent, chunk)))
            .map_err(|error| error.to_string())?;
        loop {
            match session.recv().map_err(|error| error.to_string())? {
                SessionMessage::Binary(bytes) => {
                    let (headers, payload) = parse_binary_message(&bytes)?;
                    let path = headers.iter().find(|(key, _)| key == "Path").map(|(_, value)| value.as_str());
                    if path != Some("audio") {
                        return Err("readaloud sent a binary frame that is not audio".into());
                    }
                    match headers.iter().find(|(key, _)| key == "Content-Type").map(|(_, value)| value.as_str()) {
                        None if payload.is_empty() => continue,
                        None => return Err("readaloud audio frame without a content type still carried data".into()),
                        Some("audio/mpeg") if payload.is_empty() => return Err("readaloud audio frame carries no audio".into()),
                        Some("audio/mpeg") => {
                            chunk_audio += payload.len() as u64;
                            mp3.extend_from_slice(&payload);
                        }
                        Some(other) => return Err(format!("readaloud sent audio as `{other}`")),
                    }
                }
                SessionMessage::Text(message) => {
                    let parsed = parse_text_message(&message)?;
                    match parsed.path.as_str() {
                        "audio.metadata" => {
                            let previous_bytes = mp3.len() as u64 - chunk_audio;
                            let compensation = previous_bytes * 8 * TICKS_PER_SECOND / MP3_BITRATE_BPS;
                            boundaries.extend(parse_boundaries(&parsed.payload, compensation)?);
                        }
                        "turn.end" => return Ok(()),
                        "turn.start" | "response" => {}
                        other => return Err(format!("unknown readaloud path `{other}`")),
                    }
                }
                SessionMessage::Closed { code, reason } => return Err(format!("readaloud socket closed with {code:?}: {reason}")),
            }
        }
    }
}

/// 当前 Unix 秒（小数）。
fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs_f64()).unwrap_or_default()
}

/// 用系统安全随机源生成 32 位十六进制串。
fn random_hex(uppercase: bool) -> Result<String, String> {
    let mut bytes = [0u8; 16];
    boost_random::Secure::system().fill(&mut bytes).map_err(|error| error.to_string())?;
    let hex = hex_upper(&bytes);
    Ok(if uppercase { hex } else { hex.to_lowercase() })
}

/// 大写十六进制。
fn hex_upper(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

/// 某 Unix 时刻的 `Sec-MS-GEC` v2 令牌。
///
/// 哈希输入是 57 字节 Edge 版权行 + `{FILETIME ticks}{TRUSTED_CLIENT_TOKEN}` 的 ASCII
/// 文本，ticks 为向下取整到 5 分钟窗的百纳秒计数。纯函数，不读时钟。
#[must_use]
pub fn sec_ms_gec(unix_seconds: f64) -> String {
    let mut ticks = unix_seconds + WIN_EPOCH;
    ticks -= ticks % 300.0;
    ticks *= 1e7;
    let message = format!("{EDGE_COPYRIGHT}{ticks:.0}{TRUSTED_CLIENT_TOKEN}");
    let bytes = message.as_bytes();
    let digest = boost_crypto::hash::sha256(bytes);
    let cs64 = cs64_digest(bytes, &digest);
    format!("{}-{}", hex_upper(&cs64), hex_upper(&digest))
}

fn rol16(value: u32) -> u32 {
    value.rotate_left(16)
}

/// CS64 的字对乘数：(word, sum, product)，与 Edge 的 JavaScript 相同。
const SWAP_FIRST: (u32, u32, u32) = (0xEF05_69FB, 0x76F5_0F34, 0xC3EF_EA97);
const SWAP_SECOND: (u32, u32, u32) = (0xC317_13DB, 0x1881_CE3C, 0x35BD_1EC9);
const MIX_FIRST: [u32; 4] = [0xCF98_B111, 0x8708_5B9F, 0x12CE_B96D, 0x257E_1D83];
const MIX_SECOND: [u32; 4] = [0xA274_16F5, 0xD383_96FF, 0x7C93_2B89, 0xBFA4_9F69];

fn swap_term(word: u32, key: u32, (swapped_word, swapped_sum, swapped_product): (u32, u32, u32)) -> u32 {
    let sum = word.wrapping_mul(key).wrapping_add(rol16(word).wrapping_mul(swapped_word));
    let product = rol16(sum).wrapping_mul(swapped_sum);
    rol16(product).wrapping_mul(swapped_product).wrapping_add(product)
}

fn swap_and_multiply(mut value: u32, multipliers: &[u32; 4]) -> u32 {
    for multiplier in multipliers {
        value = rol16(value).wrapping_mul(*multiplier);
    }
    value
}

/// 令牌 CS64 半部：把整条消息按小端 32 位字对喂进以摘要前 8 字节为 key 的通用哈希。
fn cs64_digest(message: &[u8], digest: &[u8; 32]) -> [u8; 8] {
    let key_a = u32::from_le_bytes(digest[0..4].try_into().expect("four-byte key")) | 1;
    let key_b = u32::from_le_bytes(digest[4..8].try_into().expect("four-byte key")) | 1;
    let mut padded = message.to_vec();
    while padded.len() % 8 != 0 {
        padded.push(0);
    }
    let mut swap_sum = 0u32;
    let mut last_swap = 0u32;
    let mut mix = 0u32;
    let mut mix_sum = 0u32;
    for pair in padded.chunks_exact(8) {
        let first = u32::from_le_bytes(pair[0..4].try_into().expect("four-byte word"));
        let second = u32::from_le_bytes(pair[4..8].try_into().expect("four-byte word"));
        let first_swap = swap_term(first, key_a, SWAP_FIRST);
        last_swap = swap_term(second, key_b, SWAP_SECOND);
        swap_sum = swap_sum.wrapping_add(first_swap).wrapping_add(last_swap);
        mix = swap_and_multiply(mix.wrapping_add(first).wrapping_mul(key_a), &MIX_FIRST);
        mix_sum = mix_sum.wrapping_add(mix);
        mix = swap_and_multiply(mix.wrapping_add(second).wrapping_mul(key_b), &MIX_SECOND);
        mix_sum = mix_sum.wrapping_add(mix);
    }
    let mut out = [0u8; 8];
    out[0..4].copy_from_slice(&(last_swap ^ mix).to_le_bytes());
    out[4..8].copy_from_slice(&(swap_sum ^ mix_sum).to_le_bytes());
    out
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// 历元日数到公历（Howard Hinnant 的 `civil_from_days`）。
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

/// [`civil_from_days`] 的逆。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 服务的 `X-Timestamp`：JavaScript 风格的 UTC 串。
#[must_use]
pub fn js_date(unix_seconds: f64) -> String {
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

/// 把拒绝响应的 `Date` 头解析成 Unix 秒。
pub fn parse_http_date(value: &str) -> Option<f64> {
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

/// 服务器时间减本机时间：握手被拒后调用方据此校正 [`Client::with_clock_skew`] 再自行重发。
#[must_use]
pub fn server_clock_skew(date_header: &str, local_unix_seconds: f64) -> Option<f64> {
    Some(parse_http_date(date_header)? - local_unix_seconds)
}

/// 去掉服务不接受的控制码位并转义 XML 标记。
#[must_use]
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

/// 还原服务在边界文本上包的 XML 实体。
fn unescape_xml(text: &str) -> String {
    text.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// 整数百分比到服务韵律语速形式。
fn rate_string(percent: i32) -> String {
    if percent < 0 { format!("{percent}%") } else { format!("+{percent}%") }
}

/// 为一块已转义文本构造 SSML 文档。
#[must_use]
pub fn build_ssml(voice: &str, rate_percent: i32, escaped_text: &str) -> String {
    format!(
        "<speak version='1.0' xmlns='http://www.w3.org/2001/10/synthesis' xml:lang='en-US'>\
         <voice name='{voice}'><prosody pitch='+0Hz' rate='{}' volume='+0%'>{escaped_text}</prosody></voice>\
         </speak>",
        rate_string(rate_percent)
    )
}

/// 选择输出格式与边界粒度的 `speech.config` 请求。
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

/// 把 SSML 包进服务要求的请求头。
#[must_use]
pub fn ssml_request(request_id: &str, timestamp: &str, ssml: &str) -> String {
    format!("X-RequestId:{request_id}\r\nContent-Type:application/ssml+xml\r\nX-Timestamp:{timestamp}Z\r\nPath:ssml\r\n\r\n{ssml}")
}

/// 把已转义文本切成不超过 `byte_length` 的分块：优先换行与空格，绝不切断
/// UTF-8 序列或 XML 实体。
pub fn split_chunks(escaped: &str, byte_length: usize) -> Result<Vec<String>, String> {
    if byte_length == 0 {
        return Err("chunk byte length must be positive".into());
    }
    let mut text = escaped.as_bytes();
    let mut chunks = Vec::new();
    while text.len() > byte_length {
        let boundary = last_byte_within(text, b'\n', byte_length)
            .or_else(|| last_byte_within(text, b' ', byte_length))
            .unwrap_or_else(|| utf8_safe_split(text, byte_length));
        let split_at = adjust_for_entity(text, boundary);
        if split_at == 0 {
            return Err("text cannot be split within the service byte limit".into());
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

/// 不超过 `limit` 且不切断 UTF-8 序列的最大下标。
fn utf8_safe_split(text: &[u8], limit: usize) -> usize {
    let mut end = limit.min(text.len());
    while end > 0 && matches!(text.get(end), Some(&byte) if (byte & 0xC0) == 0x80) {
        end -= 1;
    }
    end
}

/// 把切点移到未闭合 `&` 之前，实体保持完整。
fn adjust_for_entity(text: &[u8], mut split_at: usize) -> usize {
    while split_at > 0 {
        let Some(ampersand) = text[..split_at].iter().rposition(|byte| *byte == b'&')
        else {
            break;
        };
        if text[ampersand..split_at].contains(&b';') {
            break;
        }
        split_at = ampersand;
    }
    split_at
}

/// 服务文本消息的头部与正文。
#[derive(Debug)]
pub struct ServiceMessage {
    /// `Path:` 头值。
    pub path: String,
    /// 头部分隔符之后、已去掉前导空白的正文。
    pub payload: String,
}

/// 解析文本帧的路径与正文。
pub fn parse_text_message(message: &str) -> Result<ServiceMessage, String> {
    let (head, payload) = message.split_once("\r\n\r\n").ok_or("readaloud text message has no header separator")?;
    let path = header_value(head, "Path").ok_or("readaloud text message has no Path header")?;
    Ok(ServiceMessage { path: path.to_owned(), payload: payload.trim_start().to_owned() })
}

/// 解析二进制帧的头与载荷。
pub fn parse_binary_message(bytes: &[u8]) -> Result<(Vec<(String, String)>, Vec<u8>), String> {
    if bytes.len() < 2 {
        return Err("readaloud binary frame is missing its header length".into());
    }
    let header_length = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
    if bytes.len() < 2 + header_length {
        return Err("readaloud binary header exceeds the frame".into());
    }
    let head = std::str::from_utf8(&bytes[2..2 + header_length]).map_err(|_| "readaloud binary header is not UTF-8")?;
    let headers = head
        .split("\r\n")
        .filter_map(|line| line.split_once(':').map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned())))
        .collect();
    Ok((headers, bytes[2 + header_length..].to_vec()))
}

fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.split("\r\n").find_map(|line| line.split_once(':').filter(|(key, _)| *key == name).map(|(_, value)| value.trim()))
}

/// 把一条 `audio.metadata` 正文转成绝对时间边界。
///
/// `compensation_ticks` 把块内偏移平移到拼接流：前面各块已收集音频字节对应的 tick 数。
pub fn parse_boundaries(body: &str, compensation_ticks: u64) -> Result<Vec<Boundary>, String> {
    let envelope: serde_json::Value = serde_json::from_str(body).map_err(|error| format!("readaloud metadata is not valid JSON: {error}"))?;
    let items = envelope.get("Metadata").and_then(serde_json::Value::as_array).ok_or("readaloud metadata has no Metadata array")?;
    let mut boundaries = Vec::new();
    for item in items {
        let kind = item.get("Type").and_then(serde_json::Value::as_str).ok_or("readaloud metadata item has no Type")?;
        match kind {
            "WordBoundary" | "SentenceBoundary" => {
                let data = item.get("Data").ok_or("readaloud boundary carries no data")?;
                let offset = data.get("Offset").and_then(serde_json::Value::as_u64).ok_or("readaloud boundary has no Offset")?;
                let duration = data.get("Duration").and_then(serde_json::Value::as_u64).ok_or("readaloud boundary has no Duration")?;
                let text = data.pointer("/text/Text").and_then(serde_json::Value::as_str).ok_or("readaloud boundary has no text")?;
                let start_ms = (offset + compensation_ticks) / (TICKS_PER_SECOND / 1000);
                let end_ms = (offset + duration + compensation_ticks) / (TICKS_PER_SECOND / 1000);
                boundaries.push(Boundary { start_ms, end_ms: end_ms.max(start_ms + 1), text: unescape_xml(text) });
            }
            "SessionEnd" => {}
            other => return Err(format!("unknown readaloud metadata type `{other}`")),
        }
    }
    if boundaries.is_empty() {
        return Err("readaloud metadata carried no boundary".into());
    }
    Ok(boundaries)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    struct Scripted {
        sent: Vec<String>,
        replies: VecDeque<SessionMessage>,
    }

    impl Scripted {
        fn new(replies: Vec<SessionMessage>) -> Self {
            Self { sent: Vec::new(), replies: replies.into() }
        }
    }

    impl Session for Scripted {
        fn send(&mut self, message: &str) -> Result<(), NetError> {
            self.sent.push(message.to_owned());
            Ok(())
        }

        fn recv(&mut self) -> Result<SessionMessage, NetError> {
            self.replies.pop_front().ok_or_else(|| NetError::Closed)
        }
    }

    #[derive(Clone)]
    struct ScriptedConnector {
        scripts: std::rc::Rc<std::cell::RefCell<VecDeque<Vec<SessionMessage>>>>,
        rejections: std::rc::Rc<std::cell::RefCell<VecDeque<(u16, Option<String>)>>>,
        connects: std::rc::Rc<std::cell::Cell<usize>>,
        skews: std::rc::Rc<std::cell::RefCell<Vec<f64>>>,
    }

    impl ScriptedConnector {
        fn new(scripts: Vec<Vec<SessionMessage>>) -> Self {
            Self {
                scripts: std::rc::Rc::new(std::cell::RefCell::new(scripts.into())),
                rejections: std::rc::Rc::new(std::cell::RefCell::new(VecDeque::new())),
                connects: std::rc::Rc::new(std::cell::Cell::new(0)),
                skews: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            }
        }

        fn rejecting(rejections: Vec<(u16, Option<String>)>, scripts: Vec<Vec<SessionMessage>>) -> Self {
            Self {
                scripts: std::rc::Rc::new(std::cell::RefCell::new(scripts.into())),
                rejections: std::rc::Rc::new(std::cell::RefCell::new(rejections.into())),
                connects: std::rc::Rc::new(std::cell::Cell::new(0)),
                skews: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            }
        }
    }

    impl Connector for ScriptedConnector {
        fn connect(&self, skew_seconds: f64) -> Result<Box<dyn Session>, ConnectFailure> {
            self.connects.set(self.connects.get() + 1);
            self.skews.borrow_mut().push(skew_seconds);
            if let Some((status, date)) = self.rejections.borrow_mut().pop_front() {
                return Err(ConnectFailure::Rejected { status, date });
            }
            let script = self.scripts.borrow_mut().pop_front().ok_or_else(|| ConnectFailure::Transport(NetError::Closed))?;
            Ok(Box::new(Scripted::new(script)))
        }
    }

    fn request(text: &str) -> ReadaloudRequest {
        ReadaloudRequest { text: text.to_owned(), voice: "zh-CN-XiaoxiaoNeural".to_owned(), rate_percent: 0, word_boundaries: true }
    }

    fn audio_frame(payload: &[u8]) -> Vec<u8> {
        let head = "Path:audio\r\nContent-Type:audio/mpeg\r\n";
        let mut frame = (head.len() as u16).to_be_bytes().to_vec();
        frame.extend_from_slice(head.as_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    fn metadata_frame(text: &str) -> SessionMessage {
        SessionMessage::Text(format!(
            "Path:audio.metadata\r\n\r\n{{\"Metadata\":[{{\"Type\":\"WordBoundary\",\"Data\":{{\"Offset\":0,\"Duration\":10000000,\"text\":{{\"Text\":\"{text}\"}}}}}}]}}"
        ))
    }

    #[test]
    fn the_token_matches_an_independent_reference_implementation() {
        // The same timestamp hashed by a verbatim port of MsEdgeTTS's
        // production TypeScript (`SecMsGec.ts`), outside this crate.
        assert_eq!(sec_ms_gec(1_750_000_000.0), "B029A7426816F097-4579D112DC47FC3BF432B499C0A282F427C659474AF9BF5FA33CE2C003F9E9C2");
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
        assert_eq!(server_clock_skew("Fri, 20 Nov 2026 09:15:30 GMT", 1_795_166_100.0), Some(30.0));
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

        let cjk = "词".repeat(3000);
        let chunks = split_chunks(&cjk, 4096).expect("cjk chunks");
        assert!(chunks.iter().all(|chunk| chunk.len() <= 4096 && chunk.chars().all(|c| c == '词')));
        assert_eq!(chunks.concat(), cjk);

        let entity = format!("{}&amp;", "w".repeat(4094));
        let chunks = split_chunks(&entity, 4096).expect("entity chunks");
        assert!(chunks.iter().all(|chunk| !chunk.ends_with('&') && !chunk.ends_with("&am")), "{chunks:?}");
        assert_eq!(chunks.concat(), entity);
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
        let boundaries = parse_boundaries(body, 10_000_000).expect("boundaries");
        assert_eq!(boundaries[0].start_ms, 1150);
        assert_eq!(boundaries[0].end_ms, 1350);
        assert_eq!(boundaries[0].text, "请 & 求");

        let session = r#"{"Metadata":[{"Type":"SessionEnd","Offset":1,"Duration":1}]}"#;
        assert!(parse_boundaries(session, 0).is_err(), "a message without a boundary is not an alignment");
        let unknown = r#"{"Metadata":[{"Type":"PhonemeBoundary","Offset":1,"Duration":1,"Data":{}}]}"#;
        assert!(parse_boundaries(unknown, 0).is_err());
    }

    #[test]
    fn a_single_chunk_synthesizes_audio_and_boundaries() {
        let outcome = Client::new()
            .with_connector(Box::new(ScriptedConnector::new(vec![vec![
                SessionMessage::Binary(audio_frame(b"AAAA")),
                metadata_frame("先看"),
                SessionMessage::Text("Path:turn.end\r\n\r\n".to_owned()),
            ]])))
            .synthesize(&request("先看请求"))
            .expect("no local error");
        match outcome {
            ReadaloudOutcome::Completed { mp3, boundaries } => {
                assert_eq!(mp3, b"AAAA");
                assert_eq!(boundaries.len(), 1);
                assert_eq!(boundaries[0].text, "先看");
            }
            other => panic!("expected completion, got {other:?}"),
        }
    }

    #[test]
    fn the_second_chunk_is_shifted_by_the_first_chunks_audio() {
        let long = format!("{} {}", "a".repeat(4000), "b".repeat(4000));
        let connector = ScriptedConnector::new(vec![
            vec![
                SessionMessage::Binary(audio_frame(&vec![b'A'; 20_000])),
                metadata_frame("c1"),
                SessionMessage::Text("Path:turn.end\r\n\r\n".to_owned()),
            ],
            vec![
                SessionMessage::Binary(audio_frame(&vec![b'A'; 20_000])),
                metadata_frame("c2"),
                SessionMessage::Text("Path:turn.end\r\n\r\n".to_owned()),
            ],
        ]);
        let connects = connector.connects.clone();
        let outcome = Client::new().with_connector(Box::new(connector)).synthesize(&request(&long)).expect("two-chunk synthesis");
        match outcome {
            ReadaloudOutcome::Completed { boundaries, .. } => {
                assert_eq!(connects.get(), 2, "each chunk gets its own connection");
                assert_eq!(boundaries.len(), 2);
                // 20000 audio bytes at 48 kbit/s CBR = 20000 * 8 / 48000 s = 10/3 s = 3333 ms.
                assert_eq!(boundaries[0].start_ms, 0);
                assert_eq!(boundaries[1].start_ms, 3333);
            }
            other => panic!("expected completion, got {other:?}"),
        }
    }

    #[test]
    fn an_interrupted_stream_is_indeterminate_and_is_never_retried() {
        let connector = ScriptedConnector::new(vec![vec![SessionMessage::Binary(audio_frame(b"AAAA"))]]);
        let connects = connector.connects.clone();
        let outcome = Client::new().with_connector(Box::new(connector)).synthesize(&request("hello")).expect("no local error");
        match outcome {
            ReadaloudOutcome::Indeterminate { reason } => assert!(reason.contains("closed"), "{reason}"),
            other => panic!("expected indeterminate, got {other:?}"),
        }
        assert_eq!(connects.get(), 1, "an indeterminate result must not trigger another connection");
    }

    #[test]
    fn a_handshake_rejection_is_a_result_not_an_error() {
        let outcome = Client::new()
            .with_connector(Box::new(ScriptedConnector::rejecting(vec![(403, Some("Fri, 20 Nov 2026 09:15:30 GMT".to_owned()))], vec![])))
            .synthesize(&request("hello"))
            .expect("no local error");
        match outcome {
            ReadaloudOutcome::Rejected { status, date } => {
                assert_eq!(status, 403);
                assert_eq!(date.as_deref(), Some("Fri, 20 Nov 2026 09:15:30 GMT"));
            }
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn the_clock_skew_is_an_explicit_caller_input() {
        let connector = ScriptedConnector::new(vec![vec![
            SessionMessage::Binary(audio_frame(b"CCCC")),
            SessionMessage::Text("Path:turn.end\r\n\r\n".to_owned()),
        ]]);
        let skews = connector.skews.clone();
        let outcome =
            Client::new().with_clock_skew(45.0).with_connector(Box::new(connector)).synthesize(&request("hello")).expect("no local error");
        assert!(matches!(outcome, ReadaloudOutcome::Completed { .. }));
        assert_eq!(skews.borrow().as_slice(), &[45.0], "the client stamps the caller's skew into every token");
    }

    #[test]
    fn invalid_requests_fail_locally_before_any_connection() {
        let connector = ScriptedConnector::new(vec![]);
        let connects = connector.connects.clone();
        let client = Client::new().with_connector(Box::new(connector));
        assert!(client.synthesize(&request("  ")).is_err());
        let bad_voice = ReadaloudRequest { voice: "bad voice".to_owned(), ..request("hi") };
        assert!(client.synthesize(&bad_voice).is_err());
        let bad_rate = ReadaloudRequest { rate_percent: 500, ..request("hi") };
        assert!(client.synthesize(&bad_rate).is_err());
        assert_eq!(connects.get(), 0, "validation happens before the first connect");
    }
}
