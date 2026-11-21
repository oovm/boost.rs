//! Product-owned TLS 1.3 client.
//!
//! Implements the RFC 8446 handshake for a single profile: X25519 key share,
//! `TLS_AES_128_GCM_SHA256`, SHA-256 transcripts, ALPN `http/1.1`, and server
//! authentication through [`crate::pki`]. HelloRetryRequest with the cookie
//! extension is supported; session resumption, early data, and client
//! authentication are not. The primitives come from `boost_crypto` and the DER
//! codec from `data_binary_asn1`; this module depends on neither `rustls` nor an
//! OS certificate store.

use std::{
    io::{self, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

use boost_crypto::{
    ecdsa::{p256, p384},
    ed25519,
    hash::sha256,
    mac::hmac_sha256,
    rsa,
};
use boost_random::Secure;
use data_binary_asn1::{DateTime, Writer, oids};

use crate::pki::{Certificate, PublicKey, TrustAnchor, matches_dns, verify_chain};

use super::{
    error::{TlsError, TlsResult},
    kx::{KeyPair, TrafficKey, derive_secret, extract},
    record::{MAX_FRAGMENT, Wire},
};

const EXT_SERVER_NAME: u16 = 0;
const EXT_SUPPORTED_GROUPS: u16 = 10;
const EXT_SIGNATURE_ALGORITHMS: u16 = 13;
const EXT_ALPN: u16 = 16;
const EXT_COOKIE: u16 = 33;
const EXT_SUPPORTED_VERSIONS: u16 = 43;
const EXT_KEY_SHARE: u16 = 51;
const EXT_HELLO_RETRY_REQUEST: u16 = 0xFF01;

const GROUP_X25519: u16 = 29;

const SIG_ECDSA_P256_SHA256: u16 = 0x0403;
const SIG_ECDSA_P384_SHA384: u16 = 0x0503;
const SIG_RSA_PSS_SHA256: u16 = 0x0804;
const SIG_RSA_PSS_SHA384: u16 = 0x0805;
const SIG_RSA_PKCS1_SHA256: u16 = 0x0401;
const SIG_RSA_PKCS1_SHA384: u16 = 0x0501;
const SIG_ED25519: u16 = 0x0807;

const HT_SERVER_HELLO: u8 = 2;
const HT_ENCRYPTED_EXTENSIONS: u8 = 8;
const HT_CERTIFICATE: u8 = 11;
const HT_CERTIFICATE_VERIFY: u8 = 15;
const HT_FINISHED: u8 = 20;
const HT_KEY_UPDATE: u8 = 24;

/// SHA-256 of the empty string, the transcript hash before any message.
const EMPTY_HASH: [u8; 32] = [
    0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93,
    0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55,
];

/// Parameters for one connection.
#[derive(Debug, Clone, Copy)]
pub struct ClientConfig<'a> {
    /// The host name used for SNI and certificate verification.
    pub hostname: &'a str,
    /// Trust anchors the server chain must terminate at.
    pub anchors: &'a [TrustAnchor],
    /// Handshake and read timeout used by [`TlsClient::connect`].
    pub timeout: Duration,
}

/// The ephemeral X25519 key pair of one handshake.
///
/// A [`ClientHelloHook`] receives this view so it can derive a shared secret
/// against a static peer key before the ClientHello leaves the machine.
#[derive(Clone, Copy)]
pub struct EphemeralKey<'a> {
    pair: &'a KeyPair,
}

impl EphemeralKey<'_> {
    /// The public key the ClientHello carries in `key_share`.
    pub fn public(&self) -> &[u8; 32] {
        self.pair.public()
    }

    /// ECDH against a peer public key.
    pub fn shared(&self, peer: &[u8; 32]) -> TlsResult<[u8; 32]> {
        self.pair.shared(peer)
    }
}

/// A hook that rewrites the completed ClientHello before it is sent.
pub trait ClientHelloHook {
    /// `hello` is the whole handshake message including its four-byte header,
    /// and `random` is the client random it already carries.
    fn rewrite(&self, hello: &mut Vec<u8>, random: &[u8; 32], key: EphemeralKey<'_>) -> Result<(), String>;
}

/// A replacement for the default chain and hostname validation.
pub trait CertificatePolicy {
    /// Return `Ok(())` only when the presented chain is acceptable for `hostname`.
    fn verify(&self, chain: &[Certificate<'_>], hostname: &str) -> Result<(), String>;
}

/// Per-connection deviations from the default handshake.
#[derive(Default)]
pub struct HandshakeOptions<'a> {
    /// ALPN protocols to offer; the default is `http/1.1`.
    pub alpn: Option<&'a [&'a str]>,
    /// ClientHello rewriter applied after the message is built.
    pub hello: Option<&'a dyn ClientHelloHook>,
    /// Replacement for certificate chain validation.
    pub policy: Option<&'a dyn CertificatePolicy>,
}

/// A connected TLS 1.3 client carrying application traffic.
pub struct TlsClient<S> {
    wire: Wire<S>,
    server_app: TrafficKey,
    client_app: TrafficKey,
    server_app_secret: [u8; 32],
    client_app_secret: [u8; 32],
    read_buffer: Vec<u8>,
    read_position: usize,
    negotiated_alpn: Option<String>,
}

impl TlsClient<TcpStream> {
    /// Dial `address` and perform the full handshake with the default options.
    pub fn connect<A: ToSocketAddrs>(config: &ClientConfig<'_>, address: A) -> TlsResult<Self> {
        Self::connect_with(config, &HandshakeOptions::default(), address)
    }

