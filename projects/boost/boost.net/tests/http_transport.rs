//! Response parsing, redirect handling, and budget limits of the HTTP transport.

use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

use boost_net::http::{HttpTransport, HttpTransportConfig, Method, NetError, Request, Transport};
use url::Url;

/// Serves exactly one response over a local TCP connection and returns its URL.
fn serve(response: &'static str) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        stream.write_all(response.as_bytes()).unwrap();
    });
    Url::parse(&format!("http://{address}/resource?part=1")).unwrap()
}

#[test]
fn parses_content_length_and_non_success_status() {
    let url = serve("HTTP/1.1 418 I'm a teapot\r\nContent-Length: 3\r\n\r\ntea");
    let response = HttpTransport::new().send(&Request::new(Method::Get, url)).unwrap();
    assert_eq!(response.status, 418);
    assert_eq!(response.body, b"tea");
    assert!(response.is_error());
}

#[test]
fn decodes_chunked_body() {
    let url = serve("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n");
    let response = HttpTransport::new().send(&Request::new(Method::Get, url)).unwrap();
    assert_eq!(response.body, b"hello");
}

#[test]
fn keeps_repeated_response_headers() {
    let url = serve("HTTP/1.1 200 OK\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nContent-Length: 0\r\n\r\n");
    let response = HttpTransport::new().send(&Request::new(Method::Get, url)).unwrap();
    assert_eq!(response.headers.get_all("set-cookie").iter().map(String::as_str).collect::<Vec<_>>(), ["a=1", "b=2"]);
}

#[test]
#[ignore = "requires outbound HTTPS access"]
fn fetches_https_resource_with_webpki_roots() {
    let url = Url::parse("https://example.com/").unwrap();
    let response = HttpTransport::new().send(&Request::new(Method::Get, url)).unwrap();
    assert_eq!(response.status, 200);
    assert!(!response.body.is_empty());
}

#[test]
fn rejects_response_header_budget() {
    let url = serve("HTTP/1.1 200 OK\r\nX-Large: value\r\nContent-Length: 0\r\n\r\n");
    let transport = HttpTransport::with_config(HttpTransportConfig { max_header_bytes: 10, ..HttpTransportConfig::default() });
    assert!(matches!(transport.send(&Request::new(Method::Get, url)), Err(NetError::HeaderTooLarge { limit: 10 })));
}

/// Serves one request on a fake proxy and reports the received request head.
fn serve_proxy(response: &'static str) -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            if stream.read_exact(&mut byte).is_err() {
                break;
            }
            request.push(byte[0]);
        }
        let _ = sender.send(String::from_utf8_lossy(&request).into_owned());
        let _ = stream.write_all(response.as_bytes());
    });
    (address, receiver)
}

#[test]
fn routes_plain_http_through_proxy_with_absolute_form() {
    let (address, received) = serve_proxy("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nproxy");
    let proxy = boost_net::http::ProxyConfig::parse(&format!("http://user:secret@{address}")).unwrap();
    let transport = HttpTransport::with_config(HttpTransportConfig { proxy: Some(proxy), ..HttpTransportConfig::default() });
    let url = Url::parse("http://proxied.example/resource?part=1").unwrap();

    let response = transport.send(&Request::new(Method::Get, url)).unwrap();
    assert_eq!(response.body, b"proxy");

    let head = received.recv().unwrap();
    assert!(head.starts_with("GET http://proxied.example/resource?part=1 HTTP/1.1\r\n"), "{head}");
    // `user:secret` 的 Basic 凭据。
    assert!(head.contains("Proxy-Authorization: Basic dXNlcjpzZWNyZXQ=\r\n"), "{head}");
}

#[test]
fn no_proxy_entries_skip_the_proxy() {
    let proxy = boost_net::http::ProxyConfig::parse("http://127.0.0.1:1")
        .unwrap()
        .with_no_proxy(["localhost".to_string(), ".internal.test".to_string()]);
    assert!(proxy.bypasses("localhost"));
    assert!(proxy.bypasses("build.internal.test"));
    assert!(proxy.bypasses("internal.test"));
    assert!(!proxy.bypasses("registry.npmjs.org"));
}

#[test]
fn https_targets_open_a_connect_tunnel() {
    let (address, received) = serve_proxy("HTTP/1.1 200 Connection Established\r\n\r\n");
    let proxy = boost_net::http::ProxyConfig::parse(&format!("http://{address}")).unwrap();
    let transport = HttpTransport::with_config(HttpTransportConfig { proxy: Some(proxy), ..HttpTransportConfig::default() });
    let url = Url::parse("https://secure.example/api").unwrap();

    // 假代理在 200 之后立即关闭连接，TLS 握手必然失败；这里只验证隧道请求本身。
    assert!(transport.send(&Request::new(Method::Get, url)).is_err());
    let head = received.recv().unwrap();
    assert!(head.starts_with("CONNECT secure.example:443 HTTP/1.1\r\n"), "{head}");
}

#[test]
fn rejects_proxy_entries_without_host_port_or_with_path() {
    assert!(boost_net::http::ProxyConfig::parse("127.0.0.1:7897").is_err());
    assert!(boost_net::http::ProxyConfig::parse("http://127.0.0.1").is_err());
    assert!(boost_net::http::ProxyConfig::parse("http://127.0.0.1:7897/proxy").is_err());
    assert!(boost_net::http::ProxyConfig::parse("socks5://127.0.0.1:7897").is_err());
}
