# 0009. Keep state-changing tools off by default and require elicited confirmation

- Status: accepted
- Date: 2026-11-21

## Context

The partner API includes writes such as `SetAppBuildLive`, which changes what players download. Tool annotations are only hints, and models can misfire.

## Decision

A write tool appears only when `STEAM_MCP_ENABLE_WRITES=true` and the caller holds `steam:write`. Each call follows MRTR:

1. If the client cannot do form elicitation, the call fails with `-32021`.
2. Otherwise the user sees the change in context (the branch's current build) and must confirm and type the app ID.
3. Confirmation is accepted only together with a valid sealed `requestState` (ADR 0006).

Successful writes are logged to the `audit` target, and the related cache entries are invalidated. The generic `steam_api_call` passthrough refuses method names that look state-changing.

## Consequences

- Clients without elicitation cannot perform writes. This is intentional.
- Adding more write tools means following the same pattern.