    /// Dial `address` and perform the full handshake with explicit options.
    pub fn connect_with<A: ToSocketAddrs>(config: &ClientConfig<'_>, options: &HandshakeOptions<'_>, address: A) -> TlsResult<Self> {
        let stream = TcpStream::connect(address).map_err(TlsError::Io)?;
        stream.set_read_timeout(Some(config.timeout)).map_err(TlsError::Io)?;
        stream.set_write_timeout(Some(config.timeout)).map_err(TlsError::Io)?;
        stream.set_nodelay(true).map_err(TlsError::Io)?;
        Self::handshake_with(config, options, stream)
    }
}

impl<S: Read + Write> TlsClient<S> {
    /// Perform the full handshake over an already connected stream.
    pub fn handshake(config: &ClientConfig<'_>, stream: S) -> TlsResult<Self> {
        Self::handshake_with(config, &HandshakeOptions::default(), stream)
    }

    /// Perform the full handshake, applying the given per-connection options.
    pub fn handshake_with(config: &ClientConfig<'_>, options: &HandshakeOptions<'_>, stream: S) -> TlsResult<Self> {
        let mut wire = Wire::new(stream);
        let keypair = KeyPair::generate()?;
        let mut transcript = Vec::new();
        let mut buffer = Vec::new();

        let mut cookie: Option<Vec<u8>> = None;
        let mut server_share: Option<[u8; 32]> = None;
        for attempt in 0..2 {
            let (mut client_hello, client_random) = build_client_hello(config.hostname, keypair.public(), cookie.as_deref(), options.alpn)?;
            if let Some(hook) = options.hello {
                hook.rewrite(&mut client_hello, &client_random, EphemeralKey { pair: &keypair }).map_err(TlsError::Protocol)?;
            }
            transcript.extend_from_slice(&client_hello);
            // The initial record carries the legacy 0x0301 version per RFC 8446 §5.1.
            wire.write_record_version(0x16, 0x03, 0x01, &client_hello)?;
            wire.flush()?;

            let hello = read_plaintext_handshake(&mut wire, &mut buffer)?;
            if hello[0] != HT_SERVER_HELLO {
                return Err(TlsError::Protocol(format!("expected ServerHello, got handshake type {}", hello[0])));
            }
            transcript.extend_from_slice(&hello);
            let (share, retry_cookie) = parse_server_hello(&hello[4..])?;
            if let Some(retry_cookie) = retry_cookie {
                if attempt == 1 {
                    return Err(TlsError::Protocol("second HelloRetryRequest".to_string()));
                }
                cookie = Some(retry_cookie);
                continue;
            }
            server_share = Some(share.ok_or_else(|| TlsError::Protocol("ServerHello has no key_share".to_string()))?);
            break;
        }
        let server_share = server_share.ok_or_else(|| TlsError::Protocol("no server key share".to_string()))?;

        // RFC 8446 §7.1: the handshake secret is extracted with the salt
        // Derive-Secret(early_secret, "derived", "").
        let early_secret = extract(&[0u8; 32], &[0u8; 32]);
        let derived_zero = derive_secret(&early_secret, "derived", &EMPTY_HASH)?;
        let handshake_secret = extract(&derived_zero, &keypair.shared(&server_share)?);
        let hash_after_sh = sha256(&transcript);
        let client_hs = derive_secret(&handshake_secret, "c hs traffic", &hash_after_sh)?;
        let server_hs = derive_secret(&handshake_secret, "s hs traffic", &hash_after_sh)?;
        let mut server_hs_key = TrafficKey::from_secret(&server_hs)?;
        let mut client_hs_key = TrafficKey::from_secret(&client_hs)?;
        let server_finished_key = derive_secret(&server_hs, "finished", &[])?;

        let mut alpn: Option<String> = None;
        let mut chain: Vec<Vec<u8>> = Vec::new();
        let mut verify_scheme = 0u16;
        let mut verify_signature: Vec<u8> = Vec::new();
        let mut cv_transcript_hash: Vec<u8> = Vec::new();
        let mut finished_transcript_len = 0usize;
        let mut server_finished: Option<Vec<u8>> = None;
        while server_finished.is_none() {
            let message = read_encrypted_handshake(&mut wire, &mut server_hs_key, &mut buffer)?;
            let message_type = message[0];
            let transcript_before = transcript.len();
            transcript.extend_from_slice(&message);
            let body = &message[4..];
            match message_type {
                HT_ENCRYPTED_EXTENSIONS => alpn = parse_alpn(body),
                HT_CERTIFICATE => parse_certificate(body, &mut chain)?,
                HT_CERTIFICATE_VERIFY => {
                    verify_scheme = u16::from_be_bytes([body[0], body[1]]);
                    let signature_len = u16::from_be_bytes([body[2], body[3]]) as usize;
                    verify_signature = body[4..4 + signature_len].to_vec();
                    // The CertificateVerify signature covers the transcript up to
                    // and including the Certificate message, never CV itself.
                    cv_transcript_hash = sha256(&transcript[..transcript_before]).to_vec();
                }
                HT_FINISHED => {
                    server_finished = Some(body.to_vec());
                    finished_transcript_len = transcript_before;
                }
                other => return Err(TlsError::Protocol(format!("unexpected handshake type {other}"))),
            }
        }

        let parsed: Vec<Certificate<'_>> = chain
            .iter()
            .map(|der| Certificate::parse(der).map_err(|error| TlsError::Certificate(error.to_string())))
            .collect::<Result<_, _>>()?;
        if parsed.is_empty() {
            return Err(TlsError::Certificate("empty certificate message".to_string()));
        }
        match options.policy {
            Some(policy) => policy.verify(&parsed, config.hostname).map_err(TlsError::Certificate)?,
            None => {
                let intermediates: Vec<&Certificate<'_>> = parsed[1..].iter().collect();
                let now = DateTime::now_utc();
                verify_chain(&parsed[0], &intermediates, config.anchors, &now).map_err(|error| TlsError::Certificate(error.to_string()))?;
                if !matches_dns(&parsed[0], config.hostname) {
                    return Err(TlsError::Certificate(format!("{} is not valid for {}", config.hostname, parsed[0].subject)));
                }
            }
        }
        verify_certificate_verify(verify_scheme, &parsed[0].public_key, &cv_transcript_hash, &verify_signature)?;

        let server_finished = server_finished.ok_or_else(|| TlsError::Protocol("no server Finished".to_string()))?;
        // The Finished MAC covers the transcript through CertificateVerify, not the Finished itself.
        let expected = hmac(&server_finished_key, &sha256(&transcript[..finished_transcript_len]))?;
        if expected[..] != server_finished[..] {
            return Err(TlsError::Protocol("server Finished does not verify".to_string()));
        }

        let client_finished_key = derive_secret(&client_hs, "finished", &[])?;
        let client_verify = hmac(&client_finished_key, &sha256(&transcript))?;
        let mut client_finished = Vec::with_capacity(4 + client_verify.len());
        client_finished.push(HT_FINISHED);
        client_finished.extend_from_slice(&(client_verify.len() as u32).to_be_bytes()[1..]);
        client_finished.extend_from_slice(&client_verify);
        // RFC 8446 §7.1: the application traffic secrets are derived over the
        // transcript up to and including the server's Finished, never the
        // client's own Finished.
        let app_hash = sha256(&transcript);
        wire.write_encrypted(&mut client_hs_key, 0x16, &client_finished)?;
        wire.flush()?;

        let derived = derive_secret(&handshake_secret, "derived", &EMPTY_HASH)?;
        let master = extract(&derived, &[0u8; 32]);
        let client_app_secret = derive_secret(&master, "c ap traffic", &app_hash)?;
        let server_app_secret = derive_secret(&master, "s ap traffic", &app_hash)?;
        Ok(Self {
            wire,
            server_app: TrafficKey::from_secret(&server_app_secret)?,
            client_app: TrafficKey::from_secret(&client_app_secret)?,
            server_app_secret,
            client_app_secret,
            read_buffer: Vec::new(),
            read_position: 0,
            negotiated_alpn: alpn,
        })
    }

