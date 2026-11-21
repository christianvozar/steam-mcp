# 0005. Use OS threads and blocking I/O instead of an async runtime

- Status: accepted
- Date: 2026-11-21

## Context

An async runtime (tokio) would add a large dependency tree, which conflicts with ADR 0002. The workload is I/O bound but moderate: an MCP server for a handful of clients, with Steam calls limited to about 10 requests per second.

## Decision

The server uses `std::thread` with blocking sockets:

- one thread per HTTP connection, capped by `STEAM_MCP_MAX_CONNECTIONS`;
- one thread per in-flight stdio request, capped at 64;
- background threads for cache refresh, cache maintenance, the change watcher and task workers.

Cross-thread signalling uses `Mutex`/`Condvar` and atomics. To detect HTTP client disconnects, a short-lived monitor thread peeks the socket while a request is processed.

## Consequences

- The code is simple, has no colouring, and is easy to debug.
- Thousands of idle long-lived `subscriptions/listen` streams would cost one thread each. That is acceptable for the expected scale, and the connection cap bounds the resource use.
- Disconnect detection on TLS connections sees only TCP FIN, not `close_notify`.
