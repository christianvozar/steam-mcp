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

The **Verified** column records the evidence, from a live run on 2026-11-22 from the build sandbox:

- 🟢 **live** — called live without a key; the response parsed and the tool returned correct data.
- 🟡 **exists** — probed live without a key. Steam answered "parameter missing" (400), "key required" (401/403) or "use POST" (405), which proves the method and version exist. Control probes for a fake method and a wrong version returned 404 HTML. Response *shape* and parameter names past the key check still need a keyed run.
- 🔴 **open** — could not be verified from the sandbox.

To close the gaps, run `cargo test --release --test live -- --ignored --nocapture` with real keys (see `tests/live.rs`). It exercises every tool and writes `target/live-report.md`.

The live run found four bugs, now fixed:

1. `IGameInventory/GetItemDefMeta` does not exist. The method is `IInventoryService/GetItemDefMeta/v1`.
2. Partner `ISteamApps/GetServerList` requires `appid`.
3. `ISteamApps/GetAppList/v2` was retired ("Method not found"). Catalog search now requires a key and uses `IStoreService/GetAppList`.
4. The storefront answers `/appreviews` from the sandbox network with a 302 redirect to its home page. Redirects are now reported as transient errors (stale data is served), not parsed.

The server also now reports any non-JSON 404 from an API host as "method or version is wrong or retired", so version mistakes surface immediately instead of looking like "no data".

Response parsing is defensive everywhere: missing fields become `null`, and VDF-style `{name: {...}}` maps are flattened into arrays.

## Public Web API

| Endpoint | Host | Key | Cache | Used by | Verified |
| --- | --- | --- | --- | --- | --- |
| `IStoreService/GetAppList/v1` (paged by `last_appid`, `max_results` 50000) | api | public | static | catalog, `steam_search_apps`, completion | 🟡 exists (403 without key) |
| `ISteamApps/GetAppList/v2` (fallback when no key) | api | none | static | catalog | ❌ retired upstream; fallback removed |
| `ISteamUserStats/GetNumberOfCurrentPlayers/v1` | api | none | realtime | `steam_get_current_players`, overview | 🟢 live |
| `ISteamNews/GetNewsForApp/v2` | api | none | short | `steam_get_app_news`, overview | 🟢 live |
| `ISteamUserStats/GetGlobalAchievementPercentagesForApp/v2` (`gameid`) | api | none | long | achievements, overview | 🟢 live |
| `ISteamUserStats/GetSchemaForGame/v2` | api | public | static | `steam_get_game_schema` | 🟡 exists |
| `ISteamUserStats/GetGlobalStatsForGame/v1` (`name[i]`, `count`) | api | none | long | `steam_get_global_stats` | 🟢 live |
| `ISteamUserStats/GetPlayerAchievements/v1` | api | public | short | `steam_get_player_achievements` | 🟡 exists |
| `ISteamUserStats/GetUserStatsForGame/v2` | api | public | short | `steam_get_user_stats_for_game` | 🟡 exists |
| `ISteamUser/GetPlayerSummaries/v2` (≤100 ids) | api | public | medium | summaries, profile | 🟡 exists |
| `ISteamUser/ResolveVanityURL/v1` | api | public | static | `steam_resolve_vanity_url` | 🟡 exists |
| `ISteamUser/GetFriendList/v1` | api | public | medium | `steam_get_friend_list` | 🟡 exists |
| `ISteamUser/GetPlayerBans/v1` | api | public | medium | bans, profile | 🟡 exists |
| `IPlayerService/GetOwnedGames/v1` (`appids_filter[i]`) | api | public | medium | `steam_get_owned_games` | 🟡 exists |
| `IPlayerService/GetRecentlyPlayedGames/v1` | api | public | short | recent, profile | 🟡 exists |
| `IPlayerService/GetSteamLevel/v1` | api | public | medium | level, profile | 🟡 exists |
| `IPlayerService/GetBadges/v1` | api | public | medium | `steam_get_badges` | 🟡 exists |
| `ISteamChartsService/GetMostPlayedGames/v1` | api | none | short | `steam_get_most_played_games` | 🟢 live |
| `ISteamRemoteStorage/GetPublishedFileDetails/v1` (POST form) | api | none | medium | `steam_get_workshop_item_details` | 🟢 live |
| `IPublishedFileService/QueryFiles/v1` (`query_type` enum, `requiredtags[i]`) | api | public | short | `steam_query_workshop_files` | 🟡 exists |
| `ISteamWebAPIUtil/GetSupportedAPIList/v1` | api / partner | optional / publisher | static | `steam_get_supported_api_list` | 🟢 live |
| `IWishlistService/GetWishlist/v1` | api | optional | medium | `steam_get_wishlist` | 🟢 live (works without a key) |
| `ISteamApps/GetServersAtAddress/v1` | api | none | short | `steam_get_servers_at_address` | 🟢 live |
| `ISteamApps/UpToDateCheck/v1` | api | none | medium | `steam_check_app_up_to_date` | 🟢 live |
| `IGameServersService/GetServerList/v1` | api | public | realtime | `steam_get_game_servers` | 🟡 exists |
| `IGameInventory/GetItemDefArchive/v1` (`digest`) | api | none | static (immutable per digest) | `steam_partner_get_item_definitions` | 🟡 exists |
| any `I*/*/v*` GET | api / partner | optional / publisher | caller-chosen | `steam_api_call` | n/a |

