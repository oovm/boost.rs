//! TCP and TLS connection plumbing shared by the HTTP and WebSocket protocols.

use std::{
    io::{self, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::{Duration, Instant},
};

#[cfg(feature = "tls")]
use std::sync::Arc;

#[cfg(feature = "tls")]
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned, pki_types::ServerName};

use crate::{NetError, tls::TlsConfig};

/// Timeout policy of one request socket.
enum SocketTimeout {
    /// 一个绝对截止时间，随每次读写递减；过期即失败。
    Deadline(Instant),
    /// 每次读写独立计时，零值表示阻塞。
    Idle(Duration),
}

/// Applies the request timeout policy to every read and write.
pub(crate) struct DeadlineSocket {
    stream: TcpStream,
    timeout: SocketTimeout,
}

impl DeadlineSocket {
    pub(crate) fn new(stream: TcpStream, deadline: Instant) -> Self {
        Self { stream, timeout: SocketTimeout::Deadline(deadline) }
    }

    /// 把请求级截止时间换成流式响应的空闲超时。
    ///
    /// 流式响应没有整体完成时间，只剩“多久没有新字节”这一约束。
    pub(crate) fn use_idle(&mut self, timeout: Duration) {
        self.timeout = SocketTimeout::Idle(timeout);
    }

    fn apply(&self) -> io::Result<Option<Duration>> {
        match self.timeout {
            SocketTimeout::Deadline(deadline) => deadline
                .checked_duration_since(Instant::now())
                .filter(|duration| !duration.is_zero())
                .map(Some)
                .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "request deadline exceeded")),
            SocketTimeout::Idle(timeout) if timeout.is_zero() => Ok(None),
            SocketTimeout::Idle(timeout) => Ok(Some(timeout)),
        }
    }
}

impl Read for DeadlineSocket {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(self.apply()?)?;
        self.stream.read(buffer)
    }
}

impl Write for DeadlineSocket {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(self.apply()?)?;
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.set_write_timeout(self.apply()?)?;
        self.stream.flush()
    }
}

/// Applies one fixed per-operation timeout to every read and write.
///
/// A zero timeout leaves the socket blocking.
pub(crate) struct TimedSocket {
    stream: TcpStream,
    timeout: Duration,
}

impl TimedSocket {
    pub(crate) fn new(stream: TcpStream, timeout: Duration) -> Self {
        Self { stream, timeout }
    }

    fn apply(&self) -> io::Result<()> {
        let timeout = if self.timeout.is_zero() { None } else { Some(self.timeout) };
        self.stream.set_read_timeout(timeout)?;
        self.stream.set_write_timeout(timeout)
    }
}

impl Read for TimedSocket {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.apply()?;
        self.stream.read(buffer)
    }
}

impl Write for TimedSocket {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.apply()?;
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.apply()?;
        self.stream.flush()
    }
}

/// A plaintext or TLS connection over one socket.
pub(crate) enum Connection<S: Read + Write> {
    Tcp(S),
    #[cfg(feature = "tls")]
    Tls(Box<StreamOwned<ClientConnection, S>>),
}

impl<S: Read + Write> Read for Connection<S> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(bytes),
            #[cfg(feature = "tls")]
            Self::Tls(stream) => stream.read(bytes),
        }
    }
}

impl<S: Read + Write> Write for Connection<S> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write(bytes),
            #[cfg(feature = "tls")]
            Self::Tls(stream) => stream.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.flush(),
            #[cfg(feature = "tls")]
            Self::Tls(stream) => stream.flush(),
        }
    }
}

impl Connection<DeadlineSocket> {
    /// 响应头之后切换为流式读取：每次读写使用独立的空闲超时。
    pub(crate) fn use_idle(&mut self, timeout: Duration) {
        match self {
            Self::Tcp(socket) => socket.use_idle(timeout),
            #[cfg(feature = "tls")]
            Self::Tls(stream) => stream.sock.use_idle(timeout),
        }
    }
}

/// Resolve a host and connect one address before `deadline`.
pub(crate) fn dial(host: &str, port: u16, deadline: Instant) -> Result<TcpStream, NetError> {
    let addresses = (host, port).to_socket_addrs().map_err(|error| NetError::Transport(error.to_string()))?.collect::<Vec<_>>();
    let mut last_error = "host resolved to no addresses".to_string();
    for address in addresses {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| NetError::Transport("request deadline exceeded".into()))?;
        match TcpStream::connect_timeout(&address, remaining) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(NetError::Transport(last_error))
}

/// Wrap a connected socket with TLS when the scheme requires it.
///
/// `http` and `ws` stay plaintext. `https` and `wss` fail with a structured error when the
/// `tls` feature is off instead of silently downgrading.
pub(crate) fn secure<S: Read + Write>(scheme: &str, host: &str, stream: S, tls: &TlsConfig) -> Result<Connection<S>, NetError> {
    match scheme {
        "http" | "ws" => Ok(Connection::Tcp(stream)),
        "https" | "wss" => {
            #[cfg(feature = "tls")]
            {
                let mut roots = RootCertStore::empty();
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                let versions = tls.protocol_versions();
                let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                    .with_protocol_versions(&versions)
                    .map_err(|error| NetError::Transport(format!("unsupported TLS version selection: {error}")))?
                    .with_root_certificates(roots)
                    .with_no_client_auth();
                let name = ServerName::try_from(host.to_string()).map_err(|error| NetError::Unsupported(error.to_string()))?;
                let connection = ClientConnection::new(Arc::new(config), name).map_err(|error| NetError::Transport(error.to_string()))?;
                Ok(Connection::Tls(Box::new(StreamOwned::new(connection, stream))))
            }
            #[cfg(not(feature = "tls"))]
            {
                let _ = (host, stream, tls);
                Err(NetError::Unsupported("secure schemes require the `tls` feature".into()))
            }
        }
        _ => Err(NetError::Unsupported(format!("unsupported URL scheme `{scheme}`"))),
    }
}
