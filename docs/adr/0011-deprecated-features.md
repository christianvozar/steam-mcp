# 0011. Use deprecated client features only where they fit, always with a fallback

- Status: accepted
- Date: 2026-11-21

## Context

2026-07-28 deprecates Roots, Sampling and Logging: new implementations SHOULD NOT adopt them. The project brief, however, asks for complete spec coverage.

## Decision

We implement them where they serve a real purpose, and only when the client declares the capability:

- **Sampling**: `steam_summarize_reviews` asks the client's LLM through MRTR. Without sampling support, it falls back to term-frequency themes.
- **Roots**: `export_report` (stdio only) discovers its output directory through MRTR `roots/list`. It falls back to an explicit `directory` argument or `STEAM_MCP_EXPORT_DIR`.
- **Logging**: request-scoped `notifications/message`, sent only when the request sets `logLevel`. Operational logs always go to stderr, in structured text or JSON.

## Consequences

- When these features are removed (no earlier than the first revision released on or after 2027-07-28), the fallbacks become the only behaviour.
- `includeContext` is always sent as `"none"`.