## Storefront (unofficial but stable JSON endpoints)

| Endpoint | Key | Cache | Used by | Verified |
| --- | --- | --- | --- | --- |
| `/api/appdetails?appids=&cc=&l=&filters=` | none | medium | details, overview, compare | 🟢 live |
| `/api/packagedetails?packageids=&cc=` | none | medium | `steam_get_package_details` | 🟢 live |
| `/api/featuredcategories?cc=&l=` | none | short | `steam_get_store_featured` | 🟢 live |
| `/appreviews/{appid}?json=1&filter=&language=&review_type=&purchase_type=&num_per_page=&cursor=&day_range=` | none | short | reviews, summaries, overview | 🔴 open: 302 to store home from the sandbox network |

Storefront notes:

- The storefront is rate-limited more aggressively than the Web API (roughly 200 requests per 5 minutes per IP). The long TTLs exist partly for this reason.
- `appdetails` accepts one app per call unless `filters=price_overview` is set.
- With `num_per_page=0`, `appreviews` returns only `query_summary`.
- Review cursors contain `+` and `/` and must be percent-encoded. `build_query` does this.

## Store services and charts (added 2026-11-22)

These protobuf-backed services take a JSON `input_json` parameter. Every keyless one below was called live with real input and returned correct data.

| Endpoint | Host | Key | Cache | Used by | Verified |
| --- | --- | --- | --- | --- | --- |
| `IStoreBrowseService/GetItems/v1` | api | none | medium (short for review fallback) | `steam_get_store_items`, review-summary fallback | 🟢 live |
| `IStoreQueryService/SearchSuggestions/v1` | api | none | medium | `steam_search_store`, keyless `steam_search_apps` | 🟢 live |
| `IStoreService/GetTagList/v1` | api | none | static | tag names | 🟢 live |
| `IStoreService/GetMostPopularTags/v1` | api | none | static | `steam_get_store_tags` | 🟢 live |
| `IStoreTopSellersService/GetWeeklyTopSellers/v1` | api | none | short | `steam_get_weekly_top_sellers` | 🟢 live |
| `ICommunityService/GetApps/v1` | api | none | static | app-name resolution for charts and lists | 🟢 live |
| `ISteamChartsService/GetGamesByConcurrentPlayers/v1` | api | none | realtime | `steam_get_top_concurrent_games` | 🟢 live |
| `ISteamChartsService/GetMostPlayedSteamDeckGames/v1` | api | none | short | `steam_get_top_steam_deck_games` | 🟢 live |
| `ISteamChartsService/GetTopReleasesPages/v1` | api | none | long | `steam_get_top_releases` | 🟢 live |
| `ISteamRemoteStorage/GetCollectionDetails/v1` (POST) | api | none | medium | `steam_get_workshop_collection` | 🟢 live |
| `IGCVersion_<appid>/GetClientVersion|GetServerVersion/v1` | api | none | realtime | `steam_get_gc_version` (440, 570, 730, 583950, 1046930, 1269260, 1422450) | 🟢 live |
| `ISteamApps/GetSDRConfig/v1` | api | none | medium | `steam_get_sdr_config` | 🟢 live |
| `IContentServerDirectoryService/GetDepotPatchInfo/v1` | api | none | static (immutable per manifest pair) | `steam_get_depot_patch_info` | 🟡 exists (answered for fake manifests) |
| `IWishlistService/GetWishlistItemCount/v1` | api | none | medium | `steam_get_wishlist_count` | 🟢 live |
| `IStoreService/GetGamesFollowed/v1` | api | none | medium | `steam_get_followed_games` | 🟢 live |
| `IPublishedFileService/GetUserVoteSummary/v1` | api | public | short | `steam_get_workshop_vote_summary` | 🟡 exists; needs a key despite appearing in the keyless list |
| `IPlayerService/GetSingleGamePlaytime/v1` | api | public | short | `steam_get_single_game_playtime` | 🟡 exists |
| `IPlayerService/IsPlayingSharedGame/v1` (`appid_playing`) | api | public | volatile | `steam_is_playing_shared_game` | 🟡 exists |
| `IPlayerService/GetCommunityBadgeProgress/v1` | api | public | medium | `steam_get_badge_progress` | 🟡 exists |
| `ISteamUser/GetUserGroupList/v1` | api | public | medium | `steam_get_user_group_list` | 🟡 exists |
| `ISteamEconomy/GetAssetClassInfo/v1` (`classid<i>`) | api | public | long | `steam_get_asset_class_info` | 🟡 exists |
| `IPartnerStoreBrowseService/GetItems/v1` | partner | publisher | medium | `steam_partner_get_store_items` | 🟡 exists |
| `IWorkshopService/GetFinalizedContributors/v1` | partner | publisher | medium | `steam_partner_get_workshop_contributors` | 🟡 exists |

