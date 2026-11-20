# `boost_net`

Protocol-layer network primitives for the rust-boost workspace.

## Modules

- `http` — request and response data, case-insensitive headers, bounded bodies, redirect policy,
  and the replaceable `Transport` boundary. The built-in `HttpTransport` owns synchronous HTTP/1.1
  framing.
- `ws` — synchronous WebSocket client: opening handshake, masked client frames, fragmentation
  reassembly, control frames, and ping/pong. Pings are answered with a pong before being returned.
- `tls` — `TlsConfig` / `TlsVersion` selection shared by HTTPS and WSS. A secure URL either
  negotiates one of the configured versions or fails with a structured error; it never falls back
  to plaintext.
- `acme` — RFC 8555 protocol core: directory, account, order, authorization and challenge
  documents, ES256 JWS signing over an injected `Transport`, `dns-01` / `http-01` challenge
  material, and `Retry-After`-aware polling. Account key storage, CSR generation, DNS provider
  credentials, certificate delivery and renewal schedules stay with the consuming product.

`NetError` is the single error surface for every protocol in the crate.

## Boundaries

`boost_net` does not own crawling, robots policy, response caching, format detection, document
parsing, connection pooling, proxies, authentication policy, or an asynchronous runtime. It does not
interpret response bodies beyond framing limits, and it does not apply HTTP content-coding
decompression or character-set conversion.

TLS roots come from the bundled `webpki-roots` set; client certificates and custom root stores are
outside this layer. WebSocket extensions such as `permessage-deflate` are not negotiated.
