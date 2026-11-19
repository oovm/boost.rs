//! Live TLS 1.3 interoperability check against a public HTTPS endpoint.
//!
//! Ignored by default because it needs a route to the public internet. Set
//! `HTTPS_PROXY` when the environment only reaches the network through a proxy,
//! then run:
//!
//! `cargo test -p boost-net --test live_tls -- --ignored --nocapture`

use std::time::Duration;

use boost_net::http::{HttpTransport, HttpTransportConfig, Method, ProxyConfig, Request, Transport};

#[test]
#[ignore = "requires network access"]
fn lets_encrypt_staging_directory_completes_the_handshake() {
    let proxy = ProxyConfig::from_env().expect("proxy environment parses");
    let transport =
        HttpTransport::with_config(HttpTransportConfig { proxy, timeout: Duration::from_secs(20), ..HttpTransportConfig::default() });
    let url = boost_net::Url::parse("https://acme-staging-v02.api.letsencrypt.org/directory").expect("static URL");
    let response = transport.send(&Request::new(Method::Get, url)).expect("TLS handshake and GET");
    assert_eq!(response.status, 200);
    assert!(String::from_utf8_lossy(&response.body).contains("newNonce"), "ACME directory payload expected");
}