## Extended read coverage (added 2026-11-22)

Group 1 of the parity plan: every remaining read-only method. 🟢 rows were called live without a key. 🟡 rows were confirmed to exist by probing (403/400 instead of the HTML 404 for unknown methods), and where possible their required parameters were taken from Steam's "Required parameter" errors. Keyed rows can only be fully verified with real keys through `tests/live.rs`.

| Endpoint | Host | Key | Cache | Used by | Verified |
| --- | --- | --- | --- | --- | --- |
| `IPlayerService/GetProfileItemsEquipped/v1` (+ GetAnimatedAvatar, GetAvatarFrame, GetMiniProfileBackground, GetProfileBackground) | api | none | medium | `steam_get_profile_items` | 🟢 live |
| `IStoreBrowseService/GetStoreCategories/v1` | api | none | static | `steam_get_store_categories` | 🟢 live (72 categories) |
| `IStoreQueryService/Query/v1` (`query.filters.tagids_must_match`, `type_filters`, `price_filters`) | api | none | short | `steam_query_store` | 🟢 live |
| `IWishlistService/GetWishlistSortedFiltered/v1` | api | none | medium | `steam_get_wishlist_detailed` | 🟡 answers `{}` for a private wishlist |
| `IStoreBrowseService/GetDLCForApps/v1` | api | public | medium | `steam_get_dlc_for_apps` | 🟡 exists |
| `IPlayerService/GetTopAchievementsForGames/v1` | api | public | short | `steam_get_top_achievements_for_games` | 🟡 exists |
| `IPublishedFileService/GetDetails, GetUserFiles, GetSubSectionData/v1` | api | public | medium / short | workshop tools | 🟡 exist |
| `ISteamRemoteStorage/GetUGCFileDetails/v1` (`ugcid`, `appid`) | api | public | long | `steam_get_ugc_file_details` | 🟡 required params from Steam errors |
| `IGameServersService/GetAccountList, GetAccountPublicInfo, QueryLoginToken, GetServerSteamIDsByIP, GetServerIPsBySteamID/v1` | api | public | volatile–medium (QueryLoginToken uncached) | game-server tools | 🟡 exist |
| `ISteamGameServerStats/GetGameServerPlayerStatsForGame/v1` | api | public | short | `steam_get_game_server_player_stats` | 🟡 checks the key first |
| `ISteamNews/GetNewsForAppAuthed/v2` | partner | publisher | short | `steam_partner_get_news_authed` | 🟡 checks the key first |
| `IWorkshopService/GetItemDailyRevenue/v1` | partner | publisher (financial scope) | financial | `steam_partner_get_workshop_item_revenue` | 🟡 exists |
| `ISteamEconomy/GetMarketPrices` (`appid`), CanTrade (`appid`, `steamid`, `targetid`)/v1 | partner | publisher | short / volatile | market tools | 🟡 required params from Steam errors |
| `IEconMarketService/GetPopular, GetAssetID, GetMarketEligibility/v1` | partner | publisher | short–volatile | market tools | 🟡 exist |
| `IInventoryService/GetItemDefs, GetPriceSheet, GetQuantity/v1` | partner | publisher | medium / volatile | inventory tools | 🟡 exist |
| `IGameInventory/GetUserHistory` (`appid`, `steamid`, `contextid`, `starttime`, `endtime`), GetHistoryCommandDetails (`command`, `arguments`), SupportGetAssetHistory (`assetid`)/v1 | partner | publisher | volatile / short | inventory history tools | 🟡 required params from Steam errors |
| `IGameNotificationsService/EnumerateSessionsForApp, GetSessionDetailsForApp/v1` | partner | publisher | volatile | notification tools | 🟡 exist |
| `ILobbyMatchmakingService/GetLobbyData/v1` | partner | publisher | volatile | `steam_partner_get_lobby_data` | 🟡 exists |
| `ISteamMicroTxn[Sandbox]/GetUserInfo/v2, ISteamMicroTxn/GetUserAgreementInfo/v2` (`steamid`, `appid`) | partner | publisher | volatile | microtransaction tools | 🟡 required params from Steam errors |
| `ICheatReportingService/RequestVacStatusForUser/v1` (POST) | partner | publisher | never cached | `steam_partner_get_vac_status` | 🟡 exists |

