//! TLS protocol version selection shared by HTTPS and WSS.

use crate::NetError;

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

impl std::fmt::Display for TlsVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

    /// Map the configuration onto the versions understood by the TLS backend.
    #[cfg(feature = "tls")]
    pub(crate) fn protocol_versions(&self) -> Vec<&'static rustls::SupportedProtocolVersion> {
        self.versions
            .iter()
            .map(|version| match version {
                TlsVersion::Tls12 => &rustls::version::TLS12,
                TlsVersion::Tls13 => &rustls::version::TLS13,
            })
            .collect()
    }
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self::all()
    }
}
