//! Synchronous WebSocket client over TCP with optional TLS versions.

use std::{
    fmt::{Debug, Formatter},
    time::{Duration, Instant},
};

use url::Url;

use crate::{
    NetError,
    connection::{self, Connection, TimedSocket},
    tls::TlsConfig,
};

mod frame;
mod handshake;

/// One WebSocket message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsMessage {
    /// A UTF-8 text message.
    Text(String),
    /// A binary message.
    Binary(Vec<u8>),
    /// A ping the peer sent; a pong with the same payload was already written back.
    Ping(Vec<u8>),
    /// A pong the peer sent.
    Pong(Vec<u8>),
    /// A close frame from the peer.
    Close {
        /// Close status code, when the frame carried one.
        code: Option<u16>,
        /// UTF-8 close reason.
        reason: String,
    },
}

/// Configuration for one WebSocket connection.
#[derive(Debug, Clone)]
pub struct WsConfig {
    /// Maximum assembled message size.
    pub max_message_bytes: usize,
    /// Per-operation read and write timeout.
    pub timeout: Duration,
    /// TLS protocol versions offered for `wss` URLs.
    pub tls: TlsConfig,
    /// Extra request headers for the opening handshake, e.g. a user agent or a
    /// cookie a service requires. Names the handshake owns are ignored.
    pub headers: Vec<(String, String)>,
}

impl Default for WsConfig {
    fn default() -> Self {
        Self { max_message_bytes: 8 * 1024 * 1024, timeout: Duration::from_secs(30), tls: TlsConfig::default(), headers: Vec::new() }
    }
}

/// A connected WebSocket session.
pub struct WebSocket {
    stream: Connection<TimedSocket>,
    max_message_bytes: usize,
    fragmented: Option<(u8, Vec<u8>)>,
    closed: bool,
}

impl Debug for WebSocket {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("WebSocket").field("closed", &self.closed).finish_non_exhaustive()
    }
}

impl WebSocket {
    /// Open a `ws://` or `wss://` connection and perform the opening handshake.
    pub fn connect(url: &Url, config: WsConfig) -> Result<Self, NetError> {
        validate_url(url)?;
        let host = url.host_str().ok_or_else(|| NetError::Unsupported("URL has no host".into()))?;
        let network_host = host.trim_start_matches('[').trim_end_matches(']');
        let port = url.port_or_known_default().ok_or_else(|| NetError::Unsupported("URL has no port".into()))?;
        let deadline = Instant::now().checked_add(config.timeout).ok_or_else(|| NetError::Unsupported("timeout is too large".into()))?;
        let tcp = connection::dial(network_host, port, deadline)?;
        let socket = TimedSocket::new(tcp, config.timeout);
        let connection = connection::secure(url.scheme(), network_host, socket, &config.tls)?;
        let mut stream = connection;
        handshake::perform(&mut stream, url, &config.headers)?;
        Ok(Self { stream, max_message_bytes: config.max_message_bytes, fragmented: None, closed: false })
    }

    /// Send one message.
    pub fn send(&mut self, message: WsMessage) -> Result<(), NetError> {
        if self.closed {
            return Err(NetError::Closed);
        }
        match message {
            WsMessage::Text(text) => self.send_frame(frame::OP_TEXT, text.as_bytes()),
            WsMessage::Binary(bytes) => self.send_frame(frame::OP_BINARY, &bytes),
            WsMessage::Ping(payload) => {
                ensure_control_payload(&payload)?;
                self.send_frame(frame::OP_PING, &payload)
            }
            WsMessage::Pong(payload) => {
                ensure_control_payload(&payload)?;
                self.send_frame(frame::OP_PONG, &payload)
            }
            WsMessage::Close { code, reason } => {
                let payload = close_payload(code, &reason)?;
                self.send_frame(frame::OP_CLOSE, &payload)?;
                self.closed = true;
                Ok(())
            }
        }
    }

    /// Send a close frame with a status code and reason.
    pub fn close(&mut self, code: u16, reason: &str) -> Result<(), NetError> {
        self.send(WsMessage::Close { code: Some(code), reason: reason.to_string() })
    }