    /// The ALPN protocol negotiated by the server, if any.
    pub fn alpn(&self) -> Option<&str> {
        self.negotiated_alpn.as_deref()
    }

    /// The underlying transport, for timeout or cancellation handles.
    pub fn socket(&self) -> &S {
        self.wire.stream()
    }

    /// The underlying transport, mutably.
    pub fn socket_mut(&mut self) -> &mut S {
        self.wire.stream_mut()
    }

    /// Send application data.
    pub fn send(&mut self, data: &[u8]) -> TlsResult<()> {
        for chunk in data.chunks(MAX_FRAGMENT) {
            self.wire.write_encrypted(&mut self.client_app, 0x17, chunk)?;
        }
        self.wire.flush()
    }

    /// Read application data into `out`, returning the byte count.
    pub fn receive(&mut self, out: &mut [u8]) -> TlsResult<usize> {
        loop {
            if self.read_position < self.read_buffer.len() {
                let take = out.len().min(self.read_buffer.len() - self.read_position);
                out[..take].copy_from_slice(&self.read_buffer[self.read_position..self.read_position + take]);
                self.read_position += take;
                if self.read_position == self.read_buffer.len() {
                    self.read_buffer.clear();
                    self.read_position = 0;
                }
                return Ok(take);
            }
            let (inner, body) = match self.wire.read_encrypted(&mut self.server_app) {
                Ok(pair) => pair,
                // A TCP close at a record boundary is the end of the stream.
                Err(TlsError::Eof) => return Ok(0),
                Err(error) => return Err(error),
            };
            match inner {
                0x17 => {
                    self.read_buffer = body;
                    self.read_position = 0;
                }
                0x15 => {
                    // An alert carries [level, description]; `close_notify` is a
                    // clean end of stream, anything else is a fatal failure.
                    let description = body.get(1).copied().unwrap_or(0);
                    if description == 0 {
                        return Ok(0);
                    }
                    return Err(TlsError::Alert(description));
                }
                0x18 => {
                    let next = derive_secret(&self.server_app_secret, "traffic upd", &[])?;
                    self.server_app_secret = next;
                    self.server_app = TrafficKey::from_secret(&next)?;
                    if body.first() == Some(&1) {
                        let next_client = derive_secret(&self.client_app_secret, "traffic upd", &[])?;
                        self.client_app_secret = next_client;
                        self.client_app = TrafficKey::from_secret(&next_client)?;
                        self.wire.write_encrypted(&mut self.client_app, 0x18, &[0])?;
                        self.wire.flush()?;
                    }
                }
                0x16 => {
                    // Post-handshake messages such as NewSessionTicket are ignored.
                }
                other => return Err(TlsError::Protocol(format!("unexpected encrypted type {other}"))),
            }
        }
    }

