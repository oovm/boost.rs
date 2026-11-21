//! peer 通道的可选 mTLS 配置。
//!
//! 证书与私钥由宿主以 PEM 提供;节点之间使用同一个受信 CA。启用 TLS 后监听端
//! 要求客户端证书,连接端校验服务端证书与主机名;任何一侧校验失败都在传输层
//! 直接断开,不会退回明文。

use std::{
    fmt,
    io,
    net::SocketAddr,
    path::Path,
    sync::{Arc, Once},
};

use data_encoding::representation::Base64;
use rustls::{
    pki_types::{CertificateDer, IpAddr, PrivateKeyDer, ServerName},
    server::WebPkiClientVerifier,
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection,
};

/// TLS 材料或配置失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerTlsError {
    /// PEM 文本里没有可用块。
    Malformed(String),
    /// `rustls` 拒绝证书或私钥。
    Rejected(String),
    /// 连接目标地址不能作为 TLS 服务端名。
    InvalidServerName(String),
}

impl fmt::Display for PeerTlsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(detail) => write!(formatter, "peer TLS PEM is malformed: {detail}"),
            Self::Rejected(detail) => write!(formatter, "peer TLS material was rejected: {detail}"),
            Self::InvalidServerName(detail) => write!(formatter, "peer TLS server name is invalid: {detail}"),
        }
    }
}

impl std::error::Error for PeerTlsError {}

impl PeerTlsError {
    /// 稳定诊断码。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "database.cluster.peer.tls.malformed",
            Self::Rejected(_) => "database.cluster.peer.tls.rejected",
            Self::InvalidServerName(_) => "database.cluster.peer.tls.server_name",
        }
    }
}

/// 一套 peer mTLS 配置:本节点证书/私钥 + 受信 CA。
#[derive(Debug, Clone)]
pub struct PeerTls {
    server: Arc<ServerConfig>,
    client: Arc<ClientConfig>,
}

impl PeerTls {
    /// 从 PEM 文本构造监听端与连接端配置。
    ///
    /// `cert_pem` 是本节点证书链,`key_pem` 是与之匹配的私钥,`ca_pem` 是受信 CA。
    pub fn from_pem(cert_pem: &str, key_pem: &str, ca_pem: &str) -> Result<Self, PeerTlsError> {
        install_crypto_provider();
        let chain = read_certificates(cert_pem)?;
        if chain.is_empty() {
            return Err(PeerTlsError::Malformed("PEM text carries no `CERTIFICATE` block".to_owned()));
        }
        let key = read_private_key(key_pem)?;
        let verifier = WebPkiClientVerifier::builder(Arc::new(read_roots(ca_pem)?))
            .build()
            .map_err(|error| PeerTlsError::Rejected(error.to_string()))?;
        let server = ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(chain.clone(), key.clone_key())
            .map_err(|error| PeerTlsError::Rejected(error.to_string()))?;
        let client = ClientConfig::builder()
            .with_root_certificates(read_roots(ca_pem)?)
            .with_client_auth_cert(chain, key)
            .map_err(|error| PeerTlsError::Rejected(error.to_string()))?;
        Ok(Self { server: Arc::new(server), client: Arc::new(client) })
    }

    /// 从文件读取 PEM 材料。
    pub fn from_files(cert: impl AsRef<Path>, key: impl AsRef<Path>, ca: impl AsRef<Path>) -> Result<Self, PeerTlsError> {
        let cert = std::fs::read_to_string(cert).map_err(|error| PeerTlsError::Malformed(error.to_string()))?;
        let key = std::fs::read_to_string(key).map_err(|error| PeerTlsError::Malformed(error.to_string()))?;
        let ca = std::fs::read_to_string(ca).map_err(|error| PeerTlsError::Malformed(error.to_string()))?;
        Self::from_pem(&cert, &key, &ca)
    }

    /// 监听端握手对象。
    pub(crate) fn server_connection(&self) -> io::Result<ServerConnection> {
        ServerConnection::new(Arc::clone(&self.server))
            .map_err(|error| io::Error::other(format!("peer TLS server connection failed: {error}")))
    }

    /// 连接端握手对象;服务端名由目标地址推导。
    pub(crate) fn client_connection(&self, address: SocketAddr) -> Result<ClientConnection, PeerTlsError> {
        let name = server_name(address)?;
        ClientConnection::new(Arc::clone(&self.client), name)
            .map_err(|error| PeerTlsError::Rejected(error.to_string()))
    }
}

