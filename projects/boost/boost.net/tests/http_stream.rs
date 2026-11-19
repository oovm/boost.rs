//! 流式响应合同：framing、预算、超时、断线与取消。

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
    time::Duration,
};

use boost_net::http::{HttpTransport, HttpTransportConfig, Method, NetError, Request};
use url::Url;

/// 读取一个请求头，直到空行。
fn read_request(stream: &mut TcpStream) {
    let mut request = Vec::new();
    let mut byte = [0u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        if stream.read_exact(&mut byte).is_err() {
            return;
        }
        request.push(byte[0]);
    }
}

/// 只服务一次流式响应。
///
/// `linger` 为真时服务器在写完数据后继续持有连接，直到客户端关闭（返回读到的字节数，
/// 关闭为 `0`，超时为 `-1`）；为假时写完立即断开。
fn serve(head: &'static str, chunks: &'static [&'static str], linger: bool) -> (Url, mpsc::Receiver<i32>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_request(&mut stream);
        stream.write_all(head.as_bytes()).unwrap();
        for chunk in chunks {
            let _ = stream.write_all(chunk.as_bytes());
            let _ = stream.flush();
        }
        let observed = if linger {
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut buffer = [0u8; 1];
            stream.read(&mut buffer).map(|read| read as i32).unwrap_or(-1)
        }
        else {
            -2
        };
        let _ = sender.send(observed);
    });
    (Url::parse(&format!("http://{address}/stream")).unwrap(), receiver)
}

#[test]
fn streams_content_length_incrementally() {
    let (url, _observed) = serve("HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n", &["hello", "world"], true);
    let mut response = HttpTransport::new().send_stream(&Request::new(Method::Get, url)).unwrap();
    assert_eq!(response.status, 200);
    assert!(response.is_success());

    let mut first = [0u8; 5];
    response.body.read_exact(&mut first).unwrap();
    assert_eq!(&first, b"hello");

    let mut rest = Vec::new();
    response.body.read_to_end(&mut rest).unwrap();
    assert_eq!(rest, b"world");
}

#[test]
fn streams_chunked_body_without_waiting_for_close() {
    let (url, _observed) =
        serve("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n", &["5\r\nhello\r\n", "6\r\n world\r\n", "0\r\n\r\n"], true);
    let mut response = HttpTransport::new().send_stream(&Request::new(Method::Get, url)).unwrap();

    let mut first = [0u8; 4];
    response.body.read_exact(&mut first).unwrap();
    assert_eq!(&first, b"hell");

    let mut rest = Vec::new();
    response.body.read_to_end(&mut rest).unwrap();
    assert_eq!(rest, b"o world");
}

#[test]
fn streams_until_connection_close() {
    let (url, _observed) = serve("HTTP/1.1 200 OK\r\n\r\n", &["par", "tial"], false);
    let mut response = HttpTransport::new().send_stream(&Request::new(Method::Get, url)).unwrap();
    let mut body = Vec::new();
    response.body.read_to_end(&mut body).unwrap();
    assert_eq!(body, b"partial");
}

#[test]
fn rejects_stream_body_over_budget() {
    let (url, _observed) = serve("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n", &[], true);
    let transport = HttpTransport::with_config(HttpTransportConfig { max_body_bytes: 8, ..HttpTransportConfig::default() });
    let error = transport.send_stream(&Request::new(Method::Get, url)).unwrap_err();
    assert!(matches!(error, NetError::BodyTooLarge { limit: 8 }), "{error}");
}

#[test]
fn reports_disconnect_before_the_declared_length() {
    let (url, _observed) = serve("HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n", &["abc"], false);
    let mut response = HttpTransport::new().send_stream(&Request::new(Method::Get, url)).unwrap();
    let mut body = Vec::new();
    let error = response.body.read_to_end(&mut body).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof, "{error}");
    assert_eq!(body, b"abc");
}

#[test]
fn dropping_the_stream_closes_the_connection() {
    let (url, observed) = serve("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n", &["3\r\nabc"], true);
    let mut response = HttpTransport::new().send_stream(&Request::new(Method::Get, url)).unwrap();
    let mut first = [0u8; 3];
    response.body.read_exact(&mut first).unwrap();
    assert_eq!(&first, b"abc");

    drop(response);
    assert_eq!(observed.recv_timeout(Duration::from_secs(5)).unwrap(), 0);
}

#[test]
fn idle_body_reads_time_out() {
    let (url, _observed) = serve("HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n", &[], true);
    let transport =
        HttpTransport::with_config(HttpTransportConfig { stream_idle_timeout: Duration::from_millis(150), ..HttpTransportConfig::default() });
    let mut response = transport.send_stream(&Request::new(Method::Get, url)).unwrap();
    let mut buffer = [0u8; 1];
    assert!(response.body.read(&mut buffer).is_err());
}

#[test]
fn cancel_handle_unblocks_a_pending_read() {
    let (url, _observed) = serve("HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n", &[], true);
    let mut response = HttpTransport::new().send_stream(&Request::new(Method::Get, url)).unwrap();
    let cancel = response.body.cancel_handle().unwrap();

    let reader = thread::spawn(move || {
        let mut buffer = [0u8; 4];
        response.body.read(&mut buffer).is_err()
    });
    thread::sleep(Duration::from_millis(100));
    cancel.cancel();

    // 没有 cancel 时这里要等满 300s 的空闲超时。
    assert!(reader.join().unwrap());
}

#[test]
fn follows_redirects_before_streaming() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        read_request(&mut first);
        let location = format!("http://{address}/final");
        let _ = first.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n").as_bytes());
        drop(first);

        let (mut second, _) = listener.accept().unwrap();
        read_request(&mut second);
        let _ = second.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    });

    let url = Url::parse(&format!("http://{address}/start")).unwrap();
    let mut response = HttpTransport::new().send_stream(&Request::new(Method::Get, url)).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.url.path(), "/final");
    assert_eq!(response.redirects.len(), 1);
    assert_eq!(response.redirects[0].status, 302);

    let mut body = Vec::new();
    response.body.read_to_end(&mut body).unwrap();
    assert_eq!(body, b"ok");
}