    /// Send `close_notify`.
    pub fn close(&mut self) -> TlsResult<()> {
        self.wire.write_encrypted(&mut self.client_app, 0x15, &[1, 0])?;
        self.wire.flush()
    }
}

impl<S: Read + Write> Read for TlsClient<S> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        match self.receive(out) {
            Ok(count) => Ok(count),
            Err(TlsError::Io(error)) => Err(error),
            Err(error) => Err(io::Error::new(io::ErrorKind::InvalidData, error.to_string())),
        }
    }
}

impl<S: Read + Write> Write for TlsClient<S> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        match self.send(data) {
            Ok(()) => Ok(data.len()),
            Err(TlsError::Io(error)) => Err(error),
            Err(error) => Err(io::Error::new(io::ErrorKind::InvalidData, error.to_string())),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.wire.flush().map_err(|error| match error {
            TlsError::Io(error) => error,
            error => io::Error::new(io::ErrorKind::InvalidData, error.to_string()),
        })
    }
}

/// The read half of an established TLS 1.3 connection.
///
/// Splitting lets a protocol such as `xtls-rprx-vision` hand the two directions
/// to different threads and later leave TLS framing on one direction at a time.
/// A `KeyUpdate` request from the server is applied to the read key but not
/// answered, because the reply would need the write half.
pub struct TlsReader<S> {
    wire: Wire<S>,
    app: TrafficKey,
    app_secret: [u8; 32],
    buffer: Vec<u8>,
    position: usize,
}

impl<S: Read> TlsReader<S> {
    /// Read application data into `out`, returning the byte count. A clean TCP
    /// close at a record boundary reports end of stream as zero bytes.
    pub fn receive(&mut self, out: &mut [u8]) -> TlsResult<usize> {
        loop {
            if self.position < self.buffer.len() {
                let take = out.len().min(self.buffer.len() - self.position);
                out[..take].copy_from_slice(&self.buffer[self.position..self.position + take]);
                self.position += take;
                if self.position == self.buffer.len() {
                    self.buffer.clear();
                    self.position = 0;
                }
                return Ok(take);
            }
            let (inner, body) = match self.wire.read_encrypted(&mut self.app) {
                Ok(pair) => pair,
                Err(TlsError::Eof) => return Ok(0),
                Err(error) => return Err(error),
            };
            match inner {
                0x17 => {
                    self.buffer = body;
                    self.position = 0;
                }
                0x15 => {
                    let description = body.get(1).copied().unwrap_or(0);
                    if description == 0 {
                        return Ok(0);
                    }
                    return Err(TlsError::Alert(description));
                }
                0x18 => {
                    let next = derive_secret(&self.app_secret, "traffic upd", &[])?;
                    self.app_secret = next;
                    self.app = TrafficKey::from_secret(&next)?;
                }
                0x16 => {}
                other => return Err(TlsError::Protocol(format!("unexpected encrypted type {other}"))),
            }
        }
    }
}