## Partner Web API (publisher key, `partner.steam-api.com`)

| Endpoint | Cache | Used by | Verified |
| --- | --- | --- | --- |
| `ISteamApps/GetPartnerAppListForWebAPIKey/v2` (`type_filter`) | medium | partner list, resources, completion | 🟡 exists |
| `ISteamApps/GetAppBetas/v1` | partner_live | betas, write-confirmation prompt | 🟡 exists |
| `ISteamApps/GetAppBuilds/v1` | partner_live | `steam_partner_get_app_builds` | 🟡 exists |
| `ISteamApps/GetAppDepotVersions/v1` | partner_live | depots | 🟡 exists |
| `ISteamApps/GetPlayersBanned/v1` | short | banned players | 🟡 exists |
| `ISteamApps/GetCheatingReports/v1` | volatile | cheating reports | 🟡 exists (all parameter names accepted) |
| `ISteamApps/GetServerList/v1` | volatile | partner server list | 🟡 exists — **`appid` required** (fixed) |
| `ISteamApps/SetAppBuildLive/v2` (**POST, write**) | never cached | `steam_partner_set_app_build_live` | 🟡 exists (POST-only confirmed) |
| `ISteamUser/CheckAppOwnership/v2` | volatile | ownership check | 🟡 exists (v2 confirmed) |
| `ISteamUser/GetPublisherAppOwnership/v3` | volatile | publisher ownership | 🟡 exists (v3 confirmed) |
| `ISteamUser/GetPublisherAppOwnershipChanges/v1` | volatile | ownership changes | 🟡 exists |
| `ISteamUser/GetAppPriceInfo/v1` | short | price info | 🟡 exists |
| `ISteamUser/GetDeletedSteamIDs/v1` | medium | deleted ids | 🟡 exists |
| `ISteamUserAuth/AuthenticateUserTicket/v1` | never cached | ticket validation | 🟡 exists |
| `ISteamLeaderboards/GetLeaderboardsForGame/v2` | medium | leaderboards | 🟡 exists |
| `ISteamLeaderboards/GetLeaderboardEntries/v1` (`datarequest` = `RequestGlobal` / `RequestAroundUser` / `RequestFriends`) | short | entries | 🟡 exists |
| `ISteamMicroTxn[Sandbox]/GetReport/v5` | short | microtransaction report | 🟡 exists |
| `ISteamMicroTxn[Sandbox]/QueryTxn/v3` | volatile | query transaction | 🟡 exists |
| `IInventoryService/GetItemDefMeta/v1` | medium | item definitions (digest) | 🟡 exists as `IInventoryService` (fixed) |
| `IInventoryService/GetInventory/v1` | volatile | user inventory | 🟡 exists |
| `ISteamEconomy/GetAssetPrices/v1` | medium | asset prices | 🟡 exists |

## Financial API (Financial API group key, partner host)

| Endpoint | Cache | Used by | Verified |
| --- | --- | --- | --- |
| `IPartnerFinancialsService/GetChangedDatesForPartner/v001` (`highwatermark`) | short | changed dates, date completion | 🟡 exists (403 without key) |
| `IPartnerFinancialsService/GetDetailedSales/v001` (`date`, `highwatermark_id`; paginate on `max_id`) | financial | detailed sales, sales summary | 🟡 exists (403 without key) |

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
