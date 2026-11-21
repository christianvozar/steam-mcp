# 0003. Implement MCP 2026-07-28 only (no legacy handshake)

- Status: accepted
- Date: 2026-11-21

## Context

MCP 2026-07-28 made the protocol stateless:

- It removed `initialize`, sessions, `ping` and `logging/setLevel`.
- It added `server/discover`, per-request `_meta` and MRTR.
- It replaced `resources/subscribe` and the HTTP GET stream with `subscriptions/listen`.

A "dual-era" server could also serve 2025-11-25 and older clients. We asked the project owner, who chose **modern only**.

## Decision

The server supports only `2026-07-28`.

- A legacy `initialize` request gets `UnsupportedProtocolVersionError` (`-32022`). Its message names the supported versions, as the versioning page recommends for legacy clients that cannot fall forward.
- Over HTTP, legacy traffic (GET, DELETE, session headers) is handled as the 2026-07-28 Streamable HTTP page recommends: 405 for those methods, and the headers are ignored.

## Consequences

- The implementation is much simpler: no connection state, no server-initiated requests, no resumable streams.
- Clients that only speak older revisions cannot use this server until they upgrade. This is documented in the README.
- If legacy support is needed later, it can be added as a separate dispatcher keyed on `initialize` (versioning: "dual-era server"), superseding this ADR.