impl<S: Read> Read for TlsReader<S> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        match self.receive(out) {
            Ok(count) => Ok(count),
            Err(TlsError::Io(error)) => Err(error),
            Err(error) => Err(io::Error::new(io::ErrorKind::InvalidData, error.to_string())),
        }
    }
}

impl TlsReader<TcpStream> {
    /// Give up TLS framing and return the socket plus bytes already decrypted
    /// or read past the last complete record, in delivery order.
    pub fn into_raw(self) -> (TcpStream, Vec<u8>) {
        let Self { wire, buffer, position, .. } = self;
        let (stream, pending) = wire.into_parts();
        let mut leftover = buffer[position..].to_vec();
        leftover.extend_from_slice(&pending);
        (stream, leftover)
    }
}

/// The write half of an established TLS 1.3 connection.
pub struct TlsWriter<S> {
    wire: Wire<S>,
    app: TrafficKey,
}

impl<S: Write> TlsWriter<S> {
    /// Encrypt and send application data, flushing the records.
    pub fn send(&mut self, data: &[u8]) -> TlsResult<()> {
        for chunk in data.chunks(MAX_FRAGMENT) {
            self.wire.write_encrypted(&mut self.app, 0x17, chunk)?;
        }
        self.wire.flush()
    }

    /// Send `close_notify`.
    pub fn close(&mut self) -> TlsResult<()> {
        self.wire.write_encrypted(&mut self.app, 0x15, &[1, 0])?;
        self.wire.flush()
    }
}

impl<S: Write> Write for TlsWriter<S> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        match self.send(data) {
            Ok(()) => Ok(data.len()),
            Err(TlsError::Io(error)) => Err(error),
            Err(error) => Err(io::Error::new(io::ErrorKind::InvalidData, error.to_string())),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.wire.flush().map_err(|error| match error {
            TlsError::Io(error) => error,
            error => io::Error::new(io::ErrorKind::InvalidData, error.to_string()),
        })
    }
}

impl TlsWriter<TcpStream> {
    /// Give up TLS framing and return the socket.
    pub fn into_raw(self) -> TcpStream {
        self.wire.into_parts().0
    }
}

impl TlsClient<TcpStream> {
    /// Split an established connection into independent read and write halves.
    pub fn split(self) -> TlsResult<(TlsReader<TcpStream>, TlsWriter<TcpStream>)> {
        let writer_stream = self.wire.stream().try_clone().map_err(TlsError::Io)?;
        let TlsClient { wire, server_app, server_app_secret, client_app, read_buffer, read_position, .. } = self;
        let (stream, pending) = wire.into_parts();
        let reader = TlsReader {
            wire: Wire::with_pending(stream, pending),
            app: server_app,
            app_secret: server_app_secret,
            buffer: read_buffer,
            position: read_position,
        };
        let writer = TlsWriter { wire: Wire::new(writer_stream), app: client_app };
        Ok((reader, writer))
    }
}

/// HMAC-SHA256 over one Finished transcript hash.
fn hmac(key: &[u8], data: &[u8]) -> TlsResult<[u8; 32]> {
    hmac_sha256(key, data).map_err(|error| TlsError::Unsupported(format!("hmac: {error}")))
}

/// Extract the next complete handshake message from the buffer, if present.
fn extract_message(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    if buffer.len() < 4 {
        return None;
    }
    let length = u32::from_be_bytes([0, buffer[1], buffer[2], buffer[3]]) as usize;
    if buffer.len() < 4 + length {
        return None;
    }
    let message = buffer[..4 + length].to_vec();
    buffer.drain(..4 + length);
    Some(message)
}

/// Read the next plaintext handshake message, skipping compatibility records.
fn read_plaintext_handshake<S: Read + Write>(wire: &mut Wire<S>, buffer: &mut Vec<u8>) -> TlsResult<Vec<u8>> {
    loop {
        if let Some(message) = extract_message(buffer) {
            return Ok(message);
        }
        let (record_type, body) = wire.read_record()?;
        if record_type == 0x14 && body.len() == 1 && body[0] == 0x01 {
            continue;
        }
        if record_type == 0x15 {
            return Err(TlsError::Alert(body.get(1).copied().unwrap_or(0)));
        }
        if record_type != 0x16 {
            return Err(TlsError::Protocol(format!("unexpected record type {record_type} during handshake")));
        }
        buffer.extend_from_slice(&body);
    }
}

