//! The TLS negotiation contract of the protocol-v3 gateway.
//!
//! A configured certificate is covered by `PgTls`'s own tests; these cases pin the
//! `SSLRequest` answer a listener without one must give, because that answer is
//! what decides whether a stock client falls back to plaintext.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
};

use database_server::yyds::pgsql::{PgAuthentication, PgTls, connection::serve_connection_with_auth_and_tls};

const SSL_REQUEST_CODE: [u8; 4] = [0x04, 0xD2, 0x16, 0x2F];

#[test]
fn an_ssl_request_is_declined_when_no_certificate_is_configured() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let _ = serve_connection_with_auth_and_tls(stream, None, PgAuthentication::Trust, None);
    });

    let mut client = TcpStream::connect(address).unwrap();
    let mut ssl_request = vec![0, 0, 0, 8];
    ssl_request.extend_from_slice(&SSL_REQUEST_CODE);
    client.write_all(&ssl_request).unwrap();
    let mut reply = [0u8; 1];
    client.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"N", "a listener without a certificate must decline TLS");

    // The same connection then runs a plaintext protocol-v3 startup.
    let mut startup = vec![0, 0, 0, 0, 0, 3, 0, 0];
    startup.extend_from_slice(b"user\0postgres\0\0");
    let length = (startup.len() as u32).to_be_bytes();
    startup[..4].copy_from_slice(&length);
    client.write_all(&startup).unwrap();
    let mut tag = [0u8; 1];
    client.read_exact(&mut tag).unwrap();
    assert_eq!(tag[0], b'R', "the startup must reach AuthenticationOk");
    client.write_all(&[b'X', 0, 0, 0, 4]).unwrap();
    server.join().unwrap();
}

#[test]
fn tls_configuration_rejects_material_it_cannot_use() {
    assert!(matches!(
        PgTls::from_pem("-----BEGIN CERTIFICATE-----\nZm9v\n-----END CERTIFICATE-----\n", "no key"),
        Err(database_server::yyds::pgsql::TlsError::Malformed(_))
    ));
    assert!(matches!(
        PgTls::from_pem(
            "-----BEGIN CERTIFICATE-----\nZm9v\n-----END CERTIFICATE-----\n",
            "-----BEGIN PRIVATE KEY-----\nZm9v\n-----END PRIVATE KEY-----\n"
        ),
        Err(database_server::yyds::pgsql::TlsError::Rejected(_))
    ));
}
