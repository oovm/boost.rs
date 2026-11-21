//! TCP and TLS connection plumbing shared by the HTTP and WebSocket protocols.

use std::{
    io::{self, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::{Duration, Instant},
};

#[cfg(feature = "tls")]
use std::sync::OnceLock;

#[cfg(feature = "tls")]
use crate::{
    pki::{TrustAnchor, lets_encrypt_anchors},
    tls::{ClientConfig, TlsClient},
};

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

    /// 克隆一个可跨线程关闭该连接的句柄。
    pub(crate) fn canceller(&self) -> io::Result<CancelHandle> {
        Ok(CancelHandle { stream: self.stream.try_clone()? })
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
    Tls(Box<TlsClient<S>>),
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
            Self::Tls(stream) => stream.socket_mut().use_idle(timeout),
        }
    }

    /// 返回可跨线程取消的句柄。
    pub(crate) fn canceller(&self) -> io::Result<CancelHandle> {
        match self {
            Self::Tcp(socket) => socket.canceller(),
            #[cfg(feature = "tls")]
            Self::Tls(stream) => stream.socket().canceller(),
        }
    }
}

/// 从任意线程关闭一条流式连接。
///
/// 阻塞在 `read` 上的线程会立刻收到错误而退出，不必等空闲超时。
#[derive(Debug)]
pub(crate) struct CancelHandle {
    stream: TcpStream,
}

impl CancelHandle {
    pub(crate) fn cancel(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
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
                // The built-in client implements TLS 1.3 only; a configuration
                // that rules it out is an explicit unsupported result.
                tls.client_version()?;
                if host.parse::<std::net::IpAddr>().is_ok() {
                    return Err(NetError::Unsupported("the built-in TLS client verifies DNS names only".into()));
                }
                let config = ClientConfig { hostname: host, anchors: trust_anchors(), timeout: Duration::from_secs(30) };
                let client = TlsClient::handshake(&config, stream).map_err(|error| NetError::Transport(error.to_string()))?;
                Ok(Connection::Tls(Box::new(client)))
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

/// The pinned Let's Encrypt anchors plus the platform store, parsed once per process.
///
/// The pinned set alone rejects every public host behind another CA, so the
/// operating system's roots are merged in; a duplicate subject is skipped
/// because the pinned copy is already trusted.
#[cfg(feature = "tls")]
fn trust_anchors() -> &'static [TrustAnchor] {
    static ANCHORS: OnceLock<Vec<TrustAnchor>> = OnceLock::new();
    ANCHORS.get_or_init(|| {
        let mut anchors = lets_encrypt_anchors();
        for anchor in crate::pki::system_anchors() {
            if !anchors.iter().any(|existing| existing.subject_raw == anchor.subject_raw) {
                anchors.push(anchor);
            }
        }
        anchors
    })
}