/// Read the next encrypted handshake message.
fn read_encrypted_handshake<S: Read + Write>(wire: &mut Wire<S>, key: &mut TrafficKey, buffer: &mut Vec<u8>) -> TlsResult<Vec<u8>> {
    loop {
        if let Some(message) = extract_message(buffer) {
            return Ok(message);
        }
        let (inner, body) = wire.read_encrypted(key)?;
        if inner == HT_KEY_UPDATE {
            return Err(TlsError::Protocol("KeyUpdate during handshake is unsupported".to_string()));
        }
        if inner == 0x15 {
            return Err(TlsError::Alert(body.first().copied().unwrap_or(0)));
        }
        if inner != 0x16 {
            return Err(TlsError::Protocol(format!("unexpected encrypted type {inner} during handshake")));
        }
        buffer.extend_from_slice(&body);
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn take(&mut self, length: usize) -> TlsResult<&'a [u8]> {
        let end = self.position.checked_add(length).ok_or_else(|| TlsError::Protocol("cursor overflow".to_string()))?;
        let slice = self.data.get(self.position..end).ok_or_else(|| TlsError::Protocol("short handshake message".to_string()))?;
        self.position = end;
        Ok(slice)
    }

    fn u8(&mut self) -> TlsResult<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> TlsResult<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u24(&mut self) -> TlsResult<u32> {
        let bytes = self.take(3)?;
        Ok(u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]))
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.position
    }
}

fn extension(out: &mut Vec<u8>, kind: u16, body: &[u8]) {
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(body);
}

fn build_client_hello(hostname: &str, key_share: &[u8; 32], cookie: Option<&[u8]>, alpn: Option<&[&str]>) -> TlsResult<(Vec<u8>, [u8; 32])> {
    let mut random = [0u8; 32];
    let mut session_id = [0u8; 32];
    let mut entropy = Secure::system();
    entropy.fill(&mut random).map_err(|error| TlsError::Unsupported(format!("host entropy: {error}")))?;
    entropy.fill(&mut session_id).map_err(|error| TlsError::Unsupported(format!("host entropy: {error}")))?;

    let mut extensions = Vec::new();
    {
        let mut sni = Vec::new();
        let host = hostname.as_bytes();
        sni.extend_from_slice(&((host.len() + 3) as u16).to_be_bytes());
        sni.push(0);
        sni.extend_from_slice(&(host.len() as u16).to_be_bytes());
        sni.extend_from_slice(host);
        extension(&mut extensions, EXT_SERVER_NAME, &sni);
    }
    extension(&mut extensions, EXT_SUPPORTED_GROUPS, &[0, 6, 0, 29, 0, 23, 0, 24]);
    extension(&mut extensions, EXT_SIGNATURE_ALGORITHMS, &[0, 14, 4, 3, 5, 3, 8, 4, 8, 5, 4, 1, 5, 1, 8, 7]);
    {
        let mut protocols = Vec::new();
        for protocol in alpn.unwrap_or(&["http/1.1"]) {
            if protocol.len() > u8::MAX as usize {
                return Err(TlsError::Unsupported(format!("ALPN protocol name of {} bytes", protocol.len())));
            }
            protocols.push(protocol.len() as u8);
            protocols.extend_from_slice(protocol.as_bytes());
        }
        if protocols.len() > u16::MAX as usize {
            return Err(TlsError::Unsupported("ALPN protocol list is too long".to_string()));
        }
        let mut body = Vec::with_capacity(2 + protocols.len());
        body.extend_from_slice(&(protocols.len() as u16).to_be_bytes());
        body.extend_from_slice(&protocols);
        extension(&mut extensions, EXT_ALPN, &body);
    }
    if let Some(cookie) = cookie {
        let mut body = Vec::new();
        body.extend_from_slice(&(cookie.len() as u16).to_be_bytes());
        body.extend_from_slice(cookie);
        extension(&mut extensions, EXT_COOKIE, &body);
    }
    // The inner `versions` list of `supported_versions` carries a one-byte
    // length, unlike the other extension lists. Real-world stacks advertise
    // TLS 1.3 and TLS 1.2; servers in the field reject a 1.3-only list with
    // `protocol_version`.
    extension(&mut extensions, EXT_SUPPORTED_VERSIONS, &[4, 0x03, 0x04, 0x03, 0x03]);
    {
        let mut body = Vec::new();
        body.extend_from_slice(&[0, 36]);
        body.extend_from_slice(&GROUP_X25519.to_be_bytes());
        body.extend_from_slice(&[0, 32]);
        body.extend_from_slice(key_share);
        extension(&mut extensions, EXT_KEY_SHARE, &body);
    }

    let mut body = Vec::new();
    body.extend_from_slice(&[0x03, 0x03]);
    body.extend_from_slice(&random);
    body.push(32);
    body.extend_from_slice(&session_id);
    body.extend_from_slice(&[0, 2, 0x13, 0x01]);
    body.extend_from_slice(&[1, 0]);
    body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
    body.extend_from_slice(&extensions);

    let mut message = Vec::with_capacity(4 + body.len());
    message.push(1);
    message.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    message.extend_from_slice(&body);
    Ok((message, random))
}

