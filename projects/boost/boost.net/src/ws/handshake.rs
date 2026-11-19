//! WebSocket opening handshake (RFC 6455 §4).

use std::io::{Read, Write};

use boost_crypto::hash::sha1;
use data_encoding::representation::Base64;
use url::Url;

use crate::NetError;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// Send the opening handshake and validate the `101 Switching Protocols` response.
///
/// `extra` carries caller-supplied request headers (user agent, cookies, service
/// identity). Names the handshake itself owns — `Host`, `Upgrade`, `Connection`
/// and the `Sec-WebSocket-*` pair — are dropped from `extra`: the client's own
/// values are the only ones that keep the session consistent.
///
/// Reads stay byte-exact, so bytes the server sends after the response head remain unread for
/// the frame reader.
pub(crate) fn perform<R: Read + Write>(stream: &mut R, url: &Url, extra: &[(String, String)]) -> Result<(), NetError> {
    let mut nonce = [0u8; 16];
    boost_random::Secure::system()
        .fill(&mut nonce)
        .map_err(|error| NetError::Transport(format!("cannot generate a WebSocket key: {error}")))?;
    let key = Base64::standard_strict().encode_text(nonce);
    let host = url.host_str().ok_or_else(|| NetError::Unsupported("URL has no host".into()))?;
    let host_header = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    let resource = match url.query() {
        Some(query) => format!("{}?{query}", if url.path().is_empty() { "/" } else { url.path() }),
        None if url.path().is_empty() => "/".to_string(),
        None => url.path().to_string(),
    };
    let mut request = format!(
        "GET {resource} HTTP/1.1\r\n\
         Host: {host_header}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n"
    );
    for (name, value) in extra {
        if is_reserved(name) {
            continue;
        }
        if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
            return Err(NetError::Unsupported("handshake header names and values must not contain line breaks".into()));
        }
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).map_err(NetError::Body)?;
    stream.flush().map_err(NetError::Body)?;

    let head = read_head(stream)?;
    if head.status != 101 {
        return Err(NetError::Rejected { status: head.status, date: head.header("date").map(str::to_owned) });
    }
    if !head.contains_token("upgrade", "websocket") {
        return Err(NetError::Protocol("handshake response is missing `Upgrade: websocket`".into()));
    }
    if !head.contains_token("connection", "upgrade") {
        return Err(NetError::Protocol("handshake response is missing `Connection: Upgrade`".into()));
    }
    let expected = Base64::standard_strict().encode_text(sha1(format!("{key}{GUID}").as_bytes()));
    let actual =
        head.header("sec-websocket-accept").ok_or_else(|| NetError::Protocol("handshake response is missing Sec-WebSocket-Accept".into()))?;
    if !actual.eq(&expected) {
        return Err(NetError::Protocol("handshake response Sec-WebSocket-Accept does not match the request key".into()));
    }
    Ok(())
}

struct Head {
    status: u16,
    entries: Vec<(String, String)>,
}

fn is_reserved(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "host" | "upgrade" | "connection" | "sec-websocket-key" | "sec-websocket-version")
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.entries.iter().find(|(entry, _)| entry == name).map(|(_, value)| value.as_str())
    }

    fn contains_token(&self, name: &str, token: &str) -> bool {
        self.header(name).is_some_and(|value| value.split(',').any(|part| part.trim().eq_ignore_ascii_case(token)))
    }
}

fn read_head<R: Read>(stream: &mut R) -> Result<Head, NetError> {
    let mut budget = MAX_HEADER_BYTES;
    let mut line = String::new();
    read_line(stream, &mut line, &mut budget)?;
    let mut parts = line.split_whitespace();
    let version = parts.next().unwrap_or("");
    if !matches!(version, "HTTP/1.1" | "HTTP/1.0") {
        return Err(NetError::Protocol("invalid handshake status line".into()));
    }
    let status = parts.next().unwrap_or("").parse::<u16>().map_err(|_| NetError::Protocol("invalid handshake status code".into()))?;
    let mut entries = Vec::new();
    loop {
        line.clear();
        read_line(stream, &mut line, &mut budget)?;
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or_else(|| NetError::Protocol("invalid handshake header".into()))?;
        entries.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    Ok(Head { status, entries })
}

fn read_line<R: Read>(stream: &mut R, line: &mut String, budget: &mut usize) -> Result<(), NetError> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if bytes.len() >= *budget {
            return Err(NetError::HeaderTooLarge { limit: MAX_HEADER_BYTES });
        }
        stream.read_exact(&mut byte).map_err(NetError::Body)?;
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n") {
            break;
        }
    }
    *budget -= bytes.len();
    *line = String::from_utf8(bytes[..bytes.len() - 2].to_vec())
        .map_err(|error| NetError::Protocol(format!("handshake head is not UTF-8: {error}")))?;
    Ok(())
}
