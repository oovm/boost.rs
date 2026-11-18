//! Server-side TLS for the PostgreSQL protocol.
//!
//! A client asks for TLS with an `SSLRequest` startup packet; the session then
//! answers `S` and the protocol continues inside the TLS tunnel. The engine
//! keeps the certificate and key as PEM or DER, so a host configures TLS without
//! the gateway owning a key store.

use std::{io, sync::Arc};

use base64::Engine as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// Failure raised while building a TLS acceptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsError {
    /// The PEM text carries no usable block.
    Malformed(String),
    /// `rustls` rejected the certificate or key.
    Rejected(String),
}

impl TlsError {
    /// Human-readable detail.
    pub fn message(&self) -> String {
        match self {
            Self::Malformed(detail) | Self::Rejected(detail) => detail.clone(),
        }
    }
}

/// Server TLS configuration the gateway serves `SSLRequest` from.
#[derive(Debug)]
pub struct PgTls {
    config: Arc<rustls::ServerConfig>,
    require_client_tls: bool,
}

impl PgTls {
    /// Builds the acceptor from PEM text.
    ///
    /// The certificate chain is every `CERTIFICATE` block in `cert_pem`; the key
    /// is the first `PRIVATE KEY` (PKCS#8), `RSA PRIVATE KEY` (PKCS#1) or
    /// `EC PRIVATE KEY` (SEC1) block in `key_pem`.
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Self, TlsError> {
        let Some(certificates) = read_pem_blocks(cert_pem, "CERTIFICATE")?
        else {
            return Err(TlsError::Malformed("PEM text carries no `CERTIFICATE` block".to_string()));
        };
        let chain: Vec<CertificateDer<'static>> = certificates.into_iter().map(CertificateDer::from).collect();
        let key = read_private_key(key_pem)?;
        Self::from_der(chain, key)
    }

    /// Builds the acceptor from already decoded DER material.
    pub fn from_der(chain: Vec<CertificateDer<'static>>, key: PrivateKeyDer<'static>) -> Result<Self, TlsError> {
        if chain.is_empty() {
            return Err(TlsError::Malformed("the certificate chain is empty".to_string()));
        }
        let config = rustls::ServerConfig::builder().with_no_client_auth().with_single_cert(chain, key).map_err(|error| TlsError::Rejected(error.to_string()))?;
        Ok(Self { config: Arc::new(config), require_client_tls: false })
    }

    /// Refuses a plaintext `StartupMessage` with `28000` instead of serving it.
    ///
    /// The default is offer-only: a client that skips `SSLRequest` keeps running
    /// the protocol in plaintext, which is what a stock `psql` expects from a
    /// server that does not advertise `ssl = on`.
    pub fn require_client_tls(mut self) -> Self {
        self.require_client_tls = true;
        self
    }

    /// Whether a plaintext `StartupMessage` is refused.
    pub fn requires_client_tls(&self) -> bool {
        self.require_client_tls
    }

    /// One server-side connection for an accepted socket.
    pub(crate) fn server_connection(&self) -> io::Result<rustls::ServerConnection> {
        rustls::ServerConnection::new(Arc::clone(&self.config)).map_err(|error| io::Error::other(format!("TLS server connection failed: {error}")))
    }
}

fn read_private_key(pem: &str) -> Result<PrivateKeyDer<'static>, TlsError> {
    if let Some(blocks) = read_pem_blocks(pem, "PRIVATE KEY")? {
        if let Some(block) = blocks.into_iter().next() {
            return Ok(PrivateKeyDer::Pkcs8(block.into()));
        }
    }
    if let Some(blocks) = read_pem_blocks(pem, "RSA PRIVATE KEY")? {
        if let Some(block) = blocks.into_iter().next() {
            return Ok(PrivateKeyDer::Pkcs1(block.into()));
        }
    }
    if let Some(blocks) = read_pem_blocks(pem, "EC PRIVATE KEY")? {
        if let Some(block) = blocks.into_iter().next() {
            return Ok(PrivateKeyDer::Sec1(block.into()));
        }
    }
    Err(TlsError::Malformed("PEM text carries no private key block".to_string()))
}

/// Decodes every `BEGIN <tag>` / `END <tag>` block, returning `None` when the
/// text carries no block with that tag at all.
fn read_pem_blocks(pem: &str, tag: &str) -> Result<Option<Vec<Vec<u8>>>, TlsError> {
    let begin = format!("-----BEGIN {tag}-----");
    let end = format!("-----END {tag}-----");
    let mut blocks = Vec::new();
    let mut rest = pem;
    while let Some(start) = rest.find(&begin) {
        let body_start = start + begin.len();
        let Some(close) = rest[body_start..].find(&end) else {
            return Err(TlsError::Malformed(format!("unterminated `{tag}` PEM block")));
        };
        let body = &rest[body_start..body_start + close];
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(body.split_whitespace().collect::<String>())
            .map_err(|error| TlsError::Malformed(format!("invalid base64 in `{tag}` PEM block: {error}")))?;
        blocks.push(decoded);
        rest = &rest[body_start + close + end.len()..];
    }
    Ok((!blocks.is_empty()).then_some(blocks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pem_without_material_is_rejected() {
        let error = PgTls::from_pem("not a certificate", "not a key").unwrap_err();
        assert!(matches!(error, TlsError::Malformed(_)), "unexpected error: {error:?}");
        let error = PgTls::from_pem("-----BEGIN CERTIFICATE-----\nZm9v\n-----END CERTIFICATE-----\n", "no key").unwrap_err();
        assert_eq!(error, TlsError::Malformed("PEM text carries no private key block".to_string()));
    }

    #[test]
    fn a_malformed_chain_is_rejected_by_the_tls_stack() {
        let chain = "-----BEGIN CERTIFICATE-----\nZm9v\n-----END CERTIFICATE-----\n";
        let key = "-----BEGIN PRIVATE KEY-----\nZm9v\n-----END PRIVATE KEY-----\n";
        let error = PgTls::from_pem(chain, key).unwrap_err();
        assert!(matches!(error, TlsError::Rejected(_)), "unexpected error: {error:?}");
    }
}