/// The key share and HelloRetryRequest cookie extracted from a ServerHello.
type ServerHelloOutcome = (Option<[u8; 32]>, Option<Vec<u8>>);

fn parse_server_hello(message: &[u8]) -> TlsResult<ServerHelloOutcome> {
    let mut cursor = Cursor::new(message);
    if cursor.u16()? != 0x0303 {
        return Err(TlsError::Protocol("legacy version is not 0x0303".to_string()));
    }
    cursor.take(32)?;
    let session_id_len = cursor.u8()? as usize;
    cursor.take(session_id_len)?;
    let cipher = cursor.u16()?;
    if cipher != 0x1301 {
        return Err(TlsError::Protocol(format!("server selected unsupported cipher suite {cipher:#06x}")));
    }
    let compression = cursor.u8()?;
    if compression != 0 {
        return Err(TlsError::Protocol("server selected compression".to_string()));
    }
    let extensions_len = cursor.u16()? as usize;
    let mut extensions = cursor.take(extensions_len)?;
    let mut key_share: Option<[u8; 32]> = None;
    let mut cookie: Option<Vec<u8>> = None;
    let mut retry = false;
    while !extensions.is_empty() {
        let kind = u16::from_be_bytes([extensions[0], extensions[1]]);
        let length = u16::from_be_bytes([extensions[2], extensions[3]]) as usize;
        let body = &extensions[4..4 + length];
        extensions = &extensions[4 + length..];
        match kind {
            EXT_SUPPORTED_VERSIONS => {
                if body != [0x03, 0x04] {
                    return Err(TlsError::Protocol("server did not select TLS 1.3".to_string()));
                }
            }
            EXT_KEY_SHARE => {
                let mut share = Cursor::new(body);
                let group = share.u16()?;
                if group != GROUP_X25519 {
                    return Err(TlsError::Protocol(format!("server key share group {group} is not x25519")));
                }
                let exchange_len = share.u16()? as usize;
                if exchange_len != 32 {
                    return Err(TlsError::Protocol(format!("server x25519 key share length {exchange_len}")));
                }
                let value = share.take(32)?;
                key_share = Some(value.try_into().expect("32 bytes"));
            }
            EXT_COOKIE => cookie = Some(body.to_vec()),
            EXT_HELLO_RETRY_REQUEST => retry = true,
            _ => {}
        }
    }
    if retry {
        let cookie = cookie.ok_or_else(|| TlsError::Protocol("HelloRetryRequest without cookie".to_string()))?;
        return Ok((None, Some(cookie)));
    }
    Ok((key_share, None))
}

fn parse_alpn(message: &[u8]) -> Option<String> {
    let mut cursor = Cursor::new(message);
    let Ok(length) = cursor.u16()
    else {
        return None;
    };
    let Ok(mut extensions) = cursor.take(length as usize)
    else {
        return None;
    };
    while extensions.len() >= 4 {
        let kind = u16::from_be_bytes([extensions[0], extensions[1]]);
        let length = u16::from_be_bytes([extensions[2], extensions[3]]) as usize;
        if extensions.len() < 4 + length {
            break;
        }
        let body = &extensions[4..4 + length];
        if kind == EXT_ALPN && body.len() >= 3 {
            let name_len = body[2] as usize;
            if body.len() >= 3 + name_len {
                return String::from_utf8(body[3..3 + name_len].to_vec()).ok();
            }
        }
        extensions = &extensions[4 + length..];
    }
    None
}

fn parse_certificate(message: &[u8], chain: &mut Vec<Vec<u8>>) -> TlsResult<()> {
    let mut cursor = Cursor::new(message);
    let context_len = cursor.u8()? as usize;
    cursor.take(context_len)?;
    let list_len = cursor.u24()? as usize;
    let mut list = Cursor::new(cursor.take(list_len)?);
    while list.remaining() > 0 {
        let cert_len = list.u24()? as usize;
        chain.push(list.take(cert_len)?.to_vec());
        let extensions_len = list.u16()? as usize;
        list.take(extensions_len)?;
    }
    Ok(())
}

