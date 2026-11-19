//! WebSocket handshake, framing, fragmentation, and ping handling against a local peer.

use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

use boost_crypto::hash::sha1;
use boost_net::{
    NetError, Url,
    ws::{WebSocket, WsConfig, WsMessage},
};
use data_encoding::representation::Base64;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

fn read_head(stream: &mut impl Read) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.split("\r\n")
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(entry, _)| entry.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}

fn handshake_response(head: &str) -> String {
    let key = header(head, "sec-websocket-key").expect("client key");
    let accept = Base64::standard_strict().encode_text(sha1(format!("{key}{GUID}").as_bytes()));
    format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    )
}

fn read_client_frame(stream: &mut impl Read) -> (bool, u8, Vec<u8>) {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).unwrap();
    let fin = head[0] & 0x80 != 0;
    let opcode = head[0] & 0x0F;
    assert_ne!(head[1] & 0x80, 0, "client frames must be masked");
    let mut length = u64::from(head[1] & 0x7F);
    if length == 126 {
        let mut bytes = [0u8; 2];
        stream.read_exact(&mut bytes).unwrap();
        length = u64::from(u16::from_be_bytes(bytes));
    }
    else if length == 127 {
        let mut bytes = [0u8; 8];
        stream.read_exact(&mut bytes).unwrap();
        length = u64::from_be_bytes(bytes);
    }
    let mut mask = [0u8; 4];
    stream.read_exact(&mut mask).unwrap();
    let mut payload = vec![0u8; length as usize];
    stream.read_exact(&mut payload).unwrap();
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    (fin, opcode, payload)
}

fn server_frame(opcode: u8, fin: bool, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![if fin { 0x80 | opcode } else { opcode }];
    let length = payload.len();
    if length < 126 {
        frame.push(length as u8);
    }
    else if let Ok(length) = u16::try_from(length) {
        frame.push(126);
        frame.extend_from_slice(&length.to_be_bytes());
    }
    else {
        frame.push(127);
        frame.extend_from_slice(&(length as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    frame
}

fn masked_server_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mask = [0x11u8, 0x22, 0x33, 0x44];
    let mut frame = vec![0x80 | opcode, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(index, byte)| byte ^ mask[index % 4]));
    frame
}

fn local_url() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    (listener, format!("ws://{address}/chat"))
}

fn connect(url: &str) -> WebSocket {
    WebSocket::connect(&Url::parse(url).unwrap(), WsConfig::default()).unwrap()
}

#[test]
fn handshake_and_text_roundtrip() {
    let (listener, url) = local_url();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let head = read_head(&mut stream);
        stream.write_all(handshake_response(&head).as_bytes()).unwrap();
        let (fin, opcode, payload) = read_client_frame(&mut stream);
        assert!(fin);
        assert_eq!(opcode, 0x1);
        assert_eq!(String::from_utf8(payload).unwrap(), "hello");
        stream.write_all(&server_frame(0x1, true, b"world")).unwrap();
        stream.flush().unwrap();
    });

    let mut client = connect(&url);
    client.send(WsMessage::Text("hello".into())).unwrap();
    assert_eq!(client.recv().unwrap(), WsMessage::Text("world".into()));
    server.join().unwrap();
}

#[test]
fn reassembles_a_fragmented_message() {
    let (listener, url) = local_url();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let head = read_head(&mut stream);
        stream.write_all(handshake_response(&head).as_bytes()).unwrap();
        stream.write_all(&server_frame(0x1, false, b"hel")).unwrap();
        stream.write_all(&server_frame(0x0, true, b"lo")).unwrap();
        stream.flush().unwrap();
    });

    let mut client = connect(&url);
    assert_eq!(client.recv().unwrap(), WsMessage::Text("hello".into()));
    server.join().unwrap();
}

#[test]
fn answers_a_ping_before_returning_it() {
    let (listener, url) = local_url();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let head = read_head(&mut stream);
        stream.write_all(handshake_response(&head).as_bytes()).unwrap();
        stream.write_all(&server_frame(0x9, true, b"beat")).unwrap();
        stream.flush().unwrap();
        let (fin, opcode, payload) = read_client_frame(&mut stream);
        assert!(fin);
        assert_eq!(opcode, 0xA);
        assert_eq!(payload, b"beat");
        stream.write_all(&server_frame(0x1, true, b"after-pong")).unwrap();
        stream.flush().unwrap();
    });

    let mut client = connect(&url);
    assert_eq!(client.recv().unwrap(), WsMessage::Ping(b"beat".to_vec()));
    assert_eq!(client.recv().unwrap(), WsMessage::Text("after-pong".into()));
    server.join().unwrap();
}

#[test]
fn rejects_a_masked_server_frame() {
    let (listener, url) = local_url();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let head = read_head(&mut stream);
        stream.write_all(handshake_response(&head).as_bytes()).unwrap();
        stream.write_all(&masked_server_frame(0x1, b"bad")).unwrap();
        stream.flush().unwrap();
    });

    let mut client = connect(&url);
    assert!(matches!(client.recv(), Err(NetError::Protocol(_))));
    server.join().unwrap();
}

#[test]
fn close_frame_marks_the_session_closed() {
    let (listener, url) = local_url();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let head = read_head(&mut stream);
        stream.write_all(handshake_response(&head).as_bytes()).unwrap();
        let mut payload = 1000u16.to_be_bytes().to_vec();
        payload.extend_from_slice(b"bye");
        stream.write_all(&server_frame(0x8, true, &payload)).unwrap();
        stream.flush().unwrap();
    });

    let mut client = connect(&url);
    assert_eq!(client.recv().unwrap(), WsMessage::Close { code: Some(1000), reason: "bye".into() });
    assert!(matches!(client.recv(), Err(NetError::Closed)));
    server.join().unwrap();
}

#[test]
fn extra_headers_reach_the_server_and_reserved_names_are_dropped() {
    let (listener, url) = local_url();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let head = read_head(&mut stream);
        assert_eq!(header(&head, "user-agent"), Some("readaloud-test"));
        assert_eq!(header(&head, "cookie"), Some("muid=0;"));
        assert_eq!(head.to_ascii_lowercase().matches("host:").count(), 1, "the client owns the Host header");
        stream.write_all(handshake_response(&head).as_bytes()).unwrap();
    });

    let mut config = WsConfig::default();
    config.headers =
        vec![("User-Agent".into(), "readaloud-test".into()), ("Cookie".into(), "muid=0;".into()), ("Host".into(), "evil.invalid".into())];
    let mut client = WebSocket::connect(&Url::parse(&url).unwrap(), config).unwrap();
    let _ = client.close(1000, "done");
    server.join().unwrap();
}

#[test]
fn a_status_rejection_surfaces_the_status_and_date() {
    let (listener, url) = local_url();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _head = read_head(&mut stream);
        stream.write_all(b"HTTP/1.1 403 Forbidden\r\nDate: Fri, 20 Nov 2026 09:15:30 GMT\r\nContent-Length: 0\r\n\r\n").unwrap();
        stream.flush().unwrap();
    });

    let error = WebSocket::connect(&Url::parse(&url).unwrap(), WsConfig::default()).expect_err("403 is a rejection");
    match error {
        NetError::Rejected { status, date } => {
            assert_eq!(status, 403);
            assert_eq!(date.as_deref(), Some("Fri, 20 Nov 2026 09:15:30 GMT"));
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
    server.join().unwrap();
}
