# steam-mcp

A production [Model Context Protocol](https://modelcontextprotocol.io) server for Steam, written in Rust. It gives LLM applications cached access to:

- **The public Steam Web API**: players, libraries, achievements, stats, news, workshop, servers and charts.
- **The Steam storefront**: app and package details, pricing, reviews and featured lists.
- **The Steamworks partner Web API** (with a publisher key): apps, builds, branches, depots, ownership, leaderboards, microtransactions, cheating reports, inventory and ticket validation.
- **Steamworks financial reporting** (with a Financial API key): changed dates, detailed sales and aggregated sales summaries.

It implements MCP revision **2026-07-28**, the stateless protocol, completely. It also implements the official **Tasks** extension. See [Protocol coverage](#protocol-coverage).

## Design goals

- **Heavy caching.** Steam data is read far more often than it changes. Every upstream call goes through a shared cache that uses TTL classes, stale-while-revalidate, stale-if-error, request coalescing, negative caching, LRU eviction and disk persistence. Cache provenance is returned with every result, and MCP `ttlMs`/`cacheScope` hints are derived from the real cache state.
- **Few dependencies.** JSON, HTTP/1.1 (client and server), SSE, SHA-256, HMAC, base64, URI templates, JSON Schema validation, rate limiting, the cache and threading are all implemented in-tree. TLS (`rustls`, `webpki-roots`) and asymmetric JWT signature verification (`ring`, which `rustls` already depends on) use audited crates, because hand-written versions would be unsafe. The full dependency tree is 15 crates.
- **Safe by default.** State-changing tools are off unless you enable them, and they require the user to confirm through elicitation. API keys are never logged, never returned and never part of cache keys (keys are fingerprinted). OAuth tokens are validated for audience and are never passed through to Steam.

## Quick start

```sh
cargo build --release
export STEAM_API_KEY=...            # https://steamcommunity.com/dev/apikey
export STEAM_PUBLISHER_KEY=...      # optional: Steamworks publisher Web API key
export STEAM_FINANCIAL_KEY=...      # optional: Financial API group key
export STEAM_MCP_CACHE_FILE=~/.cache/steam-mcp/cache.jsonl   # optional: warm restarts

# stdio (for local MCP clients)
./target/release/steam-mcp --stdio

# Streamable HTTP
./target/release/steam-mcp --http 127.0.0.1:8080
```

A client configuration for stdio:

```json
{
  "mcpServers": {
    "steam": {
      "command": "/path/to/steam-mcp",
      "args": ["--stdio"],
      "env": { "STEAM_API_KEY": "…", "STEAM_MCP_CACHE_FILE": "/home/me/.cache/steam-mcp.jsonl" }
    }
  }
}
```

> The server speaks only the stateless **2026-07-28** revision. It answers a legacy `initialize` handshake with `UnsupportedProtocolVersionError` (`-32022`), which lists the supported versions. Clients must send `_meta.io.modelcontextprotocol/protocolVersion` and `clientCapabilities` on every request.

Useful commands:

```sh
steam-mcp --help            # every setting, with descriptions
steam-mcp --list-tools      # tool catalogue
steam-mcp --print-config    # effective configuration (secrets redacted)
```

## Tools

The tool list is filtered by the configured credentials and the caller's scopes. A server with no keys still offers the storefront and keyless Web API tools. Without a key, app search uses the store's search suggestions instead of the full app list, which Valve no longer serves keylessly. Results include a human-readable summary, the JSON as text and `structuredContent`. Cache provenance is in `_meta["io.github.christianvozar/cache"]` (`hit`, `stale`, `miss`, `coalesced` or `stale-on-error`, plus age and TTL). Every tool also accepts `force_refresh`.

| Tool | Scope | Title |
| --- | --- | --- |
| `steam_search_apps` | steam:public | Search Steam Apps |
| `steam_get_app_details` | steam:public | App Store Details |
| `steam_get_app_reviews` | steam:public | App Reviews |
| `steam_get_review_summary` | steam:public | Review Score Summary |
| `steam_get_current_players` | steam:public | Current Player Count |
| `steam_get_app_news` | steam:public | App News |
| `steam_get_global_achievement_percentages` | steam:public | Global Achievement Rates |
| `steam_get_game_schema` | steam:public | Game Stats Schema |
| `steam_get_global_stats` | steam:public | Global Aggregated Stats |
| `steam_get_player_summaries` | steam:public | Player Profiles |
| `steam_resolve_vanity_url` | steam:public | Resolve Vanity URL |
| `steam_get_owned_games` | steam:public | Owned Games |
| `steam_get_recently_played_games` | steam:public | Recently Played |
| `steam_get_player_achievements` | steam:public | Player Achievements |
| `steam_get_user_stats_for_game` | steam:public | Player Game Stats |
| `steam_get_friend_list` | steam:public | Friend List |
| `steam_get_player_bans` | steam:public | Player Bans |
| `steam_get_steam_level` | steam:public | Steam Level |
| `steam_get_badges` | steam:public | Badges |
| `steam_get_player_profile` | steam:public | Player Profile Overview |
| `steam_get_most_played_games` | steam:public | Most Played Games |
| `steam_get_workshop_item_details` | steam:public | Workshop Item Details |
| `steam_query_workshop_files` | steam:public | Search Workshop |
| `steam_get_supported_api_list` | steam:public | Supported Web API Methods |
| `steam_get_store_featured` | steam:public | Store Featured |
| `steam_get_package_details` | steam:public | Package Details |
| `steam_get_wishlist` | steam:public | Wishlist |
| `steam_get_servers_at_address` | steam:public | Servers at Address |
| `steam_check_app_up_to_date` | steam:public | Version Up-To-Date Check |
| `steam_get_game_servers` | steam:public | Game Server List |
| `steam_get_app_overview` | steam:public | App Overview |
| `steam_compare_apps` | steam:public | Compare Apps |
| `steam_get_store_items` | steam:public | Store Items (Batch) |
| `steam_search_store` | steam:public | Search Store |
| `steam_get_store_tags` | steam:public | Store Tags |
| `steam_get_weekly_top_sellers` | steam:public | Weekly Top Sellers |
| `steam_get_top_concurrent_games` | steam:public | Top Games by Concurrent Players |
| `steam_get_top_steam_deck_games` | steam:public | Top Steam Deck Games |
| `steam_get_top_releases` | steam:public | Top Releases by Month |
| `steam_get_workshop_collection` | steam:public | Workshop Collections |
| `steam_get_gc_version` | steam:public | Game Coordinator Version |
| `steam_get_sdr_config` | steam:public | Steam Datagram Relay Config |
| `steam_get_depot_patch_info` | steam:public | Depot Patch Info |
| `steam_get_wishlist_count` | steam:public | Wishlist Count |
| `steam_get_followed_games` | steam:public | Followed Games |
| `steam_get_workshop_vote_summary` | steam:public | Workshop Vote Summary |
| `steam_get_single_game_playtime` | steam:public | Single Game Playtime |
| `steam_is_playing_shared_game` | steam:public | Family Sharing Check |
| `steam_get_badge_progress` | steam:public | Community Badge Progress |
| `steam_get_user_group_list` | steam:public | User Groups |
| `steam_get_asset_class_info` | steam:public | Asset Class Info |
| `steam_partner_get_store_items` | steam:partner | Partner Store Items |
| `steam_partner_get_workshop_contributors` | steam:partner | Workshop Item Contributors |
| `steam_get_profile_items` | steam:public | Equipped Profile Items |
| `steam_get_top_achievements_for_games` | steam:public | Top Achievements per Game |
| `steam_get_workshop_details` | steam:public | Workshop Details (Rich) |
| `steam_get_user_workshop_files` | steam:public | User Workshop Files |
| `steam_get_workshop_subsections` | steam:public | Workshop Sub-sections |
| `steam_get_ugc_file_details` | steam:public | UGC File Details |
| `steam_get_store_categories` | steam:public | Store Categories |
| `steam_query_store` | steam:public | Browse Store |
| `steam_get_wishlist_detailed` | steam:public | Wishlist with Store Data |
| `steam_get_dlc_for_apps` | steam:public | DLC for Apps |
| `steam_get_server_steamids_by_ip` | steam:public | Server SteamIDs by IP |
| `steam_get_server_ips_by_steamid` | steam:public | Server IPs by SteamID |
| `steam_get_game_server_accounts` | steam:public | Game Server Accounts |
| `steam_get_game_server_account_info` | steam:public | Game Server Account Info |
| `steam_query_game_server_token` | steam:public | Query Game Server Token |
| `steam_get_game_server_player_stats` | steam:public | Game Server Player Stats |
| `steam_partner_get_news_authed` | steam:partner | News (Including Unpublished) |
| `steam_partner_get_workshop_item_revenue` | steam:financial | Workshop Item Revenue |
| `steam_partner_get_market_prices` | steam:partner | Market Prices |
| `steam_partner_can_trade` | steam:partner | Can Trade |
| `steam_partner_get_market_popular` | steam:partner | Popular Market Listings |
| `steam_partner_get_market_asset_id` | steam:partner | Market Listing Asset |
| `steam_partner_get_market_eligibility` | steam:partner | Market Eligibility |
| `steam_partner_get_inventory_item_defs` | steam:partner | Inventory Item Definitions (Service) |
| `steam_partner_get_inventory_price_sheet` | steam:partner | Inventory Price Sheet |
| `steam_partner_get_inventory_quantity` | steam:partner | Inventory Quantities |
| `steam_partner_get_inventory_history` | steam:partner | Inventory History |
| `steam_partner_get_inventory_history_command` | steam:partner | Inventory History Command |
| `steam_partner_get_asset_history` | steam:partner | Asset History |
| `steam_partner_get_notification_sessions` | steam:partner | Game Notification Sessions |
| `steam_partner_get_notification_session_details` | steam:partner | Notification Session Details |
| `steam_partner_get_lobby_data` | steam:partner | Lobby Data |
| `steam_partner_get_microtxn_user_info` | steam:partner | Microtransaction User Info |
| `steam_partner_get_microtxn_agreements` | steam:partner | Recurring Agreements |
| `steam_partner_get_vac_status` | steam:partner | VAC Status |
| `steam_partner_create_leaderboard` | steam:write | Create Leaderboard |
| `steam_partner_set_leaderboard_score` | steam:write | Set Leaderboard Score |
| `steam_partner_delete_leaderboard_score` | steam:write | Delete Leaderboard Score |
| `steam_partner_reset_leaderboard` | steam:write | Reset Leaderboard |
| `steam_partner_delete_leaderboard` | steam:write | Delete Leaderboard |
| `steam_create_game_server_account` | steam:write | Create Game Server Account |
| `steam_set_game_server_memo` | steam:write | Set Game Server Memo |
| `steam_reset_game_server_token` | steam:write | Reset Game Server Token |
| `steam_delete_game_server_account` | steam:write | Delete Game Server Account |
| `steam_partner_set_game_server_ban` | steam:write | Ban Game Server Account |
| `steam_partner_list_apps` | steam:partner | Partner App List |
| `steam_partner_check_app_ownership` | steam:partner | Check App Ownership |
| `steam_partner_get_publisher_app_ownership` | steam:partner | Publisher App Ownership |
| `steam_partner_get_ownership_changes` | steam:partner | Ownership Changes |
| `steam_partner_get_app_price_info` | steam:partner | App Price Info |
| `steam_partner_get_deleted_steamids` | steam:partner | Deleted SteamIDs |
| `steam_partner_get_app_betas` | steam:partner | App Branches |
| `steam_partner_get_app_builds` | steam:partner | App Builds |
| `steam_partner_get_app_depot_versions` | steam:partner | Depot Versions |
| `steam_partner_get_players_banned` | steam:partner | Banned Players |
| `steam_partner_get_cheating_reports` | steam:partner | Cheating Reports |
| `steam_partner_get_server_list` | steam:partner | Partner Server List |
| `steam_partner_get_leaderboards` | steam:partner | Leaderboards |
| `steam_partner_get_leaderboard_entries` | steam:partner | Leaderboard Entries |
| `steam_partner_get_microtxn_report` | steam:partner | Microtransaction Report |
| `steam_partner_query_microtxn` | steam:partner | Query Transaction |
| `steam_partner_get_item_definitions` | steam:partner | Inventory Item Definitions |
| `steam_partner_get_inventory` | steam:partner | User Inventory |
| `steam_partner_authenticate_ticket` | steam:partner | Authenticate User Ticket |
| `steam_partner_get_asset_prices` | steam:partner | Asset Prices |
| `steam_partner_get_changed_sales_dates` | steam:financial | Changed Sales Dates |
| `steam_partner_get_detailed_sales` | steam:financial | Detailed Sales |
| `steam_partner_sales_summary` | steam:financial | Sales Summary |
| `steam_partner_set_app_build_live` | steam:write | Set Build Live |
| `cache_stats` | steam:public | Cache Statistics |
| `cache_invalidate` | steam:admin | Invalidate Cache |
| `steam_api_call` | steam:partner | Steam Web API Call |
| `export_report` | steam:public | Export Report |
| `steam_summarize_reviews` | steam:public | Summarize Reviews |

Notes:

- **`steam_api_call`** is a generic GET passthrough for any Web API method that has no dedicated tool. Use `steam_get_supported_api_list` to discover methods. It refuses methods whose names look state-changing, and it caches results under a TTL class you choose.
- **`steam_partner_set_app_build_live`** only appears when `STEAM_MCP_ENABLE_WRITES=true`. With a client that supports form elicitation, the user is shown the branch's current build and must type the app ID to confirm. The confirmation is bound to the call through an HMAC-sealed `requestState`. A client that only supports URL elicitation is handed off to the Steamworks builds page so the user can make the change there; the server makes no API write in that case.
- **Leaderboard and game server account writes** (`steam_partner_create_leaderboard`, `steam_partner_set_leaderboard_score`, `steam_partner_delete_leaderboard_score`, `steam_partner_reset_leaderboard`, `steam_partner_delete_leaderboard`, `steam_create_game_server_account`, `steam_set_game_server_memo`, `steam_reset_game_server_token`, `steam_delete_game_server_account`, `steam_partner_set_game_server_ban`) only appear when writes are enabled. Each needs form elicitation: the user types the target's identifier (leaderboard ID or name, server SteamID or app ID) to confirm. The affected cached reads are purged afterwards, and every write is audit-logged. Login tokens are returned once and never logged or cached. See ADR 0014.
- **`steam_summarize_reviews`** uses MRTR sampling when the client supports it. Otherwise it returns themes ranked by term frequency.
- **`export_report`** (stdio only) runs any read-only data tool and writes the result as CSV or JSON into the client's first root, obtained through MRTR `roots/list`. It never overwrites a file and neutralises CSV formula injection.
- **Keyless store services**: `steam_get_store_items`, `steam_search_store`, the charts and several other tools use Steam's protobuf-backed services (`input_json`), which need no key. `steam_search_apps` and review summaries fall back to them, so a server with no keys still gets name search and review scores. They are also what makes review scores available when the storefront's `/appreviews` endpoint refuses a network.
- **Extended coverage** (`src/mcp/tools/extended.rs`): profile cosmetics, rich workshop details, store browsing by tag (`steam_query_store` accepts tag names), detailed wishlists, DLC lists, game-server accounts and tokens, Community Market and Inventory Service reads, inventory history, notification sessions, lobbies, microtransaction user data and VAC status. Partner tools map Steam's parameters one to one; required parameters were found from Steam's own error messages. `steam_query_game_server_token` and `steam_partner_get_vac_status` are never cached.
- **Task-capable tools** (`steam_search_apps`, `steam_get_app_reviews`, `steam_compare_apps`, `steam_partner_get_detailed_sales`, `steam_partner_sales_summary`) return a task handle when the client declares the tasks extension and the work takes longer than `STEAM_MCP_TASK_SYNC_WINDOW_MS`. While running as a task, a multi-page sales fetch that reaches its page limit asks the user through `tasks/update` whether to continue.
- Every endpoint was checked against live Steam on 2026-11-22. 14 returned correct data without a key. The other 39 were confirmed to exist at the version we call, using unauthenticated probes. The live run found four bugs, all fixed; see `ref/steam-webapi.md` for the details. When Steam answers with an unknown method or version, the tool says so explicitly rather than reporting "no data". `steam_api_call` can reach any other method or version.

## Resources and prompts

Static resources include `steam://store/featured`, `steam://store/tags`, `steam://charts/most-played`, `steam://charts/top-concurrent`, `steam://charts/weekly-top-sellers`, `steam://charts/steam-deck`, `steam://catalog/summary`, `steam://webapi/supported`, `steam://server/cache-stats` and `steam://partner/apps`. Each partner app is also listed as `steam://app/{appid}/overview`.

Resource templates (RFC 6570), all with argument completion:

```
steam://app/{appid}/details | store | overview | players | news | reviews | achievements | schema
steam://player/{steamid}/profile | games | recent
steam://partner/app/{appid}/betas | builds | depots | leaderboards
steam://partner/sales/{date}
```

Each resource is served by the same cached handler as its tool. `ttlMs` is the remaining freshness of the underlying cache entries. `cacheScope` is `public` for data that is the same for every caller and `private` for player and partner data. You can subscribe to any resource with `subscriptions/listen`. A watcher re-reads subscribed resources from the cache (so upstream is only hit when entries go stale) and sends `notifications/resources/updated` when the content hash changes.

Prompts: `game_health_report`, `review_sentiment_analysis`, `player_profile_report`, `achievement_design_review`, `patch_notes_digest`, `competitor_comparison`, `release_readiness_check` (partner) and `partner_sales_report` (financial). Prompts embed live data as embedded resources. If a required argument is missing and the client supports elicitation, the server requests it through MRTR.

Completion covers app IDs (your partner apps first, then a name search of the cached catalog), languages, country codes, sales dates and focus areas.

## Caching

| Class | Default TTL / stale window | Used for |
| --- | --- | --- |
| `volatile` | 30 s / 60 s | ownership checks, transactions, inventories |
| `realtime` | 60 s / 5 min | current player counts, live server lists |
| `short` | 15 min / 1 h | news, reviews, workshop queries, recently played |
| `partner_live` | 5 min / 15 min | builds, branches, depots |
| `medium` | 1 h / 6 h | store pages, profiles, libraries, partner app list |
| `financial` | 1 h / 6 h | detailed sales pages |
| `long` | 6 h / 24 h | achievement percentages, global stats |
| `static` | 24 h / 7 d | schemas, the app catalog, API lists, item-definition archives (keyed by digest) |

To override a class, set `STEAM_MCP_TTL_<CLASS>=ttl[,stale]` in seconds. TTL classes reload live from the config file.

- **Fresh** entries are served directly.
- **Stale** entries (past the TTL but within the stale window) are served immediately, and one background refresh runs.
- **Expired** entries are refetched. Concurrent callers for the same key share one upstream request.
- If upstream fails, a value up to `STEAM_MCP_MAX_STALE_ON_ERROR` old (default 24 h) is served instead of an error.
- "Not found" results are cached briefly.
- Memory is bounded by `STEAM_MCP_CACHE_MAX_BYTES` and `STEAM_MCP_CACHE_MAX_ENTRIES`, with LRU eviction.
- With `STEAM_MCP_CACHE_FILE` set, the cache is written to disk atomically every `STEAM_MCP_CACHE_PERSIST_INTERVAL` seconds and on shutdown, then reloaded at start.

Upstream protection includes:

- A token bucket (`STEAM_MCP_UPSTREAM_RPS` / `_BURST`).
- A daily call budget. It warns at 80%. Once the budget is exhausted, only cached data is served.
- Retries with jittered exponential backoff that honour `Retry-After` on 429 and 5xx responses.
- Per-caller request rate limits.

## Streamable HTTP and authorization

`--http ADDR` serves a single MCP endpoint (`STEAM_MCP_HTTP_PATH`, default `/mcp`):

- POST only. GET and DELETE return `405`. There are no sessions; `Mcp-Session-Id` and `Last-Event-ID` are ignored.
- Responses are JSON by default. A request that carries a `progressToken` or `logLevel`, and every `subscriptions/listen` request, gets a request-scoped SSE stream. SSE responses send `X-Accel-Buffering: no` and keep-alive comments.
- The server validates `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name` and `Mcp-Param-*` against the body, including `=?base64?…?=` decoding. Failures return `400` with `HeaderMismatch` (`-32020`). `appid` tool parameters are annotated with `x-mcp-header: AppId`.
- The `Origin` header is validated. By default only loopback origins are allowed; set `STEAM_MCP_ALLOWED_ORIGINS` to change this. CORS preflight is supported.
- Closing a response stream cancels the request.
- Native TLS is available with `STEAM_MCP_TLS_CERT` and `STEAM_MCP_TLS_KEY`.
- Once a request starts arriving, it must be complete within `STEAM_MCP_REQUEST_READ_TIMEOUT` (default 30 s). This defeats slow-drip ("slowloris") clients. Idle keep-alive connections close after 60 s.
- On Linux, peer hangups are detected with `poll(POLLRDHUP)`, so closing a TLS connection cancels its request even when a close_notify alert is still unread.
- Operational endpoints: `/healthz`, `/readyz` (returns 503 while shutting down), and `/metrics` (Prometheus format; requires `steam:admin` when auth is enabled).

### Shutdown

On SIGTERM or SIGINT, and on stdin EOF in stdio mode, the server shuts down in order:

1. It stops accepting work. New requests get `503`.
2. It ends every `subscriptions/listen` stream with a completion result.
3. It cancels tasks.
4. It lets in-flight requests finish, for up to `STEAM_MCP_SHUTDOWN_GRACE` seconds (default 10).
5. It writes the cache snapshot and exits 0.

A second signal exits immediately.

### Authorization

Set `STEAM_MCP_AUTH_MODE` to choose how bearer tokens are validated:

| Mode | Validation |
| --- | --- |
| `static` | `STEAM_MCP_AUTH_TOKENS="token=principal:scope scope;…"`. Tokens are compared in constant time against SHA-256 hashes. |
| `jwt` | HS256 with `STEAM_MCP_JWT_HS256_SECRET`, and/or RS256 and ES256 using `STEAM_MCP_JWKS_URL` (cached, refreshed when an unknown `kid` appears). Checks `exp`, `nbf` and `iss`, and requires the `aud` claim to match `STEAM_MCP_PUBLIC_URL` (RFC 8707). Scopes come from `scope` or `scp`. |
| `introspection` | RFC 7662 against `STEAM_MCP_INTROSPECTION_URL`, using client credentials. Active results are cached for up to 60 s, inactive results for 10 s. |

In every mode:

- Protected Resource Metadata (RFC 9728) is served at `/.well-known/oauth-protected-resource` and at the path-inserted form `/.well-known/oauth-protected-resource/mcp`. It lists `STEAM_MCP_AUTHORIZATION_SERVERS`.
- A missing or invalid token gets `401` with `WWW-Authenticate: Bearer resource_metadata="…", scope="…"`.
- A call that needs more scope gets `403` with `error="insufficient_scope"` and the required scope.

Scopes form a hierarchy:

- `steam:admin` includes every other scope.
- `steam:write` includes `steam:partner`.
- `steam:partner` includes `steam:public`.
- `steam:financial` includes `steam:public`.

Tool, prompt and resource lists are filtered by the caller's scopes.

Without authentication, which is the default for stdio, the caller is trusted fully. The server logs a warning if you serve HTTP on a non-loopback address without auth.

## Protocol coverage

| Area | Implementation |
| --- | --- |
| Base protocol | JSON-RPC 2.0. String or integer ids (never null). `resultType` on every result. `serverInfo` in result `_meta`. Batches rejected. |
| Statelessness | Per-request `_meta` validation: `protocolVersion` and `clientCapabilities` are required. Optional `clientInfo`, `logLevel` and `progressToken` are supported. Nothing is inferred from the connection. |
| Versioning | `server/discover` with `supportedVersions`, capabilities, instructions and cache hints. `UnsupportedProtocolVersionError` (`-32022`) includes `supported` and `requested`. A legacy `initialize` gets an explanatory `-32022`. |
| Error codes | `-32700` to `-32603`, `-32020` (HeaderMismatch), `-32021` (MissingRequiredClientCapability, with `requiredCapabilities`) and `-32022`. Application codes sit outside the reserved range. "Resource not found" uses `-32602`. |
| MRTR | `InputRequiredResult` with `inputRequests` for elicitation (form), sampling and roots. `requestState` is HMAC-sealed and bound to the method, a parameter digest, the principal and an expiry. Requests are only sent when the client declares the matching capability. |
| Tools | Listing (deterministic order, paginated, `ttlMs`/`cacheScope`), calling, `structuredContent` plus a text mirror, `outputSchema`, annotations, `isError` execution errors versus protocol errors, input validation (JSON Schema 2020-12 subset including `pattern` via a bounded in-tree regex engine, local `$ref`, bounded), lenient coercion, `x-mcp-header`. |
| Resources | List, templates, read with `ttlMs`/`cacheScope`, annotations (`audience`, `priority`), `lastModified` in content `_meta`, subscriptions with `notifications/resources/updated`, list-changed notifications. |
| Prompts | List and get, embedded resources, icons, list-changed notifications, MRTR for missing arguments. |
| Completion | `ref/prompt` and `ref/resource`, at most 100 values, `total` and `hasMore`. |
| Subscriptions | `subscriptions/listen` with a notification filter. The acknowledgement is sent first and lists the honoured subset. Every notification is tagged with `subscriptionId`. Multiple concurrent subscriptions are supported. The client cancels by closing the stream (HTTP) or with `notifications/cancelled` (stdio). Shutdown ends subscriptions with a completion result. |
| Progress and cancellation | `notifications/progress` with strictly increasing values, on the request's own stream. Cancelled requests get no response. |
| Logging (deprecated) | Request-scoped `notifications/message`, only when `logLevel` is present. Invalid levels get `-32602`. |
| Caching | `ttlMs` and `cacheScope` on discover, list and read results. No hints on `input_required` results. |
| Pagination | Opaque cursors. Invalid cursors get `-32602`. |
| Tasks extension | `io.modelcontextprotocol/tasks`: `CreateTaskResult` (`resultType: "task"`), `tasks/get`, `tasks/update` (mid-task elicitation), `tasks/cancel`, and `notifications/tasks` through `subscriptions/listen` `taskIds`. Tasks are owned by their principal. `-32021` for clients that don't declare the extension. |
| Transports | stdio (newline-delimited, concurrent, cancellation, graceful EOF shutdown) and Streamable HTTP (described above). |
| Authorization | Covered above. |
| Icons | The server, tools, resources, resource templates and prompts carry inline SVG data-URI icons for their scope category, so clients never fetch third-party URLs. |
| Elicitation | Form mode (build confirmation, missing prompt arguments, continuing a multi-page task) and URL mode (Steamworks hand-off). |

Roots and sampling are deprecated in 2026-07-28. They are used only where they are the natural fit (export destinations and review summaries), and each has a fallback.

## Configuration

Settings come from the defaults, then the `--config` file (`KEY=VALUE` lines), then environment variables, then `--set KEY=VALUE`. Later sources win. `steam-mcp --help` lists every key. The config file is watched, and these settings reload without a restart:

- TTL classes
- `STEAM_MCP_DISABLED_TOOLS` (names or `prefix*`)
- `STEAM_MCP_ENABLE_WRITES`
- page size
- default country and language
- log level

A reload that changes the visible tool or prompt list sends `list_changed` notifications to subscribers.

When you run more than one replica behind a load balancer:

- Set the same `STEAM_MCP_STATE_SECRET` on every replica so MRTR `requestState` verifies anywhere.
- Route `tasks/*` by `Mcp-Name` (the task id). Tasks are kept in memory by the replica that created them.

See `examples/steam-mcp.conf` for an annotated configuration.

## Design records and references

- [`docs/adr/`](docs/adr/README.md) — architecture decision records (dependencies, protocol revision, caching, concurrency, MRTR state, auth, writes, tasks).
- [`ref/`](ref/README.md) — pinned copies of the MCP 2026-07-28 spec and schema and the Tasks extension, a Steam endpoint inventory with verification status, and a requirement-to-code compliance map.

## Live verification

The normal test suite never contacts Steam. To verify against the real APIs with your own keys:

```sh
STEAM_API_KEY=… STEAM_PUBLISHER_KEY=… STEAM_FINANCIAL_KEY=… \
STEAM_LIVE_APPID=<one of your apps> STEAM_LIVE_STEAMID=<a public profile> STEAM_LIVE_DATE=<a sales date> \
  cargo test --release --test live -- --ignored --nocapture
```

The run calls every read-only tool whose credential is set and writes `target/live-report.md`. It fails only on endpoint-level problems (an unknown method or version, or rejected parameter names). Data-level outcomes, such as private profiles or a key without permission for an app, are reported but do not fail the run.

## Development

```sh
scripts/check.sh         # fmt, clippy, tests and release build (the project's quality gate)
scripts/check.sh --fuzz  # also run an extended multi-seed fuzz pass
```

The integration tests start a mock Steam server and drive the dispatcher, the HTTP transport (over real sockets) and the stdio binary (as a subprocess). They cover:

- caching and coalescing, and stale-if-error;
- MRTR flows (form and URL elicitation, sampling, roots), tasks and subscriptions;
- auth: static tokens, HS256, RS256 through a fetched JWKS, and introspection;
- header validation, slow-drip clients and TLS disconnects;
- config hot reload;
- shutdown on SIGTERM and on stdin EOF.

`tests/fuzz.rs` runs seeded fuzzing of the JSON, HTTP, URI-template, base64 and regex code, sealed tokens, schema validation and the dispatcher. Set `FUZZ_SEED` and `FUZZ_ITERS` to explore further:

```sh
FUZZ_SEED=42 FUZZ_ITERS=200000 cargo test --release --test fuzz
```

Fuzzing found and fixed two issues:

- Sealed tokens accepted non-canonical base64. Tokens are now strictly canonical.
- Duplicate JSON object keys produced maps that equality and serialization disagreed on. The parser now keeps only the last value for each key.

## License

MIT
