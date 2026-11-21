# 0010. Keep tasks in memory, owned by their principal, with a synchronous window

- Status: accepted
- Date: 2026-11-21

## Context

The Tasks extension lets the server return a handle for long work: multi-page sales, catalog loads, multi-app comparisons. Tasks must be durably created before they are returned, and they are polled through `tasks/get`.

## Decision

A task-capable tool, called by a client that declares the extension, runs on a worker thread.

- If it finishes within `STEAM_MCP_TASK_SYNC_WINDOW_MS` (default 1.5 s), its result is returned directly.
- Otherwise the server returns `CreateTaskResult`. The task is registered before the response is sent.

Tasks are held in memory:

- Each task is owned by its principal. Other principals get "not found".
- Tasks expire after `STEAM_MCP_TASK_TTL`.
- Status changes are pushed as `notifications/tasks` to subscriptions that listen for them.
- Mid-task input uses unique keys and `tasks/update`.
- Cancellation is cooperative, and terminal states are immutable.

## Consequences

- Tasks do not survive a restart and are not shared between replicas. Load balancers must route `tasks/*` by `Mcp-Name` (the task id). Durable tasks would need a store, and a new ADR.
- Quick calls avoid polling overhead even for task-capable clients.
