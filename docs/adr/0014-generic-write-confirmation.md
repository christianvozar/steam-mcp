# 0014. One confirmation gate for every state-changing tool

- Status: accepted (extends ADR 0009)
- Date: 2026-11-22

## Context

ADR 0009 gated a single write, `steam_partner_set_app_build_live`. The parity plan adds many more: leaderboard creation, scores, resets and deletion, and game server account creation, memos, token resets, deletion and bans. Copying the build-live flow into each tool would let the safety checks drift apart. Some of these writes also return secrets (game server login tokens).

## Decision

All new writes go through `run_write` in `src/mcp/tools/writes.rs`. Each tool only builds a `WriteOp` describing the change, and the gate applies the same rules to all of them:

1. The tool is hidden unless `STEAM_MCP_ENABLE_WRITES=true`, and it refuses to run without it.
2. Form elicitation is required. A client without it gets -32021 (missing capability). There is no URL hand-off, because Steamworks has no page that maps one-to-one onto these calls.
3. The user must accept and type the target's identifier: the leaderboard ID for score changes and resets, the leaderboard name for deletion, the server SteamID for account changes, or the app ID for creation. The prompt says plainly when a change cannot be undone.
4. The sealed `requestState` carries a SHA-256 fingerprint of the tool name and its canonical arguments. This sits on top of the server's own binding of state to arguments.
5. The write is sent uncached. A failure reported inside a 200 response (`result` other than 1, `success: false` or an `error` string) is returned as a tool error.
6. The cached reads the write affects are purged, matched on exact `name=value` pairs in the cache key.
7. An `audit` log line records the action, its non-secret subject and the principal. Secret response fields such as `login_token` are returned to the caller once and flagged in the summary. They are never logged and never cached.

Microtransaction and agreement tools take a required `sandbox` boolean instead of defaulting to either interface. The confirmation prompt names the mode (SANDBOX or LIVE) and lists every line item with the total, and the user confirms by typing the order or agreement ID.

## Consequences

- A new write needs only a `WriteOp` and a schema, and it gets every safeguard above.
- Clients without form elicitation cannot use these tools at all. That is intended.
- Typing the identifier slows down repeated writes. Bulk operations stay out of scope until there is a real need for them.
