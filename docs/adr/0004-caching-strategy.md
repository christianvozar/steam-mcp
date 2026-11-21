# 0004. Cache every upstream call with TTL classes, SWR, stale-if-error and coalescing

- Status: accepted
- Date: 2026-11-21

## Context

Steam data is queried far more often than it changes:

- Catalogs and schemas change daily.
- Store pages change hourly.
- Player counts change by the minute.

Steam enforces about 100k Web API calls per day per key and stricter storefront limits. MCP 2026-07-28 also asks servers for `ttlMs` and `cacheScope` hints on list, read and discover results.

## Decision

All upstream traffic goes through one in-process cache (`src/cache.rs`) with:

- **TTL classes** (`volatile`, `realtime`, `short`, `partner_live`, `medium`, `financial`, `long`, `static`), assigned per endpoint and overridable through configuration (hot-reloadable).
- **Stale-while-revalidate**: within the stale window, the stale value is served immediately and a single background refresh runs.
- **Stale-if-error**: an expired value up to `STEAM_MCP_MAX_STALE_ON_ERROR` old (default 24 h) is served when upstream fails. A TTL of 0 still stores the value so this keeps working (bug fixed in `fix(cache)`).
- **Single-flight coalescing** of concurrent misses.
- **Negative caching** of not-found results, with a short TTL.
- **LRU eviction** bounded by approximate bytes and entry count.
- **Atomic JSONL persistence** on an interval and on shutdown, so restarts start warm. Negative entries are not persisted.
- **Cache keys** made of host, path, sorted parameters and a *fingerprint* of the key class used. The key itself never appears.
- **Content-addressed immutability** where Steam provides it: item-definition archives are keyed by digest and cached `static`.

MCP cache hints are derived from the real cache state:

- A resource's `ttlMs` is the minimum remaining freshness of its underlying entries.
- `cacheScope` is `public` for data that is the same for every caller and `private` for player and partner data.
- Lists are `private` when authorization filters them.

Upstream protection sits behind the cache: a token bucket, a daily budget (once exhausted, only cached data is served), and retries with jittered backoff that honour `Retry-After`.

## Consequences

- Most calls are served without touching Steam, and results carry provenance (`hit`, `stale`, `miss`, `coalesced`, `stale-on-error`) so the model can judge freshness.
- The cache is per process. Replicas do not share it, though persistence can be pointed at shared storage per replica. A shared cache (for example Redis) would need a new ADR and a dependency.
- Memory sizes are approximations (`Value::approx_size`), not exact accounting.
