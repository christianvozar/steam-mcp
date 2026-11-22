# 0013. Harden the transport and parsers: read deadlines, hangup detection, canonical input

- Status: accepted (amends the disconnect-detection consequence of ADR 0005)
- Date: 2026-11-21

## Context

A gap review and seeded fuzzing (`tests/fuzz.rs`) found four weaknesses:

1. Per-read socket timeouts let a client drip one byte at a time and hold a connection open indefinitely.
2. Disconnect detection used `peek`. Over TLS, a client's close_notify alert sits unread in the socket, so `peek` never sees the FIN that follows. Closing a TLS connection therefore did not cancel its request.
3. Sealed `requestState` and JWT segments were decoded with a lenient base64 decoder that ignores whitespace, so more than one string mapped to the same token.
4. The JSON parser kept duplicate object keys. Lookups returned the last one, but equality and serialization saw all of them, and an intermediary reading the first value could disagree with the server.

## Decision

1. Each HTTP connection is wrapped in a deadline-aware `Conn`. While idle it uses the keep-alive timeout. Once the first byte of a request arrives, the complete head and body must be received within `STEAM_MCP_REQUEST_READ_TIMEOUT` (default 30 s), or the connection is closed.
2. On Linux, the disconnect monitor calls `poll(2)` with `POLLRDHUP` through FFI, which reports a peer hangup even when unread bytes are present. Other platforms fall back to `peek`.
3. Security tokens use `b64url_decode_strict`: unpadded base64url only, no whitespace, and non-zero trailing bits rejected. Each byte string has exactly one accepted encoding.
4. The parser collapses duplicate keys, keeping the last occurrence. This matches what lookups returned before, and parsed values are now canonical.

## Consequences

- Clients that half-close their write side (`shutdown(SHUT_WR)`) after sending a request are treated as having cancelled it. HTTP/1.1 clients do not normally do this, and the spec treats closing the stream as cancellation.
- A request body that takes longer than the read deadline to upload is rejected. Raise the setting for slow links.
- The fuzz suite runs with fixed seeds in every `cargo test`. `scripts/check.sh --fuzz` runs it with more seeds and iterations.
