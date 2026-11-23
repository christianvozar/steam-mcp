# 0018. Serve many principals and unattended agents fairly and safely

- Status: accepted (numbered to match the same decision in twitch-mcp, whose 0015–0017 are Twitch-specific)
- Date: 2026-11-23

## Context

The server is no longer used by one person at a desk. Many agents call it from automated workflows, often in parallel, retrying on errors, and without a human to answer an elicitation prompt. That creates four problems:

1. **Fairness.** One runaway agent can exhaust the shared per-request rate limit, the upstream rate limit and the daily upstream budget for everyone.
2. **Retries.** An agent that times out and retries a write may apply it twice.
3. **Unattended writes.** ADR 0014 requires a person to confirm each write, which an unattended workflow cannot do. Granting such workflows blanket write access would be worse.
4. **Round trips and visibility.** Workflows often need many small reads, and operators need to see which agent is using what.

## Decision

All of this lives in `src/mcp/agents.rs` and is keyed by the authenticated principal (static token, JWT `sub` or introspection subject):

- **Rate.** The existing per-principal request limiter stays, and `STEAM_MCP_PRINCIPAL_LIMITS` overrides it per principal (`principal=rps:burst:concurrent:daily_calls:daily_upstream`, `*` wildcards).
- **Concurrency.** Each principal may run `STEAM_MCP_PRINCIPAL_MAX_CONCURRENT` tool calls at once (default 8). A call over the cap waits up to `STEAM_MCP_PRINCIPAL_QUEUE_MS` (default 5 s) for one of that principal's own calls to finish, so bursts queue rather than fail and cannot crowd out other principals.
- **Daily budgets.** Optional per-principal budgets cover tool calls and upstream fetches. The dispatcher records the calling principal in a thread-local (task workers and batch workers too), and the Steam client charges each actual fetch, that is each cache miss, to it. Steam's own 100k-calls-per-day key limit is shared by every principal, which is what this protects. Coalesced waiters and cache hits cost nothing. When a principal's upstream budget is spent, it still gets cached data.
- Limits fail with `-31029` and `retryAfterMs` plus a `reason` (`concurrency`, `daily_tool_calls`, rate). Over HTTP this is a 429 with `Retry-After`.
- **Idempotency.** Every state-changing tool accepts `idempotency_key`, as an argument or as `_meta["io.github.christianvozar/idempotencyKey"]`. Keys are scoped to the principal. A repeated key with the same arguments returns the stored successful result, marked `idempotentReplay`. A key that is still running gets a 409. A key reused with different arguments is refused. Secrets that a write returns once, such as game server login tokens, are removed from the stored copy, so a replay confirms success without repeating the secret. Failed or interrupted calls forget the key so they can be retried. Results are kept for `STEAM_MCP_IDEMPOTENCY_TTL` (default 24 h), in memory.
- **Pre-approved writes.** `STEAM_MCP_WRITE_POLICY` rules (`principal|tool|target[|max_per_day]`, `*` wildcards) let an operator approve specific writes for specific principals. The target is the same identifier a person would type to confirm. A matching write runs without elicitation and is counted against the rule's daily cap in the same atomic step. It still needs writes enabled and the write scope, and it is audited with the rule that approved it. A rule approving every tool for every principal is rejected at startup.
- **Dry runs.** `dry_run` (or `_meta["io.github.christianvozar/dryRun"]`) returns the planned change, the exact request and whether a policy rule would approve it, without acting.
- **Batches.** `batch_call` runs up to `STEAM_MCP_BATCH_MAX_CALLS` read-only calls on `STEAM_MCP_BATCH_PARALLELISM` workers. Each call is checked, validated, admitted and cached exactly like an individual call. Writes, exports and nested batches are refused.
- **Metering.** `usage_stats` reports calls, errors, upstream fetches, rejections and writes for the caller (or for anyone, with the admin scope). `audit_log` keeps the last 2,000 writes with who made them and how each was approved. `/metrics` adds per-principal series for the 50 busiest principals.

## Consequences

- Quotas, idempotency records and the audit ring are per process. Several replicas behind a load balancer each enforce their own limits. A shared store would need a new ADR, and sticky routing by principal is the simple mitigation meanwhile.
- Pre-approval moves the human decision from each write to the operator's policy. Rules should name a principal and a tool, and should stay narrow.
- Queuing adds latency under bursts, up to the queue wait, instead of errors. Workflows that would rather fail fast can set `STEAM_MCP_PRINCIPAL_QUEUE_MS=0`.
