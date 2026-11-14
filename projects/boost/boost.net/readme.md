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

See `src/readme.md` for the transport boundary and the policies this crate does not own.
