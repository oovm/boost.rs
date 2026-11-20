//! TLS protocol version selection shared by HTTPS and WSS.
//!
//! The handshake itself lives in the `client`, `kx` and `record` submodules and
//! is compiled with the `tls` feature. Only TLS 1.3 is implemented today; a
//! configuration that excludes it is rejected instead of silently downgrading.

use alloc::{vec, vec::Vec};

use crate::NetError;

#[cfg(feature = "tls")]
mod client;
#[cfg(feature = "tls")]
mod error;
#[cfg(feature = "tls")]
mod kx;
#[cfg(feature = "tls")]
mod record;

#[cfg(feature = "tls")]
pub use client::{CertificatePolicy, ClientConfig, ClientHelloHook, EphemeralKey, HandshakeOptions, TlsClient};
#[cfg(feature = "tls")]
pub use error::{TlsError, TlsResult};

/// One TLS protocol version a connection may negotiate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TlsVersion {
    /// TLS 1.2.
    Tls12,
    /// TLS 1.3.
    Tls13,
}

impl TlsVersion {
    /// Stable label for diagnostics and configuration output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tls12 => "TLS1.2",
            Self::Tls13 => "TLS1.3",
        }
    }
}

impl core::fmt::Display for TlsVersion {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The protocol versions a TLS client may offer, in preference order.
///
/// An empty or duplicate-free list is enforced by [`TlsConfig::new`]; the default offers
/// TLS 1.2 and TLS 1.3 together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsConfig {
    versions: Vec<TlsVersion>,
}

impl TlsConfig {
    /// Build a configuration from the given versions, preserving order and dropping duplicates.
    ///
    /// Returns [`NetError::Unsupported`] when the iterator yields no version.
    pub fn new(versions: impl IntoIterator<Item = TlsVersion>) -> Result<Self, NetError> {
        let mut unique = Vec::new();
        for version in versions {
            if !unique.contains(&version) {
                unique.push(version);
            }
        }
        if unique.is_empty() {
            return Err(NetError::Unsupported("TLS configuration requires at least one protocol version".into()));
        }
        Ok(Self { versions: unique })
    }

    /// Offer every version this crate supports.
    pub fn all() -> Self {
        Self { versions: vec![TlsVersion::Tls12, TlsVersion::Tls13] }
    }

    /// Offer only the given version.
    pub fn only(version: TlsVersion) -> Self {
        Self { versions: vec![version] }
    }

    /// Return the configured versions in preference order.
    pub fn versions(&self) -> &[TlsVersion] {
        &self.versions
    }

    /// Return whether a version is enabled.
    pub fn contains(&self, version: TlsVersion) -> bool {
        self.versions.contains(&version)
    }

    /// The protocol version the built-in client will negotiate, if it supports one.
    ///
    /// The product stack implements TLS 1.3 only; TLS 1.2 is a known gap rather
    /// than a silent fallback.
    #[cfg(feature = "tls")]
    pub(crate) fn client_version(&self) -> Result<TlsVersion, NetError> {
        if self.contains(TlsVersion::Tls13) {
            Ok(TlsVersion::Tls13)
        }
        else {
            Err(NetError::Unsupported("the built-in TLS client implements TLS 1.3 only".into()))
        }
    }
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self::all()
    }
}
