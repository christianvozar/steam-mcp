# 0002. Build almost everything in-tree; delegate only TLS and asymmetric crypto

- Status: accepted
- Date: 2026-11-21

## Context

The project goal is a production server with as few external dependencies as possible, preferring hand-built implementations. The Steam Web API is HTTPS-only. Some JWT validation needs RSA and ECDSA signature verification.

## Decision

The following are implemented in-tree:

- JSON (order-preserving, exact 64-bit integers for SteamIDs)
- an HTTP/1.1 client and server, including chunked framing, keep-alive pooling and HTTP CONNECT proxying
- SSE
- SHA-256, HMAC-SHA256 and constant-time comparison
- base64 and percent-encoding
- RFC 6570 URI templates
- a bounded JSON Schema validator
- token-bucket rate limiting
- the cache, logging, and RFC 3339 time handling

Only these are delegated to audited crates:

- **`rustls` + `webpki-roots`** (with the `ring` provider): TLS, both client and optional server. The project owner approved this explicitly; a hand-rolled TLS and X.509 stack would be unsafe.
- **`ring`**: RS256 and ES256 JWT verification. `rustls` already depends on it, so it adds no new crates to the tree.

The total dependency tree is 15 crates. Our own SHA-256 and HMAC are tested against FIPS 180-4 and RFC 4231 vectors.

## Consequences

- We own correctness of the HTTP parsing and JSON. Mitigations are strict parsers with size and depth limits, and tests.
- No async runtime: see ADR 0005.
- Upgrading TLS is a `cargo update`, not a rewrite.
