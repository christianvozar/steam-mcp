# 0007. Back every resource with a tool handler

- Status: accepted
- Date: 2026-11-21

## Context

Many resources expose the same data as tools, such as an app's details or player counts. Duplicating fetch, shaping and error logic would drift, and it would split cache entries.

## Decision

Each static resource and resource template maps to a tool name plus an argument builder (`src/mcp/resources.rs`). A read:

1. runs the tool handler;
2. returns its `structuredContent` as pretty JSON;
3. turns `isError` into "resource not found" (`-32602`, with `data.uri`);
4. derives `ttlMs` and `lastModified` from the cache provenance in the result.

Resource visibility follows the tool's scope, its required key, and whether it is disabled. Prompts embed resources the same way. Subscription change detection re-reads resources through the cache and compares content hashes.

## Consequences

- There is one code path and one cache entry per datum, and tools and resources stay consistent.
- Resource content is whatever the tool returns. Changing a tool's output shape changes the resource.
- Subscription checks cost a cache lookup per watched URI per watch interval. They only reach upstream when entries go stale.
