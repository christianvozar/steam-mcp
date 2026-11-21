# 0001. Record architecture decisions

- Status: accepted
- Date: 2026-11-21

## Context

This server makes many deliberate trade-offs: minimal dependencies, a brand-new protocol revision, aggressive caching. Without a written record, future contributors cannot tell which constraints are load-bearing and which are accidental.

## Decision

We record significant decisions as lightweight ADRs (Nygard format) in `docs/adr/`, numbered sequentially, using `0000-template.md`. We never edit an accepted ADR's decision; a new ADR supersedes it instead. Pinned upstream specifications and hand-maintained inventories live in `ref/`.

## Consequences

- Each new or changed decision costs a short document.
- `ref/` must be refreshed deliberately, and doing so should produce an ADR when behaviour changes.
