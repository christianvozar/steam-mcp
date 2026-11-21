# 0006. Seal MRTR requestState with HMAC, bound to method, parameters, principal and expiry

- Status: accepted
- Date: 2026-11-21

## Context

MRTR passes `requestState` through the client, so the server must treat it as attacker-controlled. When state influences behaviour (for example confirming a build change), the server MUST protect its integrity and SHOULD bind it to the principal, a short TTL and the originating request.

## Decision

The token format is `v1.<b64url(json)>.<b64url(HMAC-SHA256)>`. The payload records:

- the method;
- a SHA-256 digest of the canonicalized salient parameters (`{name, arguments}`);
- a hash of the principal id;
- an expiry (10 minutes);
- a nonce;
- the handler's payload.

Verification uses constant-time comparison. Any failure returns `-32602`.

The key comes from `STEAM_MCP_STATE_SECRET`, hashed. If it is not set, a random per-process key is generated. Payloads are signed, not encrypted, so they must never contain secrets.

## Consequences

- Retries work statelessly on any replica that shares the secret.
- A token is not single-use. That is acceptable because the state only gates *asking again*: the destructive write still requires a fresh user confirmation in the same retry, and it is idempotent.
- Changing the secret invalidates in-flight MRTR exchanges. Clients simply restart them.
