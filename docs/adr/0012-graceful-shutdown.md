# 0012. Shut down gracefully on SIGTERM/SIGINT without extra crates

- Status: accepted
- Date: 2026-11-21

## Context

In HTTP mode the process is normally stopped by a signal (container stop, systemd, Ctrl-C). Previously there was no signal handling. A stop killed the process outright:

- `subscriptions/listen` streams were not completed;
- in-flight requests were abandoned;
- up to one persistence interval of cache updates was lost.

Signal-handling crates would add dependencies (ADR 0002).

## Decision

- Install SIGINT and SIGTERM handlers through a direct FFI call to libc's `signal()`. std already links libc on Unix, so this adds no crates. The handler only stores the signal number in an atomic, which is async-signal-safe.
- A watcher thread runs the shutdown in phases:
  1. `begin_shutdown`: stop accepting work (new requests get 503 and `/readyz` reports stopping), complete subscriptions, cancel tasks.
  2. `drain`: wait until no request is in flight, or `STEAM_MCP_SHUTDOWN_GRACE` elapses.
  3. `finish_shutdown`: write the cache snapshot exactly once. A mutex makes any concurrent caller wait for the write to finish, which fixes a race where `main` could return mid-write.
  4. Exit 0. A second signal exits immediately with code 130.
- Stdio EOF uses the same phases.

## Consequences

- Rolling restarts lose no cache state and close streams cleanly.
- On non-Unix targets, signal handling is a no-op. Stdio EOF and the periodic snapshots still apply.
