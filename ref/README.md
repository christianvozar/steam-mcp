# Reference material

This directory holds pinned copies of the specifications this server implements, plus inventories that tie the code to them. When the specs and our understanding of them disagree, these files are what we check against. Update them deliberately, and record why in an ADR (`docs/adr/`).

| Path | Contents | Source | Pinned at |
| --- | --- | --- | --- |
| `mcp-2026-07-28/spec/` | MCP specification prose for revision 2026-07-28 (`.mdx`, images omitted) | [modelcontextprotocol/modelcontextprotocol](https://github.com/modelcontextprotocol/modelcontextprotocol) `docs/specification/2026-07-28` | commit `2f3a95074916f092cd58dc06ede6a748f681a6ef` (2026-11-20) |
| `mcp-2026-07-28/schema/` | Normative TypeScript schema and generated JSON Schema | same repo, `schema/2026-07-28` | same commit |
| `mcp-ext-tasks/` | Tasks extension (`io.modelcontextprotocol/tasks`) spec and schema | [modelcontextprotocol/ext-tasks](https://github.com/modelcontextprotocol/ext-tasks) `specification/2026-07-28`, `schema/2026-07-28` | commit `cc9aaac1047a9440b07486e38f37060bbebeef4c` (2026-11-04) |
| `steam-webapi.md` | Every Steam endpoint the server calls: host, credential, cache class, verification status | Steamworks docs and Steam Web API conventions | maintained by hand |
| `mcp-compliance.md` | Normative MCP requirements mapped to the code that implements them and the tests that cover them | derived from the specs above | maintained by hand |

Licensing: the upstream license files are copied next to the material (`LICENSE`). The MCP project is moving from MIT to Apache-2.0, and the specification text is under those terms. The tasks extension is Apache-2.0.

## Refreshing

```sh
git clone --depth 1 https://github.com/modelcontextprotocol/modelcontextprotocol /tmp/mcp
git clone --depth 1 https://github.com/modelcontextprotocol/ext-tasks /tmp/ext-tasks
# copy the revision directories, update the commit hashes above, then diff
# the schema and record any behavioural changes in a new ADR.
```
