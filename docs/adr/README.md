# Architecture Decision Records

| # | Decision | Status |
| --- | --- | --- |
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | accepted |
| [0002](0002-minimal-dependencies.md) | Build almost everything in-tree; delegate only TLS and asymmetric crypto | accepted |
| [0003](0003-modern-only-protocol.md) | Implement MCP 2026-07-28 only | accepted |
| [0004](0004-caching-strategy.md) | Cache every upstream call (TTL classes, SWR, stale-if-error, coalescing) | accepted |
| [0005](0005-threads-not-async.md) | OS threads and blocking I/O instead of async | accepted |
| [0006](0006-mrtr-request-state.md) | HMAC-sealed MRTR requestState | accepted |
| [0007](0007-resources-backed-by-tools.md) | Back every resource with a tool handler | accepted |
| [0008](0008-http-authorization.md) | OAuth 2.1 resource server with pluggable validation | accepted |
| [0009](0009-gated-writes.md) | Writes off by default and confirmed through elicitation | accepted |
| [0010](0010-in-memory-tasks.md) | In-memory, principal-owned tasks with a synchronous window | accepted |
| [0011](0011-deprecated-features.md) | Deprecated features only with fallbacks | accepted |
| [0012](0012-graceful-shutdown.md) | Graceful shutdown on SIGTERM/SIGINT without extra crates | accepted |
| [0013](0013-transport-hardening.md) | Read deadlines, hangup detection, canonical tokens and JSON | accepted |
| [0014](0014-generic-write-confirmation.md) | One confirmation gate for every state-changing tool | accepted |

New ADRs copy [`0000-template.md`](0000-template.md) and take the next number.
