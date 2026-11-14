# `boost-net`

Protocol-layer network primitives for the rust-boost workspace: HTTP/1.1, WebSocket, and the TLS
version policy they share.

| Module | Owns |
|--------|------|
| `boost_net::http` | `Request` / `Response` / `Headers`, redirects, bounded bodies, and the HTTP/1.1 `Transport` |
| `boost_net::ws` | WebSocket opening handshake, frame codec, fragmentation, and ping/pong |
| `boost_net::tls` | TLS protocol version selection (`TLS1.2` / `TLS1.3`) for HTTPS and WSS |

TLS is a capability of the same protocol contract, not a separate `boost-https` or `boost-wss`
crate: a secure URL either negotiates one of the configured versions or fails with a structured
[`NetError`](src/error.rs), never downgrading to plaintext.

## Features

| Feature | What you get |
|---------|----------------|
| `std` (default) | TCP transports, WebSocket sessions, and URL parsing with `std` |
| `tls` (default) | rustls-backed HTTPS/WSS honoring `TlsConfig` (implies `std`) |

Without `std` the crate is `no_std` + `alloc` and exposes the protocol models and TLS version
policy only. The HTTP transport, WebSocket session, and TLS handshake require `std`, because they
need host sockets and a DNS resolver.

See `src/readme.md` for the transport boundary and the policies this crate does not own.
