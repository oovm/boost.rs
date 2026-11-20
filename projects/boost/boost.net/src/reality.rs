//! REALITY client authentication over the product TLS 1.3 client.
//!
//! REALITY hides an authenticated tunnel inside a normal TLS 1.3 handshake: the
//! client seals 16 bytes of plaintext into the 32-byte `session_id`, and the
//! server proves itself by signing the transcript with a temporary Ed25519
//! certificate whose signature is an HMAC over its public key. The TLS stack
//! itself stays in [`crate::tls`]; this module only supplies the ClientHello
//! rewrite and the certificate policy.
//!
//! Reference: Xray-core `transport/internet/reality` (MPL-2.0).

use core::cell::Cell;
use std::{
    net::{TcpStream, ToSocketAddrs},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use boost_crypto::{
    aead::aes_256_gcm_seal,
    mac::{hkdf_expand_sha256, hkdf_extract_sha256, hmac_sha512},
};

use crate::{
    pki::{Certificate, PublicKey, TrustAnchor},
    tls::{CertificatePolicy, ClientConfig, ClientHelloHook, EphemeralKey, HandshakeOptions, TlsClient, TlsError, TlsResult},
};

/// The TLS record offset of the 32-byte session id inside a ClientHello.
///
/// Four bytes of handshake header, two bytes of legacy version, 32 bytes of
/// random, and one byte of session id length.
const SESSION_ID_OFFSET: usize = 39;
/// The REALITY session id length.
const SESSION_ID_LEN: usize = 32;
/// The number of plaintext bytes sealed into the session id.
const AUTH_PLAINTEXT_LEN: usize = 16;
/// ALPN offered to the server, matching a browser's TLS 1.3 ClientHello.
const DEFAULT_ALPN: &[&str] = &["h2", "http/1.1"];

/// REALITY parameters for one node.
#[derive(Debug, Clone)]
pub struct RealityConfig<'a> {
    /// The SNI the ClientHello carries and the server certificate is checked against.
    pub server_name: &'a str,
    /// The server's static X25519 public key from the node configuration.
    pub public_key: [u8; 32],
    /// The short id assigned to this node, at most 16 bytes.
    pub short_id: &'a [u8],
    /// The client version advertised to the server as `x.y.z`.
    pub version: [u8; 3],
    /// ALPN protocols to offer; defaults to `h2` and `http/1.1`.
    pub alpn: Option<&'a [&'a str]>,
}

/// A TLS connection whose peer proved it holds the REALITY server key.
pub struct RealityConnection {
    tls: TlsClient<TcpStream>,
}

impl RealityConnection {
    /// Encrypt and send application data through the tunnel.
    pub fn send(&mut self, data: &[u8]) -> TlsResult<()> {
        self.tls.send(data)
    }

    /// Decrypt application data into `out`.
    pub fn receive(&mut self, out: &mut [u8]) -> TlsResult<usize> {
        self.tls.receive(out)
    }

    /// The underlying TCP stream, for timeout and socket options.
    pub fn socket_mut(&mut self) -> &mut TcpStream {
        self.tls.socket_mut()
    }

    /// The TLS client, for protocols that need to take over the raw stream.
    pub fn into_inner(self) -> TlsClient<TcpStream> {
        self.tls
    }
}

/// The auth key derived during the ClientHello rewrite, shared with the policy.
struct SharedAuth {
    key: Cell<[u8; 32]>,
}

/// Seals the REALITY session id into the ClientHello.
struct SealSessionId<'a> {
    config: &'a RealityConfig<'a>,
    auth: &'a SharedAuth,
}

impl ClientHelloHook for SealSessionId<'_> {
    fn rewrite(&self, hello: &mut Vec<u8>, random: &[u8; 32], key: EphemeralKey<'_>) -> Result<(), String> {
        let shared = key.shared(&self.config.public_key).map_err(|error| error.to_string())?;
        let auth = seal_session_id(self.config, hello, random, &shared)?;
        self.auth.key.set(auth);
        Ok(())
    }
}

