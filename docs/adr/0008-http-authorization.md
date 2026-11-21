# 0008. Act as an OAuth 2.1 resource server with pluggable token validation

- Status: accepted
- Date: 2026-11-21

## Context

On HTTP, MCP expects OAuth 2.1 resource-server behaviour: RFC 9728 metadata, `WWW-Authenticate` challenges, audience validation (RFC 8707), and no token passthrough. Deployments vary: some use an identity provider, some want simple shared tokens.

## Decision

`STEAM_MCP_AUTH_MODE` selects the validation mode:

- `none` — for stdio and loopback development.
- `static` — SHA-256-hashed tokens, compared in constant time, each with its own scopes.
- `jwt` — HS256 through the in-tree HMAC, and/or RS256 and ES256 through `ring` using a cached JWKS. It checks `exp`, `nbf` and `iss`, and requires `aud` to match the configured resource.
- `introspection` — RFC 7662, with results cached for up to 60 s (10 s when inactive).

The server publishes Protected Resource Metadata at both the root and the path-inserted well-known URIs. Scopes form a hierarchy: `steam:admin` ⊇ `steam:write` ⊇ `steam:partner` ⊇ `steam:public`, and `steam:financial` ⊇ `steam:public`. Lists are filtered by scope. A call without the needed scope gets 403 with `insufficient_scope` and the required scope.

Steam calls always use server-held keys. Client tokens never leave the server.

## Consequences

- We do not run an authorization server. Operators bring one, or use static tokens.
- A token's scopes determine which partner and financial data it can reach. The Steam key itself is shared by all authorized callers of a deployment.
