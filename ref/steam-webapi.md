# Steam endpoint inventory

This file lists every upstream call the server makes. Hosts:

- `api` — `https://api.steampowered.com`
- `partner` — `https://partner.steam-api.com`
- `store` — `https://store.steampowered.com`

Base URLs can be overridden with `STEAM_MCP_*_BASE`.

The **Key** column says which credential the call sends:

- `none` — no key is sent.
- `optional` — the user key is sent if one is configured.
- `public` — needs a user key or a publisher key.
- `publisher` — needs a Steamworks publisher key.
- `financial` — needs a Financial API group key.

The **Cache** column is the TTL class from `src/config.rs` (`TTL_CLASSES`).

The **Verified** column records how much we know about the endpoint:

- ✅ **mock** — exercised end to end against the mock in `tests/common/mod.rs`. The response *shape* follows the documented or observed Steam format.
- ⚠️ **docs** — the method name, version and parameters come from Steamworks documentation as remembered while building this server. These have **not** been called against live Steam, because the build sandbox blocked Steam hosts. Verify them with a real key before relying on them. `steam_api_call` can reach any other version if one is wrong.

Response parsing is defensive everywhere: missing fields become `null`, and VDF-style `{name: {...}}` maps are flattened into arrays.

## Public Web API

| Endpoint | Host | Key | Cache | Used by | Verified |
| --- | --- | --- | --- | --- | --- |
| `IStoreService/GetAppList/v1` (paged by `last_appid`, `max_results` 50000) | api | public | static | catalog, `steam_search_apps`, completion | ✅ mock |
| `ISteamApps/GetAppList/v2` (fallback when no key) | api | none | static | catalog | ✅ mock |
| `ISteamUserStats/GetNumberOfCurrentPlayers/v1` | api | none | realtime | `steam_get_current_players`, overview | ✅ mock |
| `ISteamNews/GetNewsForApp/v2` | api | none | short | `steam_get_app_news`, overview | ✅ mock |
| `ISteamUserStats/GetGlobalAchievementPercentagesForApp/v2` (`gameid`) | api | none | long | achievements, overview | ✅ mock |
| `ISteamUserStats/GetSchemaForGame/v2` | api | public | static | `steam_get_game_schema` | ⚠️ docs |
| `ISteamUserStats/GetGlobalStatsForGame/v1` (`name[i]`, `count`) | api | none | long | `steam_get_global_stats` | ⚠️ docs |
| `ISteamUserStats/GetPlayerAchievements/v1` | api | public | short | `steam_get_player_achievements` | ⚠️ docs |
| `ISteamUserStats/GetUserStatsForGame/v2` | api | public | short | `steam_get_user_stats_for_game` | ⚠️ docs |
| `ISteamUser/GetPlayerSummaries/v2` (≤100 ids) | api | public | medium | summaries, profile | ✅ mock |
| `ISteamUser/ResolveVanityURL/v1` | api | public | static | `steam_resolve_vanity_url` | ⚠️ docs |
| `ISteamUser/GetFriendList/v1` | api | public | medium | `steam_get_friend_list` | ⚠️ docs |
| `ISteamUser/GetPlayerBans/v1` | api | public | medium | bans, profile | ⚠️ docs |
| `IPlayerService/GetOwnedGames/v1` (`appids_filter[i]`) | api | public | medium | `steam_get_owned_games` | ⚠️ docs |
| `IPlayerService/GetRecentlyPlayedGames/v1` | api | public | short | recent, profile | ⚠️ docs |
| `IPlayerService/GetSteamLevel/v1` | api | public | medium | level, profile | ⚠️ docs |
| `IPlayerService/GetBadges/v1` | api | public | medium | `steam_get_badges` | ⚠️ docs |
| `ISteamChartsService/GetMostPlayedGames/v1` | api | none | short | `steam_get_most_played_games` | ✅ mock |
| `ISteamRemoteStorage/GetPublishedFileDetails/v1` (POST form) | api | none | medium | `steam_get_workshop_item_details` | ⚠️ docs |
| `IPublishedFileService/QueryFiles/v1` (`query_type` enum, `requiredtags[i]`) | api | public | short | `steam_query_workshop_files` | ⚠️ docs |
| `ISteamWebAPIUtil/GetSupportedAPIList/v1` | api / partner | optional / publisher | static | `steam_get_supported_api_list` | ✅ mock |
| `IWishlistService/GetWishlist/v1` | api | optional | medium | `steam_get_wishlist` | ⚠️ docs |
| `ISteamApps/GetServersAtAddress/v1` | api | none | short | `steam_get_servers_at_address` | ⚠️ docs |
| `ISteamApps/UpToDateCheck/v1` | api | none | medium | `steam_check_app_up_to_date` | ⚠️ docs |
| `IGameServersService/GetServerList/v1` | api | public | realtime | `steam_get_game_servers` | ⚠️ docs |
| `IGameInventory/GetItemDefArchive/v1` (`digest`) | api | none | static (immutable per digest) | `steam_partner_get_item_definitions` | ⚠️ docs |
| any `I*/*/v*` GET | api / partner | optional / publisher | caller-chosen | `steam_api_call` | n/a |