fn install_crypto_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn server_name(address: SocketAddr) -> Result<ServerName<'static>, PeerTlsError> {
    if address.ip().is_loopback() {
        return ServerName::try_from("localhost")
            .map_err(|error| PeerTlsError::InvalidServerName(error.to_string()));
    }
    let ip: IpAddr = address.ip().into();
    Ok(ServerName::IpAddress(ip))
}

fn read_certificates(pem: &str) -> Result<Vec<CertificateDer<'static>>, PeerTlsError> {
    Ok(read_pem_blocks(pem, "CERTIFICATE")?.unwrap_or_default().into_iter().map(CertificateDer::from).collect())
}

fn read_roots(pem: &str) -> Result<RootCertStore, PeerTlsError> {
    let mut roots = RootCertStore::empty();
    let certificates = read_pem_blocks(pem, "CERTIFICATE")?;
    let Some(certificates) = certificates
    else {
        return Err(PeerTlsError::Malformed("PEM text carries no `CERTIFICATE` block".to_owned()));
    };
    for certificate in certificates {
        roots.add(CertificateDer::from(certificate)).map_err(|error| PeerTlsError::Rejected(error.to_string()))?;
    }
    Ok(roots)
}

fn read_private_key(pem: &str) -> Result<PrivateKeyDer<'static>, PeerTlsError> {
    if let Some(block) = read_pem_blocks(pem, "PRIVATE KEY")?.and_then(|mut blocks| blocks.drain(..).next()) {
        return Ok(PrivateKeyDer::Pkcs8(block.into()));
    }
    if let Some(block) = read_pem_blocks(pem, "RSA PRIVATE KEY")?.and_then(|mut blocks| blocks.drain(..).next()) {
        return Ok(PrivateKeyDer::Pkcs1(block.into()));
    }
    if let Some(block) = read_pem_blocks(pem, "EC PRIVATE KEY")?.and_then(|mut blocks| blocks.drain(..).next()) {
        return Ok(PrivateKeyDer::Sec1(block.into()));
    }
    Err(PeerTlsError::Malformed("PEM text carries no private key block".to_owned()))
}

fn read_pem_blocks(pem: &str, tag: &str) -> Result<Option<Vec<Vec<u8>>>, PeerTlsError> {
    let begin = format!("-----BEGIN {tag}-----");
    let end = format!("-----END {tag}-----");
    let mut blocks = Vec::new();
    let mut rest = pem;
    while let Some(start) = rest.find(&begin) {
        let body_start = start + begin.len();
        let Some(close) = rest[body_start..].find(&end)
        else {
            return Err(PeerTlsError::Malformed(format!("unterminated `{tag}` PEM block")));
        };
        let body = &rest[body_start..body_start + close];
        let decoded = Base64::standard_strict()
            .decode_canonical(body.split_whitespace().collect::<String>().as_bytes())
            .map_err(|error| PeerTlsError::Malformed(format!("invalid base64 in `{tag}` PEM block: {error}")))?;
        blocks.push(decoded);
        rest = &rest[body_start + close + end.len()..];
    }
    Ok((!blocks.is_empty()).then_some(blocks))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CERT: &str = include_str!("../../tests/fixtures/peer-tls/node.pem");
    const KEY: &str = include_str!("../../tests/fixtures/peer-tls/node.key");
    const CA: &str = include_str!("../../tests/fixtures/peer-tls/ca.pem");

    #[test]
    fn valid_material_builds_both_directions() {
        PeerTls::from_pem(CERT, KEY, CA).expect("config");
    }

    #[test]
    fn a_lone_certificate_without_a_key_is_refused() {
        let error = PeerTls::from_pem(CERT, "not a key", CA).expect_err("missing key");
        assert_eq!(error.code(), "database.cluster.peer.tls.malformed");
    }

    #[test]
    fn an_invalid_certificate_body_is_rejected_by_rustls() {
        let error = PeerTls::from_pem(
            "-----BEGIN CERTIFICATE-----\nZm9v\n-----END CERTIFICATE-----\n",
            KEY,
            CA,
        )
        .expect_err("bad certificate");
        assert_eq!(error.code(), "database.cluster.peer.tls.rejected");
    }

    #[test]
    fn a_loopback_target_uses_the_localhost_server_name() {
        let name = server_name("127.0.0.1:4101".parse().expect("address")).expect("name");
        assert_eq!(name, ServerName::try_from("localhost").expect("localhost"));
    }
}