    /// Read the next message.
    ///
    /// Pings are answered with a pong before the ping is returned. A close frame marks the
    /// session closed; later reads fail with [`NetError::Closed`].
    pub fn recv(&mut self) -> Result<WsMessage, NetError> {
        if self.closed {
            return Err(NetError::Closed);
        }
        let max_message_bytes = self.max_message_bytes;
        loop {
            let current = frame::read(&mut self.stream, max_message_bytes)?;
            match current.opcode {
                frame::OP_PING => {
                    self.send_frame(frame::OP_PONG, &current.payload)?;
                    return Ok(WsMessage::Ping(current.payload));
                }
                frame::OP_PONG => return Ok(WsMessage::Pong(current.payload)),
                frame::OP_CLOSE => {
                    self.closed = true;
                    let (code, reason) = decode_close(&current.payload)?;
                    return Ok(WsMessage::Close { code, reason });
                }
                frame::OP_CONTINUATION => {
                    let Some((_opcode, buffer)) = self.fragmented.as_mut()
                    else {
                        return Err(NetError::Protocol("continuation frame without an open message".into()));
                    };
                    if buffer.len().saturating_add(current.payload.len()) > max_message_bytes {
                        return Err(NetError::BodyTooLarge { limit: max_message_bytes });
                    }
                    buffer.extend_from_slice(&current.payload);
                    if current.fin {
                        let (opcode, payload) = self.fragmented.take().expect("fragmented message is present");
                        return decode_data(opcode, payload);
                    }
                }
                frame::OP_TEXT | frame::OP_BINARY => {
                    if self.fragmented.is_some() {
                        return Err(NetError::Protocol("new data frame while a fragmented message is open".into()));
                    }
                    if current.fin {
                        return decode_data(current.opcode, current.payload);
                    }
                    if current.payload.len() > max_message_bytes {
                        return Err(NetError::BodyTooLarge { limit: max_message_bytes });
                    }
                    self.fragmented = Some((current.opcode, current.payload));
                }
                other => return Err(NetError::Protocol(format!("unsupported frame opcode {other:#x}"))),
            }
        }
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), NetError> {
        let mut mask = [0u8; 4];
        boost_random::Secure::system()
            .fill(&mut mask)
            .map_err(|error| NetError::Transport(format!("cannot generate a frame mask: {error}")))?;
        frame::write(&mut self.stream, opcode, true, payload, mask)
    }
}

fn decode_data(opcode: u8, payload: Vec<u8>) -> Result<WsMessage, NetError> {
    if opcode == frame::OP_TEXT {
        let text = String::from_utf8(payload).map_err(|_| NetError::Protocol("text message is not valid UTF-8".into()))?;
        Ok(WsMessage::Text(text))
    }
    else {
        Ok(WsMessage::Binary(payload))
    }
}

fn decode_close(payload: &[u8]) -> Result<(Option<u16>, String), NetError> {
    match payload.len() {
        0 => Ok((None, String::new())),
        1 => Err(NetError::Protocol("close frame payload must be empty or at least two bytes".into())),
        _ => {
            let code = u16::from_be_bytes([payload[0], payload[1]]);
            let reason = String::from_utf8(payload[2..].to_vec()).map_err(|_| NetError::Protocol("close reason is not valid UTF-8".into()))?;
            Ok((Some(code), reason))
        }
    }
}

fn close_payload(code: Option<u16>, reason: &str) -> Result<Vec<u8>, NetError> {
    let Some(code) = code
    else {
        if !reason.is_empty() {
            return Err(NetError::Unsupported("a close reason requires a status code".into()));
        }
        return Ok(Vec::new());
    };
    let mut payload = Vec::with_capacity(2 + reason.len());
    payload.extend_from_slice(&code.to_be_bytes());
    payload.extend_from_slice(reason.as_bytes());
    ensure_control_payload(&payload)?;
    Ok(payload)
}

fn ensure_control_payload(payload: &[u8]) -> Result<(), NetError> {
    if payload.len() > 125 {
        return Err(NetError::Unsupported("control frame payload exceeds 125 bytes".into()));
    }
    Ok(())
}

fn validate_url(url: &Url) -> Result<(), NetError> {
    if url.host_str().is_none() || url.username() != "" || url.password().is_some() {
        return Err(NetError::Unsupported("URL must have a host and no credentials".into()));
    }
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err(NetError::Unsupported(format!("unsupported WebSocket scheme `{}`", url.scheme())));
    }
    #[cfg(not(feature = "tls"))]
    if url.scheme() == "wss" {
        return Err(NetError::Unsupported("wss requires the `tls` feature".into()));
    }
    Ok(())
}