fn verify_certificate_verify(scheme: u16, key: &PublicKey, transcript_hash: &[u8], signature: &[u8]) -> TlsResult<()> {
    let mut content = vec![0x20u8; 64];
    content.extend_from_slice(b"TLS 1.3, server CertificateVerify");
    content.push(0);
    content.extend_from_slice(transcript_hash);
    match (scheme, key) {
        (SIG_ECDSA_P256_SHA256, PublicKey::Ec { curve: crate::pki::EcCurve::P256, point }) => {
            let verifying = p256::VerifyingKey::from_sec1_bytes(point).map_err(|_| TlsError::Certificate("bad leaf key".to_string()))?;
            verifying.verify_der(&content, signature).map_err(|_| TlsError::Certificate("CertificateVerify signature invalid".to_string()))
        }
        (SIG_ECDSA_P384_SHA384, PublicKey::Ec { curve: crate::pki::EcCurve::P384, point }) => {
            let verifying = p384::VerifyingKey::from_sec1_bytes(point).map_err(|_| TlsError::Certificate("bad leaf key".to_string()))?;
            verifying.verify_der(&content, signature).map_err(|_| TlsError::Certificate("CertificateVerify signature invalid".to_string()))
        }
        (SIG_ED25519, PublicKey::Ed25519(key)) => {
            ed25519::verify(key, &content, signature).map_err(|_| TlsError::Certificate("CertificateVerify signature invalid".to_string()))
        }
        (SIG_RSA_PKCS1_SHA256 | SIG_RSA_PSS_SHA256 | SIG_RSA_PKCS1_SHA384 | SIG_RSA_PSS_SHA384, PublicKey::Rsa { modulus, exponent }) => {
            let key =
                rsa::PublicKey::from_spki_der(&spki_der(modulus, exponent)).map_err(|_| TlsError::Certificate("bad RSA key".to_string()))?;
            let result = match scheme {
                SIG_RSA_PKCS1_SHA256 => key.verify_pkcs1v15_sha256(&content, signature),
                SIG_RSA_PKCS1_SHA384 => key.verify_pkcs1v15_sha384(&content, signature),
                SIG_RSA_PSS_SHA256 => key.verify_pss_sha256(&content, signature),
                _ => key.verify_pss_sha384(&content, signature),
            };
            result.map_err(|_| TlsError::Certificate("CertificateVerify signature invalid".to_string()))
        }
        (scheme, _) => Err(TlsError::Protocol(format!("unsupported signature scheme {scheme:#06x}"))),
    }
}

/// Encode an RSA public key as the `SubjectPublicKeyInfo` the crypto facade parses.
fn spki_der(modulus: &[u8], exponent: &u64) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.sequence(|outer| {
        outer.sequence(|algorithm| {
            algorithm.write_oid(&oids::rsa_encryption());
            algorithm.write_null();
        });
        let mut inner = Writer::new();
        inner.sequence(|rsa| {
            rsa.write_integer(modulus);
            rsa.write_u64(*exponent);
        });
        outer.write_bitstring(&inner.finish(), 0);
    });
    writer.finish()
}

/// Hex helper kept for diagnostics in tests.
#[cfg(test)]
fn hex(bytes: &[u8]) -> String {
    use boost_crypto::encoding::hex_encode;
    hex_encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_hello_is_a_well_formed_handshake_message() {
        let hello = build_client_hello("example.com", &[7u8; 32], None, None).expect("hello").0;
        assert_eq!(hello[0], 1);
        let length = u32::from_be_bytes([0, hello[1], hello[2], hello[3]]) as usize;
        assert_eq!(length, hello.len() - 4);
        // Legacy version 0x0303 and a 32-byte session id.
        assert_eq!(&hello[4..6], &[0x03, 0x03]);
        assert_eq!(hello[6 + 32], 32);
        // The trailing extensions carry `supported_versions` with TLS 1.3.
        assert!(hello.windows(6).any(|window| window == &[0x00, 0x2b, 0x00, 0x05, 0x04, 0x03]), "supported_versions extension present");
    }

    #[test]
    fn server_hello_rejects_tls12_selection() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0x03, 0x03]);
        body.extend_from_slice(&[0u8; 32]);
        body.push(0);
        body.extend_from_slice(&[0x13, 0x01, 0x00]);
        let mut extensions = Vec::new();
        extension(&mut extensions, EXT_SUPPORTED_VERSIONS, &[0x03, 0x03]);
        body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        body.extend_from_slice(&extensions);
        let error = parse_server_hello(&body).expect_err("TLS 1.2 must be rejected");
        assert!(matches!(error, TlsError::Protocol(_)));
    }

    #[test]
    fn rsa_spki_round_trips_through_the_der_writer() {
        let modulus = [0xABu8; 256];
        let der = spki_der(&modulus, &65537);
        let mut parsed = data_binary_asn1::Reader::new(&der);
        assert_eq!(parsed.expect(data_binary_asn1::Tag::SEQUENCE).unwrap().value.len(), der.len() - 4);
    }

    #[test]
    fn hex_helper_is_lowercase() {
        assert_eq!(hex(&[0x0A, 0xFF]), "0aff");
    }
}