## Storefront (unofficial but stable JSON endpoints)

| Endpoint | Key | Cache | Used by | Verified |
| --- | --- | --- | --- | --- |
| `/api/appdetails?appids=&cc=&l=&filters=` | none | medium | details, overview, compare | ✅ mock |
| `/api/packagedetails?packageids=&cc=` | none | medium | `steam_get_package_details` | ⚠️ docs |
| `/api/featuredcategories?cc=&l=` | none | short | `steam_get_store_featured` | ✅ mock |
| `/appreviews/{appid}?json=1&filter=&language=&review_type=&purchase_type=&num_per_page=&cursor=&day_range=` | none | short | reviews, summaries, overview | ✅ mock |

Storefront notes:

- The storefront is rate-limited more aggressively than the Web API (roughly 200 requests per 5 minutes per IP). The long TTLs exist partly for this reason.
- `appdetails` accepts one app per call unless `filters=price_overview` is set.
- With `num_per_page=0`, `appreviews` returns only `query_summary`.
- Review cursors contain `+` and `/` and must be percent-encoded. `build_query` does this.

## Partner Web API (publisher key, `partner.steam-api.com`)

| Endpoint | Cache | Used by | Verified |
| --- | --- | --- | --- |
| `ISteamApps/GetPartnerAppListForWebAPIKey/v2` (`type_filter`) | medium | partner list, resources, completion | ✅ mock |
| `ISteamApps/GetAppBetas/v1` | partner_live | betas, write-confirmation prompt | ✅ mock |
| `ISteamApps/GetAppBuilds/v1` | partner_live | `steam_partner_get_app_builds` | ✅ mock |
| `ISteamApps/GetAppDepotVersions/v1` | partner_live | depots | ⚠️ docs |
| `ISteamApps/GetPlayersBanned/v1` | short | banned players | ⚠️ docs |
| `ISteamApps/GetCheatingReports/v1` | volatile | cheating reports | ⚠️ docs |
| `ISteamApps/GetServerList/v1` | volatile | partner server list | ⚠️ docs |
| `ISteamApps/SetAppBuildLive/v2` (**POST, write**) | never cached | `steam_partner_set_app_build_live` | ✅ mock (version ⚠️ docs) |
| `ISteamUser/CheckAppOwnership/v2` | volatile | ownership check | ⚠️ docs (version may be newer) |
| `ISteamUser/GetPublisherAppOwnership/v3` | volatile | publisher ownership | ⚠️ docs (version uncertain) |
| `ISteamUser/GetPublisherAppOwnershipChanges/v1` | volatile | ownership changes | ⚠️ docs |
| `ISteamUser/GetAppPriceInfo/v1` | short | price info | ⚠️ docs |
| `ISteamUser/GetDeletedSteamIDs/v1` | medium | deleted ids | ⚠️ docs |
| `ISteamUserAuth/AuthenticateUserTicket/v1` | never cached | ticket validation | ⚠️ docs |
| `ISteamLeaderboards/GetLeaderboardsForGame/v2` | medium | leaderboards | ⚠️ docs |
| `ISteamLeaderboards/GetLeaderboardEntries/v1` (`datarequest` = `RequestGlobal` / `RequestAroundUser` / `RequestFriends`) | short | entries | ⚠️ docs |
| `ISteamMicroTxn[Sandbox]/GetReport/v5` | short | microtransaction report | ⚠️ docs |
| `ISteamMicroTxn[Sandbox]/QueryTxn/v3` | volatile | query transaction | ⚠️ docs |
| `IGameInventory/GetItemDefMeta/v1` | medium | item definitions (digest) | ⚠️ docs |
| `IInventoryService/GetInventory/v1` | volatile | user inventory | ⚠️ docs |
| `ISteamEconomy/GetAssetPrices/v1` | medium | asset prices | ⚠️ docs |

## Financial API (Financial API group key, partner host)

| Endpoint | Cache | Used by | Verified |
| --- | --- | --- | --- |
| `IPartnerFinancialsService/GetChangedDatesForPartner/v001` (`highwatermark`) | short | changed dates, date completion | ✅ mock |
| `IPartnerFinancialsService/GetDetailedSales/v001` (`date`, `highwatermark_id`; paginate on `max_id`) | financial | detailed sales, sales summary | ✅ mock |

Sales rows are aggregated defensively. Every numeric field (numbers or numeric strings) whose name ends in `_usd` or contains `units` is summed. Names are joined from the `*_info` lookup tables in the response (`app_info`, `package_info`, `bundle_info`, `country_info`).

## Error mapping (`src/steam/mod.rs`)

| Upstream | Classification | Cached? | Retried? |
| --- | --- | --- | --- |
| 2xx JSON | value (or negative, via a per-call predicate such as `success:false`) | yes | – |
| 2xx empty body / 404 | negative | briefly | no |
| 400 | bad request | no | no |
| 401 / 403 | unauthorized / forbidden | no | no |
| 429 / 5xx | transient | stale-if-error | yes, with backoff and `Retry-After` |
| network / TLS | transient | stale-if-error | yes |
| non-JSON 2xx | upstream error | stale-if-error | yes |