/// Zero the session id, seal the REALITY plaintext into it, and return the auth key.
fn seal_session_id(config: &RealityConfig<'_>, hello: &mut [u8], random: &[u8; 32], shared: &[u8; 32]) -> Result<[u8; 32], String> {
    if config.short_id.len() > AUTH_PLAINTEXT_LEN - 8 {
        return Err("REALITY short id is longer than 8 bytes".to_string());
    }
    if hello.len() < SESSION_ID_OFFSET + SESSION_ID_LEN {
        return Err("ClientHello is too short to carry a REALITY session id".to_string());
    }
    // The AAD is the ClientHello with the session id zeroed, which is what the
    // server reconstructs before opening the sealed bytes.
    hello[SESSION_ID_OFFSET..SESSION_ID_OFFSET + SESSION_ID_LEN].fill(0);

    let mut plaintext = [0u8; SESSION_ID_LEN];
    plaintext[..3].copy_from_slice(&config.version);
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|error| error.to_string())?.as_secs() as u32;
    plaintext[4..8].copy_from_slice(&seconds.to_be_bytes());
    plaintext[8..8 + config.short_id.len()].copy_from_slice(config.short_id);

    let prk = hkdf_extract_sha256(Some(&random[..20]), shared);
    let mut auth = [0u8; 32];
    hkdf_expand_sha256(&prk, b"REALITY", &mut auth).map_err(|error| error.to_string())?;
    let sealed = aes_256_gcm_seal(&auth, &random[20..], hello, &plaintext[..AUTH_PLAINTEXT_LEN]).map_err(|error| error.to_string())?;
    hello[SESSION_ID_OFFSET..SESSION_ID_OFFSET + SESSION_ID_LEN].copy_from_slice(&sealed);
    Ok(auth)
}

/// Accepts only the temporary certificate the REALITY server derives from the auth key.
struct RealityPolicy<'a> {
    auth: &'a SharedAuth,
}

impl CertificatePolicy for RealityPolicy<'_> {
    fn verify(&self, chain: &[Certificate<'_>], _hostname: &str) -> Result<(), String> {
        let leaf = chain.first().ok_or_else(|| "the server sent no certificate".to_string())?;
        let PublicKey::Ed25519(public) = &leaf.public_key
        else {
            return Err("the server presented a regular certificate; REALITY authentication failed".to_string());
        };
        let expected = hmac_sha512(&self.auth.key.get(), public).map_err(|error| error.to_string())?;
        if expected[..] != *leaf.signature {
            return Err("the server certificate is not signed by the REALITY auth key".to_string());
        }
        Ok(())
    }
}

/// Dial `address` and complete a REALITY-authenticated TLS 1.3 handshake.
pub fn connect<A: ToSocketAddrs>(
    config: &RealityConfig<'_>,
    anchors: &[TrustAnchor],
    address: A,
    timeout: Duration,
) -> TlsResult<RealityConnection> {
    if config.short_id.len() > AUTH_PLAINTEXT_LEN - 8 {
        return Err(TlsError::Unsupported("the REALITY short id is longer than 8 bytes".to_string()));
    }
    let auth = SharedAuth { key: Cell::new([0u8; 32]) };
    let hook = SealSessionId { config, auth: &auth };
    let policy = RealityPolicy { auth: &auth };
    let options = HandshakeOptions { alpn: Some(config.alpn.unwrap_or(DEFAULT_ALPN)), hello: Some(&hook), policy: Some(&policy) };
    let client = TlsClient::connect_with(&ClientConfig { hostname: config.server_name, anchors, timeout }, &options, address)?;
    Ok(RealityConnection { tls: client })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(short_id: &[u8]) -> RealityConfig<'_> {
        RealityConfig { server_name: "example.com", public_key: [7u8; 32], short_id, version: [1, 8, 0], alpn: None }
    }

    fn hello() -> Vec<u8> {
        let mut hello = vec![0u8; SESSION_ID_OFFSET + SESSION_ID_LEN + 4];
        hello[0] = 1;
        hello[SESSION_ID_OFFSET..SESSION_ID_OFFSET + SESSION_ID_LEN].fill(0xAA);
        hello
    }

    #[test]
    fn sealing_is_deterministic_and_leaves_the_aad_region_clean() {
        let config = config(&[1, 2, 3, 4]);
        let random = [9u8; 32];
        let mut first = hello();
        let auth_a = seal_session_id(&config, &mut first, &random, &[5u8; 32]).expect("seal");
        let mut second = hello();
        let auth_b = seal_session_id(&config, &mut second, &random, &[5u8; 32]).expect("seal");
        assert_eq!(first, second);
        assert_eq!(auth_a, auth_b);
        assert_ne!(auth_a, [0u8; 32]);
        assert_ne!(&first[SESSION_ID_OFFSET..SESSION_ID_OFFSET + SESSION_ID_LEN], &[0xAAu8; SESSION_ID_LEN]);
    }

    #[test]
    fn short_id_longer_than_eight_bytes_is_rejected() {
        let config = config(&[0u8; 9]);
        let error = seal_session_id(&config, &mut hello(), &[9u8; 32], &[5u8; 32]).expect_err("reject");
        assert!(error.contains("short id"));
    }

    #[test]
    fn a_different_shared_secret_changes_the_ciphertext() {
        let config = config(&[]);
        let mut first = hello();
        seal_session_id(&config, &mut first, &[9u8; 32], &[5u8; 32]).expect("seal");
        let mut second = hello();
        seal_session_id(&config, &mut second, &[9u8; 32], &[6u8; 32]).expect("seal");
        assert_ne!(first, second);
    }
}
