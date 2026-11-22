//! Storefront services, charts, workshop collections, infrastructure and
//! extended player/partner tools.
//!
//! Most of these use Steam's protobuf-backed "service" interfaces, which take a
//! JSON-encoded `input_json` query parameter. The keyless ones were verified
//! live on 2026-11-22 (see `ref/steam-webapi.md`).

use std::collections::BTreeMap;

use super::public::{steamid_arg, store_url};
use super::*;
use crate::mcp::context::scopes;
use crate::steam::{Call, Host, KeyKind};
use crate::try_steam;

macro_rules! tryo {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(o) => return o,
        }
    };
}

fn get(env: &ToolEnv, call: Call, args: &Map) -> Result<Lookup, SteamError> {
    env.steam().call(&call, force(args))
}

/// A service call carrying a JSON request body in `input_json`.
fn service(host: Host, path: &str, key: KeyKind, ttl: &'static str, input: &Value) -> Call {
    Call::get(host, path, key, ttl).param("input_json", input.canonical().to_string())
}

/// The storefront `context` object (language and uppercase country code).
pub fn store_context(cc: &str, language: &str) -> Value {
    obj! { "language" => language, "country_code" => cc.to_ascii_uppercase() }
}

fn ids_arg(args: &Map, key: &str, min: usize, max: usize) -> Result<Vec<u64>, ToolOutcome> {
    let ids: Vec<u64> = arg_list(args, key)
        .iter()
        .filter_map(|s| s.parse().ok())
        .filter(|v| *v > 0)
        .collect();
    if ids.len() < min || ids.len() > max {
        return Err(tool_error(format!(
            "'{key}' must contain {min}–{max} positive numeric IDs."
        )));
    }
    Ok(ids)
}

// ---------------------------------------------------------------------------
// Shared lookups: app names, tag names, store items.
// ---------------------------------------------------------------------------

/// Resolves app names: the cached catalog first, then ICommunityService/GetApps
/// (keyless, batched, cached for a day). Unknown IDs are omitted.
pub fn app_names(env: &ToolEnv, ids: &[u64]) -> BTreeMap<u64, String> {
    let mut out = BTreeMap::new();
    if let Some(index) = env.steam().catalog_if_cached() {
        for id in ids {
            if let Ok(p) = index.binary_search_by_key(id, |e| e.appid) {
                out.insert(*id, index[p].name.clone());
            }
        }
    }
    let mut missing: Vec<u64> = ids
        .iter()
        .filter(|i| !out.contains_key(i))
        .cloned()
        .collect();
    missing.sort_unstable();
    missing.dedup();
    for chunk in missing.chunks(100) {
        let call = service(
            Host::Public,
            "/ICommunityService/GetApps/v1/",
            KeyKind::None,
            "static",
            &obj! { "appids" => chunk.iter().copied().map(Value::from).collect::<Vec<_>>() },
        );
        if let Ok(l) = env.steam().call(&call, false) {
            for a in l
                .value
                .at(&["response", "apps"])
                .as_array()
                .into_iter()
                .flatten()
            {
                if let (Some(id), Some(name)) = (a["appid"].as_u64(), a["name"].as_str()) {
                    out.insert(id, name.to_string());
                }
            }
        }
    }
    out
}

/// Store tag id → name (IStoreService/GetTagList, cached `static`).
pub fn tag_names(env: &ToolEnv, language: &str) -> BTreeMap<u64, String> {
    let call = service(
        Host::Public,
        "/IStoreService/GetTagList/v1/",
        KeyKind::None,
        "static",
        &obj! { "language" => language },
    );
    env.steam()
        .call(&call, false)
        .ok()
        .and_then(|l| {
            l.value.at(&["response", "tags"]).as_array().map(|tags| {
                tags.iter()
                    .filter_map(|t| Some((t["tagid"].as_u64()?, t["name"].as_str()?.to_string())))
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// Shapes one `store_items` entry from IStoreBrowseService/GetItems.
pub fn shape_store_item(it: &Value, tags: &BTreeMap<u64, String>) -> Value {
    let appid = it["appid"].as_u64().unwrap_or(0);
    if it["success"].as_i64() != Some(1) {
        return obj! { "appid" => it["id"].clone(), "error" => "not found or not visible in this country" };
    }
    let names = |v: &Value| -> Vec<Value> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x["name"].as_str().map(Value::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let bpo = &it["best_purchase_option"];
    let reviews = &it["reviews"]["summary_filtered"];
    let p = &it["platforms"];
    let deck = match p["steam_deck_compat_category"].as_i64() {
        Some(3) => "verified",
        Some(2) => "playable",
        Some(1) => "unsupported",
        Some(0) => "unknown",
        _ => "n/a",
    };
    obj! {
        "appid" => appid,
        "name" => it["name"].clone(),
        "isFree" => it["is_free"].as_bool().unwrap_or(false),
        "isEarlyAccess" => it["is_early_access"].as_bool().unwrap_or(false),
        "comingSoon" => it.at(&["release", "is_coming_soon"]).clone(),
        "releaseDate" => it.at(&["release", "steam_release_date"]).as_u64().map(crate::util::time::date_from_secs),
        "price" => if bpo.is_object() { obj! {
            "final" => bpo["formatted_final_price"].clone(),
            "original" => bpo["formatted_original_price"].clone(),
            "discountPercent" => bpo["discount_pct"].clone(),
            "packageid" => bpo["packageid"].clone(),
        } } else { Value::Null },
        "reviews" => if reviews.is_object() { obj! {
            "reviewCount" => reviews["review_count"].clone(),
            "percentPositive" => reviews["percent_positive"].clone(),
            "reviewScore" => reviews["review_score"].clone(),
            "label" => reviews["review_score_label"].clone(),
        } } else { Value::Null },
        "tags" => it["tagids"].as_array().map(|ids| ids.iter().filter_map(|t| t.as_u64()).map(|t| Value::from(tags.get(&t).cloned().unwrap_or_else(|| t.to_string()))).collect::<Vec<_>>()),
        "developers" => names(&it["basic_info"]["developers"]),
        "publishers" => names(&it["basic_info"]["publishers"]),
        "franchises" => names(&it["basic_info"]["franchises"]),
        "shortDescription" => it.at(&["basic_info", "short_description"]).as_str().map(|s| crate::util::truncate(s, 400)),
        "platforms" => obj! {
            "windows" => p["windows"].as_bool().unwrap_or(false),
            "mac" => p["mac"].as_bool().unwrap_or(false),
            "linux" => p["steamos_linux"].as_bool().unwrap_or(false),
            "steamDeck" => deck,
        },
        "storeUrl" => store_url(appid),
    }
}

fn store_items_request(ids: &[u64], cc: &str, language: &str) -> Value {
    obj! {
        "ids" => ids.iter().map(|id| obj! { "appid" => *id }).collect::<Vec<_>>(),
        "context" => store_context(cc, language),
        "data_request" => obj! {
            "include_basic_info" => true,
            "include_reviews" => true,
            "include_tag_count" => 10,
            "include_release" => true,
            "include_platforms" => true,
        },
    }
}

/// Review summary from the store browse service. Used when `/appreviews` is
/// unavailable (the storefront redirects some networks).
pub fn store_review_summary(
    env: &ToolEnv,
    appid: u64,
    args: &Map,
) -> Result<(Value, Lookup), SteamError> {
    let req = store_items_request(&[appid], &cc(env, args), &lang(env, args));
    let l = get(
        env,
        service(
            Host::Public,
            "/IStoreBrowseService/GetItems/v1/",
            KeyKind::None,
            "short",
            &req,
        ),
        args,
    )?;
    let item = l.value.at(&["response", "store_items", "0"]).clone();
    let r = &item["reviews"]["summary_filtered"];
    if item["success"].as_i64() != Some(1) || !r.is_object() {
        return Err(SteamError::new(
            crate::steam::ErrorKind::NotFound,
            format!("No review summary is available for app {appid}"),
        ));
    }
    let total = r["review_count"].as_u64().unwrap_or(0);
    let pct = r["percent_positive"].as_f64();
    let positive = pct.map(|p| ((total as f64) * p / 100.0).round() as u64);
    Ok((
        obj! {
            "reviewScore" => r["review_score"].clone(),
            "reviewScoreDescription" => r["review_score_label"].clone(),
            "totalPositive" => positive,
            "totalNegative" => positive.map(|p| total.saturating_sub(p)),
            "totalReviews" => total,
            "positivePercent" => pct,
            "source" => "IStoreBrowseService/GetItems",
        },
        l,
    ))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn store_items_impl(env: &ToolEnv, args: &Map, partner: bool) -> ToolOutcome {
    let ids = tryo!(ids_arg(args, "appids", 1, 50));
    let language = lang(env, args);
    let req = store_items_request(&ids, &cc(env, args), &language);
    let call = if partner {
        service(
            Host::Partner,
            "/IPartnerStoreBrowseService/GetItems/v1/",
            KeyKind::Publisher,
            "medium",
            &req,
        )
    } else {
        service(
            Host::Public,
            "/IStoreBrowseService/GetItems/v1/",
            KeyKind::None,
            "medium",
            &req,
        )
    };
    let l = try_steam!(get(env, call, args));
    let tags = tag_names(env, &language);
    let items: Vec<Value> = l
        .value
        .at(&["response", "store_items"])
        .as_array()
        .map(|a| a.iter().map(|it| shape_store_item(it, &tags)).collect())
        .unwrap_or_default();
    let found: Vec<String> = items
        .iter()
        .filter_map(|i| {
            Some(format!(
                "{} ({}, {})",
                i["name"].as_str()?,
                i.at(&["price", "final"]).as_str().unwrap_or(
                    if i["isFree"].as_bool() == Some(true) {
                        "free"
                    } else {
                        "no price"
                    }
                ),
                i.at(&["reviews", "label"]).as_str().unwrap_or("no reviews")
            ))
        })
        .take(10)
        .collect();
    ok_result(
        format!(
            "{} of {} app(s) found: {}",
            found.len(),
            ids.len(),
            found.join("; ")
        ),
        obj! { "items" => items },
        &[&l],
    )
}

fn store_items(env: &ToolEnv, args: &Map) -> ToolOutcome {
    store_items_impl(env, args, false)
}

fn partner_store_items(env: &ToolEnv, args: &Map) -> ToolOutcome {
    store_items_impl(env, args, true)
}

/// Keyless name search via IStoreQueryService/SearchSuggestions.
pub fn store_search(
    env: &ToolEnv,
    query: &str,
    limit: usize,
    args: &Map,
) -> Result<(Vec<Value>, u64, Lookup), SteamError> {
    let req = obj! {
        "search_term" => query,
        "max_results" => limit,
        "context" => store_context(&cc(env, args), &lang(env, args)),
        "data_request" => obj! { "include_basic_info" => true },
    };
    let l = get(
        env,
        service(
            Host::Public,
            "/IStoreQueryService/SearchSuggestions/v1/",
            KeyKind::None,
            "medium",
            &req,
        ),
        args,
    )?;
    let total = l
        .value
        .at(&["response", "metadata", "total_matching_records"])
        .as_u64()
        .unwrap_or(0);
    let results = l.value.at(&["response", "store_items"]).as_array().map(|a| {
        a.iter()
            .filter(|it| it["success"].as_i64() == Some(1))
            .map(|it| {
                let appid = it["appid"].as_u64().unwrap_or(0);
                obj! { "appid" => appid, "name" => it["name"].clone(), "storeUrl" => store_url(appid) }
            })
            .collect()
    }).unwrap_or_default();
    Ok((results, total, l))
}

fn search_store(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let query = arg_str(args, "query").unwrap_or("");
    let limit = arg_u64(args, "limit").unwrap_or(10).clamp(1, 50) as usize;
    let (results, total, l) = try_steam!(store_search(env, query, limit, args));
    let top: Vec<String> = results
        .iter()
        .take(5)
        .filter_map(|r| Some(format!("{} ({})", r["name"].as_str()?, r["appid"])))
        .collect();
    ok_result(
        format!(
            "{} store match(es) for '{query}': {}",
            total,
            top.join(", ")
        ),
        obj! { "query" => query, "totalMatches" => total, "results" => results },
        &[&l],
    )
}

fn store_tags(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let language = lang(env, args);
    let popular = arg_bool(args, "popular_only", false);
    let call = if popular {
        service(
            Host::Public,
            "/IStoreService/GetMostPopularTags/v1/",
            KeyKind::None,
            "static",
            &obj! { "language" => language.as_str() },
        )
    } else {
        service(
            Host::Public,
            "/IStoreService/GetTagList/v1/",
            KeyKind::None,
            "static",
            &obj! { "language" => language.as_str() },
        )
    };
    let l = try_steam!(get(env, call, args));
    let tags = l
        .value
        .at(&["response", "tags"])
        .as_array()
        .cloned()
        .unwrap_or_default();
    let sample: Vec<String> = tags
        .iter()
        .take(8)
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect();
    ok_result(
        format!(
            "{} store tag(s){}: {}",
            tags.len(),
            if popular { " by popularity" } else { "" },
            sample.join(", ")
        ),
        obj! { "language" => language, "tags" => tags },
        &[&l],
    )
}

fn weekly_top_sellers(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let cc_v = cc(env, args);
    let count = arg_u64(args, "count").unwrap_or(20).clamp(1, 100);
    let req = obj! {
        "country_code" => cc_v.to_ascii_uppercase(),
        "context" => store_context(&cc_v, &lang(env, args)),
        "page_start" => arg_u64(args, "page_start").unwrap_or(0),
        "page_count" => count,
    };
    let l = try_steam!(get(
        env,
        service(
            Host::Public,
            "/IStoreTopSellersService/GetWeeklyTopSellers/v1/",
            KeyKind::None,
            "short",
            &req
        ),
        args
    ));
    let r = &l.value["response"];
    let ranks: Vec<Value> = r["ranks"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|x| {
                    obj! {
                        "rank" => x["rank"].clone(),
                        "appid" => x["appid"].clone(),
                        "name" => x.at(&["item", "name"]).clone(),
                        "lastWeekRank" => x["last_week_rank"].clone(),
                        "consecutiveWeeks" => x["consecutive_weeks"].clone(),
                        "firstTop100" => x["first_top100"].clone(),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let top: Vec<String> = ranks
        .iter()
        .take(5)
        .filter_map(|x| Some(format!("#{} {}", x["rank"], x["name"].as_str()?)))
        .collect();
    ok_result(
        format!(
            "Weekly top sellers ({}) for the week of {}: {}",
            cc_v.to_ascii_uppercase(),
            r["start_date"]
                .as_u64()
                .map(crate::util::time::date_from_secs)
                .unwrap_or_default(),
            top.join(", ")
        ),
        obj! { "country" => cc_v.to_ascii_uppercase(), "weekStart" => r["start_date"].as_u64().map(crate::util::time::date_from_secs), "ranks" => ranks },
        &[&l],
    )
}

fn named_ranks(
    env: &ToolEnv,
    ranks: &[Value],
    limit: usize,
    fields: &[(&str, &str)],
) -> Vec<Value> {
    let ids: Vec<u64> = ranks
        .iter()
        .take(limit)
        .filter_map(|r| r["appid"].as_u64())
        .collect();
    let names = app_names(env, &ids);
    ranks
        .iter()
        .take(limit)
        .map(|r| {
            let appid = r["appid"].as_u64().unwrap_or(0);
            let mut o = obj! { "rank" => r["rank"].clone(), "appid" => appid, "name" => names.get(&appid).cloned() };
            for (from, to) in fields {
                o.set(to, r[*from].clone());
            }
            o
        })
        .collect()
}

fn top_concurrent(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let limit = arg_u64(args, "limit").unwrap_or(25).clamp(1, 100) as usize;
    let l = try_steam!(get(
        env,
        Call::get(
            Host::Public,
            "/ISteamChartsService/GetGamesByConcurrentPlayers/v1/",
            KeyKind::None,
            "realtime"
        ),
        args
    ));
    let ranks = l
        .value
        .at(&["response", "ranks"])
        .as_array()
        .cloned()
        .unwrap_or_default();
    let shaped = named_ranks(
        env,
        &ranks,
        limit,
        &[
            ("concurrent_in_game", "concurrentInGame"),
            ("peak_in_game", "peakInGame"),
        ],
    );
    let top: Vec<String> = shaped
        .iter()
        .take(5)
        .map(|r| {
            format!(
                "#{} {} ({})",
                r["rank"],
                r["name"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| r["appid"].to_string()),
                r["concurrentInGame"]
                    .as_u64()
                    .map(fmt_num)
                    .unwrap_or_default()
            )
        })
        .collect();
    ok_result(
        format!("Most concurrent players right now: {}", top.join(", ")),
        obj! { "lastUpdate" => l.value.at(&["response", "last_update"]).as_u64().map(crate::util::time::iso8601_secs), "ranks" => shaped },
        &[&l],
    )
}

fn top_deck(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let limit = arg_u64(args, "limit").unwrap_or(25).clamp(1, 100) as usize;
    let req = obj! { "top_played_period" => 1, "count" => limit };
    let l = try_steam!(get(
        env,
        service(
            Host::Public,
            "/ISteamChartsService/GetMostPlayedSteamDeckGames/v1/",
            KeyKind::None,
            "short",
            &req
        ),
        args
    ));
    let ranks = l
        .value
        .at(&["response", "ranks"])
        .as_array()
        .cloned()
        .unwrap_or_default();
    let shaped = named_ranks(
        env,
        &ranks,
        limit,
        &[("last_period_rank", "lastPeriodRank")],
    );
    let top: Vec<String> = shaped
        .iter()
        .take(5)
        .map(|r| {
            format!(
                "#{} {}",
                r["rank"],
                r["name"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| r["appid"].to_string())
            )
        })
        .collect();
    ok_result(
        format!("Most played on Steam Deck: {}", top.join(", ")),
        obj! { "ranks" => shaped },
        &[&l],
    )
}

fn top_releases(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let pages_wanted = arg_u64(args, "pages").unwrap_or(3).clamp(1, 24) as usize;
    let per_page = arg_u64(args, "per_page").unwrap_or(10).clamp(1, 50) as usize;
    let l = try_steam!(get(
        env,
        Call::get(
            Host::Public,
            "/ISteamChartsService/GetTopReleasesPages/v1/",
            KeyKind::None,
            "long"
        ),
        args
    ));
    let mut pages: Vec<Value> = l
        .value
        .at(&["response", "pages"])
        .as_array()
        .cloned()
        .unwrap_or_default();
    pages.sort_by_key(|p| std::cmp::Reverse(p["start_of_month"].as_u64().unwrap_or(0)));
    let pages: Vec<Value> = pages.into_iter().take(pages_wanted).collect();
    let ids: Vec<u64> = pages
        .iter()
        .flat_map(|p| {
            p["item_ids"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .take(per_page)
                .filter_map(|i| i["appid"].as_u64())
        })
        .collect();
    let names = app_names(env, &ids);
    let shaped: Vec<Value> = pages.iter().map(|p| obj! {
        "name" => p["name"].clone(),
        "month" => p["start_of_month"].as_u64().map(crate::util::time::date_from_secs),
        "url" => p["url_path"].as_str().map(|u| format!("https://store.steampowered.com/charts/{u}")),
        "apps" => p["item_ids"].as_array().cloned().unwrap_or_default().iter().take(per_page).filter_map(|i| i["appid"].as_u64()).map(|id| obj! { "appid" => id, "name" => names.get(&id).cloned() }).collect::<Vec<_>>(),
    }).collect();
    let latest = shaped
        .first()
        .and_then(|p| p["name"].as_str().map(String::from))
        .unwrap_or_default();
    ok_result(
        format!("{} top-release page(s); latest: {latest}", shaped.len()),
        obj! { "pages" => shaped },
        &[&l],
    )
}

fn workshop_collection(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let ids = tryo!(ids_arg(args, "collectionids", 1, 50));
    let mut call = Call::post(
        Host::Public,
        "/ISteamRemoteStorage/GetCollectionDetails/v1/",
        KeyKind::None,
        "medium",
    )
    .param("collectioncount", ids.len());
    for (i, id) in ids.iter().enumerate() {
        call = call.param(&format!("publishedfileids[{i}]"), id);
    }
    let l = try_steam!(get(env, call, args));
    let cols: Vec<Value> = l.value.at(&["response", "collectiondetails"]).as_array().map(|a| a.iter().map(|c| {
        if c["result"].as_i64() == Some(1) {
            obj! { "collectionid" => c["publishedfileid"].clone(), "items" => c["children"].as_array().map(|ch| ch.iter().map(|x| obj! { "publishedfileid" => x["publishedfileid"].clone(), "sortOrder" => x["sortorder"].clone(), "fileType" => x["filetype"].clone() }).collect::<Vec<_>>()) }
        } else {
            obj! { "collectionid" => c["publishedfileid"].clone(), "error" => format!("not a collection or not visible (result {})", c["result"]) }
        }
    }).collect()).unwrap_or_default();
    let ok = cols.iter().filter(|c| c.get("error").is_none()).count();
    ok_result(
        format!("{ok} of {} collection(s) resolved.", ids.len()),
        obj! { "collections" => cols },
        &[&l],
    )
}

/// Apps whose game coordinator publishes `IGCVersion_<appid>` (from the live API list).
const GC_APPS: &[u64] = &[440, 570, 730, 583950, 1046930, 1269260, 1422450];

fn gc_version(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    if !GC_APPS.contains(&appid) {
        return tool_error(format!(
            "App {appid} has no public game-coordinator version endpoint. Supported: {}.",
            GC_APPS
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let side = if arg_str(args, "side") == Some("client") {
        "Client"
    } else {
        "Server"
    };
    let l = try_steam!(get(
        env,
        Call::get(
            Host::Public,
            &format!("/IGCVersion_{appid}/Get{side}Version/v1/"),
            KeyKind::None,
            "realtime"
        ),
        args
    ));
    let r = l.value["result"].clone();
    if r["success"].as_bool() != Some(true) {
        return tool_error(format!(
            "The game coordinator for app {appid} did not report a {} version.",
            side.to_lowercase()
        ));
    }
    ok_result(
        format!(
            "App {appid} game coordinator {} version: active {}{}.",
            side.to_lowercase(),
            r["active_version"],
            r["min_allowed_version"]
                .as_u64()
                .map(|m| format!(", minimum allowed {m}"))
                .unwrap_or_default()
        ),
        r.with("appid", appid).with("side", side.to_lowercase()),
        &[&l],
    )
}

fn sdr_config(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let l = try_steam!(get(
        env,
        Call::get(
            Host::Public,
            "/ISteamApps/GetSDRConfig/v1/",
            KeyKind::None,
            "medium"
        )
        .param("appid", appid),
        args
    ));
    let pops = l.value["pops"].as_object().cloned().unwrap_or_default();
    if pops.is_empty() {
        return tool_error(format!(
            "App {appid} has no Steam Datagram Relay configuration."
        ));
    }
    let shaped: Vec<Value> = pops
        .iter()
        .map(|(code, p)| {
            obj! {
                "pop" => code.as_str(),
                "description" => p["desc"].clone(),
                "geo" => p["geo"].clone(),
                "tier" => p["tier"].clone(),
                "relays" => p["relays"].as_array().map(|r| r.len()).unwrap_or(0),
                "partners" => p["partners"].clone(),
            }
        })
        .collect();
    let relays: usize = shaped
        .iter()
        .filter_map(|p| p["relays"].as_u64())
        .sum::<u64>() as usize;
    let mut out =
        obj! { "appid" => appid, "revision" => l.value["revision"].clone(), "pops" => shaped };
    if arg_bool(args, "include_raw", false) {
        out.set("raw", (*l.value).clone());
    }
    ok_result(
        format!(
            "SDR config revision {} for app {appid}: {} points of presence, {relays} relays.",
            l.value["revision"],
            pops.len()
        ),
        out,
        &[&l],
    )
}

fn depot_patch(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let depot = arg_u64(args, "depotid").unwrap_or(0);
    let (Some(src), Some(dst)) = (
        arg_str(args, "source_manifestid")
            .map(String::from)
            .or_else(|| arg_u64(args, "source_manifestid").map(|v| v.to_string())),
        arg_str(args, "target_manifestid")
            .map(String::from)
            .or_else(|| arg_u64(args, "target_manifestid").map(|v| v.to_string())),
    ) else {
        return tool_error(
            "Provide 'source_manifestid' and 'target_manifestid' (from steam_partner_get_app_depot_versions or build logs).",
        );
    };
    // Patch info for a manifest pair never changes: cache it as static.
    let call = Call::get(
        Host::Public,
        "/IContentServerDirectoryService/GetDepotPatchInfo/v1/",
        KeyKind::None,
        "static",
    )
    .param("appid", appid)
    .param("depotid", depot)
    .param("source_manifestid", &src)
    .param("target_manifestid", &dst);
    let l = try_steam!(get(env, call, args));
    let r = l.value["response"].clone();
    if r.as_object().is_none_or(|m| m.is_empty()) {
        return tool_error(format!(
            "No patch is available from manifest {src} to {dst} for depot {depot} (unknown manifests or no delta)."
        ));
    }
    let size = r["size"].as_u64_lenient();
    ok_result(
        format!(
            "Patch for depot {depot} ({src} → {dst}){}.",
            size.map(|s| format!(": {} bytes", fmt_num(s)))
                .unwrap_or_default()
        ),
        r.with("appid", appid).with("depotid", depot),
        &[&l],
    )
}

fn wishlist_count(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let l = try_steam!(get(
        env,
        Call::get(
            Host::Public,
            "/IWishlistService/GetWishlistItemCount/v1/",
            KeyKind::None,
            "medium"
        )
        .param("steamid", steamid),
        args
    ));
    let count = l.value.at(&["response", "count"]).as_u64();
    match count {
        Some(n) => ok_result(
            format!("{steamid} has {n} item(s) on their wishlist."),
            obj! { "steamid" => steamid.to_string(), "count" => n },
            &[&l],
        ),
        None => ok_result(
            format!("{steamid} has no visible wishlist items (empty or private)."),
            obj! { "steamid" => steamid.to_string(), "count" => 0, "visible" => false },
            &[&l],
        ),
    }
}

fn followed_games(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let l = try_steam!(get(
        env,
        Call::get(
            Host::Public,
            "/IStoreService/GetGamesFollowed/v1/",
            KeyKind::None,
            "medium"
        )
        .param("steamid", steamid),
        args
    ));
    let ids: Vec<u64> = l
        .value
        .at(&["response", "appids"])
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_u64()).collect())
        .unwrap_or_default();
    let limit = arg_u64(args, "limit").unwrap_or(100).clamp(1, 5000) as usize;
    let names = app_names(env, &ids[..ids.len().min(limit)]);
    let apps: Vec<Value> = ids
        .iter()
        .take(limit)
        .map(|id| obj! { "appid" => *id, "name" => names.get(id).cloned() })
        .collect();
    ok_result(
        format!(
            "{steamid} follows {} game(s){}.",
            ids.len(),
            if ids.is_empty() {
                " (or the list is private)"
            } else {
                ""
            }
        ),
        obj! { "steamid" => steamid.to_string(), "count" => ids.len(), "apps" => apps },
        &[&l],
    )
}

fn vote_summary(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let ids = tryo!(ids_arg(args, "publishedfileids", 1, 100));
    let mut call = Call::get(
        Host::Public,
        "/IPublishedFileService/GetUserVoteSummary/v1/",
        KeyKind::Public,
        "short",
    );
    for (i, id) in ids.iter().enumerate() {
        call = call.param(&format!("publishedfileids[{i}]"), id);
    }
    let l = try_steam!(get(env, call, args));
    let summaries = l.value.at(&["response", "summaries"]).clone();
    ok_result(
        format!("Vote summaries for {} workshop item(s).", ids.len()),
        obj! { "summaries" => summaries },
        &[&l],
    )
}

fn single_game_playtime(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let l = try_steam!(get(
        env,
        Call::get(
            Host::Public,
            "/IPlayerService/GetSingleGamePlaytime/v1/",
            KeyKind::Public,
            "short"
        )
        .param("steamid", steamid)
        .param("appid", appid),
        args
    ));
    let r = l.value["response"].clone();
    let minutes = r["playtime_forever"].as_u64();
    ok_result(
        match minutes {
            Some(m) => format!(
                "{steamid} has played app {appid} for {:.1} hours.",
                m as f64 / 60.0
            ),
            None => format!(
                "No playtime for app {appid} is visible for {steamid} (private profile or not owned)."
            ),
        },
        r.with("steamid", steamid.to_string()).with("appid", appid),
        &[&l],
    )
}

fn shared_game(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let l = try_steam!(get(
        env,
        Call::get(
            Host::Public,
            "/IPlayerService/IsPlayingSharedGame/v1/",
            KeyKind::Public,
            "volatile"
        )
        .param("steamid", steamid)
        .param("appid_playing", appid),
        args
    ));
    let lender = l
        .value
        .at(&["response", "lender_steamid"])
        .to_param_string();
    let shared = !lender.is_empty() && lender != "0";
    ok_result(
        if shared {
            format!("{steamid} is playing app {appid} through Family Sharing from {lender}.")
        } else {
            format!("{steamid} is not playing app {appid} through Family Sharing.")
        },
        obj! { "steamid" => steamid.to_string(), "appid" => appid, "shared" => shared, "lenderSteamId" => if shared { Value::from(lender) } else { Value::Null } },
        &[&l],
    )
}

fn badge_progress(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let badge = arg_u64(args, "badgeid").unwrap_or(2);
    let l = try_steam!(get(
        env,
        Call::get(
            Host::Public,
            "/IPlayerService/GetCommunityBadgeProgress/v1/",
            KeyKind::Public,
            "medium"
        )
        .param("steamid", steamid)
        .param("badgeid", badge),
        args
    ));
    let quests = l
        .value
        .at(&["response", "quests"])
        .as_array()
        .cloned()
        .unwrap_or_default();
    let done = quests
        .iter()
        .filter(|q| q["completed"].as_bool() == Some(true))
        .count();
    ok_result(
        format!(
            "{steamid}: {done}/{} quests completed for community badge {badge}.",
            quests.len()
        ),
        obj! { "steamid" => steamid.to_string(), "badgeid" => badge, "quests" => quests },
        &[&l],
    )
}

fn user_groups(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let l = try_steam!(get(
        env,
        Call::get(
            Host::Public,
            "/ISteamUser/GetUserGroupList/v1/",
            KeyKind::Public,
            "medium"
        )
        .param("steamid", steamid),
        args
    ));
    let r = &l.value["response"];
    if r["success"].as_bool() == Some(false) {
        return tool_error(format!(
            "The group list of {steamid} is private or unavailable."
        ));
    }
    let groups = r["groups"].clone();
    ok_result(
        format!(
            "{steamid} belongs to {} group(s).",
            groups.as_array().map(|g| g.len()).unwrap_or(0)
        ),
        obj! { "steamid" => steamid.to_string(), "groups" => groups },
        &[&l],
    )
}

fn asset_class_info(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let ids = tryo!(ids_arg(args, "classids", 1, 50));
    let mut call = Call::get(
        Host::Public,
        "/ISteamEconomy/GetAssetClassInfo/v1/",
        KeyKind::Public,
        "long",
    )
    .param("appid", appid)
    .param("class_count", ids.len())
    .opt_param("language", arg_str(args, "language"));
    for (i, id) in ids.iter().enumerate() {
        call = call.param(&format!("classid{i}"), id);
    }
    let l = try_steam!(get(env, call, args));
    let mut r = l.value["result"].clone();
    if r["success"].as_bool() == Some(false) {
        return tool_error(format!(
            "Steam could not return class info: {}",
            r["error"].as_str().unwrap_or("unknown error")
        ));
    }
    if let Some(m) = r.as_object_mut() {
        m.remove("success");
    }
    ok_result(
        format!(
            "Asset class info for {} class(es) in app {appid}.",
            ids.len()
        ),
        obj! { "appid" => appid, "classes" => r },
        &[&l],
    )
}

fn workshop_contributors(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let item = arg_u64(args, "gameitemid").unwrap_or(0);
    let l = try_steam!(get(
        env,
        Call::get(
            Host::Partner,
            "/IWorkshopService/GetFinalizedContributors/v1/",
            KeyKind::Publisher,
            "medium"
        )
        .param("appid", appid)
        .param("gameitemid", item),
        args
    ));
    let contributors = l.value.at(&["response", "contributors"]).clone();
    ok_result(
        format!(
            "{} finalized contributor(s) for item {item} in app {appid}.",
            contributors.as_array().map(|c| c.len()).unwrap_or(0)
        ),
        obj! { "appid" => appid, "gameitemid" => item, "contributors" => contributors },
        &[&l],
    )
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

fn def(
    name: &'static str,
    title: &'static str,
    desc: &str,
    schema: Value,
    key: KeyKind,
    handler: Handler,
) -> ToolDef {
    ToolDef {
        name,
        title,
        description: desc.to_string(),
        input_schema: schema,
        output_schema: None,
        hints: READ,
        scope: scopes::PUBLIC,
        key,
        task: false,
        write: false,
        handler,
    }
}

fn appids_schema(max: u64) -> Value {
    obj! { "type" => "array", "items" => obj! { "type" => "integer", "minimum" => 1 }, "minItems" => 1, "maxItems" => max, "uniqueItems" => true, "description" => format!("Up to {max} app IDs.") }
}

fn manifest_id() -> Value {
    obj! { "type" => ["string", "integer"], "description" => "Depot manifest ID (64-bit; pass as a string to avoid precision loss)." }
}

pub fn tools() -> Vec<ToolDef> {
    vec![
        def(
            "steam_get_store_items",
            "Store Items (Batch)",
            "Store data for up to 50 apps in one call: price, discount, review score, tags, release date, developers, publishers and platform/Steam Deck support. Works without an API key.",
            object_schema(
                vec![
                    ("appids", appids_schema(50)),
                    ("cc", p_cc()),
                    ("language", p_lang()),
                    p_force(),
                ],
                &["appids"],
            ),
            KeyKind::None,
            store_items,
        ),
        def(
            "steam_search_store",
            "Search Store",
            "Searches the Steam store by name (works without an API key) and returns matching app IDs.",
            object_schema(
                vec![
                    (
                        "query",
                        obj! { "type" => "string", "minLength" => 1, "maxLength" => 200, "description" => "Search terms." },
                    ),
                    ("limit", p_int("Maximum results.", 1, 50, Some(10))),
                    ("cc", p_cc()),
                    ("language", p_lang()),
                    p_force(),
                ],
                &["query"],
            ),
            KeyKind::None,
            search_store,
        ),
        def(
            "steam_get_store_tags",
            "Store Tags",
            "All Steam store tags (IDs and localized names), or the most popular tags.",
            object_schema(
                vec![
                    ("language", p_lang()),
                    (
                        "popular_only",
                        p_bool("Return tags ordered by popularity.", false),
                    ),
                    p_force(),
                ],
                &[],
            ),
            KeyKind::None,
            store_tags,
        ),
        def(
            "steam_get_weekly_top_sellers",
            "Weekly Top Sellers",
            "The store's weekly top sellers for a country with rank movement and weeks on the chart.",
            object_schema(
                vec![
                    ("cc", p_cc()),
                    ("language", p_lang()),
                    ("count", p_int("Number of ranks.", 1, 100, Some(20))),
                    ("page_start", p_int("Rank offset.", 0, 1000, Some(0))),
                    p_force(),
                ],
                &[],
            ),
            KeyKind::None,
            weekly_top_sellers,
        ),
        def(
            "steam_get_top_concurrent_games",
            "Top Games by Concurrent Players",
            "Live chart of the games with the most concurrent players, with today's peak.",
            object_schema(
                vec![
                    ("limit", p_int("Number of ranks.", 1, 100, Some(25))),
                    p_force(),
                ],
                &[],
            ),
            KeyKind::None,
            top_concurrent,
        ),
        def(
            "steam_get_top_steam_deck_games",
            "Top Steam Deck Games",
            "The most played games on Steam Deck with rank movement.",
            object_schema(
                vec![
                    ("limit", p_int("Number of ranks.", 1, 100, Some(25))),
                    p_force(),
                ],
                &[],
            ),
            KeyKind::None,
            top_deck,
        ),
        def(
            "steam_get_top_releases",
            "Top Releases by Month",
            "Steam's monthly 'Top Releases' charts, newest first.",
            object_schema(
                vec![
                    ("pages", p_int("Number of months.", 1, 24, Some(3))),
                    ("per_page", p_int("Apps per month.", 1, 50, Some(10))),
                    p_force(),
                ],
                &[],
            ),
            KeyKind::None,
            top_releases,
        ),
        def(
            "steam_get_workshop_collection",
            "Workshop Collections",
            "Items contained in up to 50 Steam Workshop collections.",
            object_schema(
                vec![
                    (
                        "collectionids",
                        obj! { "type" => "array", "items" => obj! { "type" => "integer", "minimum" => 1 }, "minItems" => 1, "maxItems" => 50, "description" => "Collection IDs (publishedfileid of the collection)." },
                    ),
                    p_force(),
                ],
                &["collectionids"],
            ),
            KeyKind::None,
            workshop_collection,
        ),
        def(
            "steam_get_gc_version",
            "Game Coordinator Version",
            "Live client or server version reported by a game coordinator (TF2 440, Dota 2 570, CS2 730 and others), for patch and deploy checks.",
            object_schema(
                vec![
                    (
                        "appid",
                        obj! { "type" => "integer", "enum" => GC_APPS.to_vec().into_iter().map(Value::from).collect::<Vec<_>>(), "description" => "App with a public game coordinator.", "x-mcp-header" => "AppId" },
                    ),
                    (
                        "side",
                        p_enum("Which version.", &["server", "client"], Some("server")),
                    ),
                    p_force(),
                ],
                &["appid"],
            ),
            KeyKind::None,
            gc_version,
        ),
        def(
            "steam_get_sdr_config",
            "Steam Datagram Relay Config",
            "An app's Steam Datagram Relay network configuration: points of presence, locations, tiers and relay counts.",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("include_raw", p_bool("Include the full relay list.", false)),
                    p_force(),
                ],
                &["appid"],
            ),
            KeyKind::None,
            sdr_config,
        ),
        def(
            "steam_get_depot_patch_info",
            "Depot Patch Info",
            "Size and availability of the delta patch between two depot manifests, for estimating update download sizes. Cached permanently per manifest pair.",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("depotid", p_u64("Depot ID.")),
                    ("source_manifestid", manifest_id()),
                    ("target_manifestid", manifest_id()),
                    p_force(),
                ],
                &["appid", "depotid", "source_manifestid", "target_manifestid"],
            ),
            KeyKind::None,
            depot_patch,
        ),
        def(
            "steam_get_wishlist_count",
            "Wishlist Count",
            "Number of items on a player's public wishlist (cheaper than fetching the list).",
            object_schema(vec![("steamid", p_steamid()), p_force()], &["steamid"]),
            KeyKind::None,
            wishlist_count,
        ),
        def(
            "steam_get_followed_games",
            "Followed Games",
            "Games a player follows on the store (public profiles).",
            object_schema(
                vec![
                    ("steamid", p_steamid()),
                    ("limit", p_int("Maximum apps returned.", 1, 5000, Some(100))),
                    p_force(),
                ],
                &["steamid"],
            ),
            KeyKind::None,
            followed_games,
        ),
        def(
            "steam_get_workshop_vote_summary",
            "Workshop Vote Summary",
            "Up/down vote totals and score for up to 100 workshop items.",
            object_schema(
                vec![
                    (
                        "publishedfileids",
                        obj! { "type" => "array", "items" => obj! { "type" => "integer", "minimum" => 1 }, "minItems" => 1, "maxItems" => 100, "description" => "Workshop item IDs." },
                    ),
                    p_force(),
                ],
                &["publishedfileids"],
            ),
            KeyKind::Public,
            vote_summary,
        ),
        def(
            "steam_get_single_game_playtime",
            "Single Game Playtime",
            "A player's total playtime in one game.",
            object_schema(
                vec![("steamid", p_steamid()), ("appid", p_appid()), p_force()],
                &["steamid", "appid"],
            ),
            KeyKind::Public,
            single_game_playtime,
        ),
        def(
            "steam_is_playing_shared_game",
            "Family Sharing Check",
            "Whether a player is currently playing a game borrowed through Steam Family Sharing, and from whom.",
            object_schema(
                vec![("steamid", p_steamid()), ("appid", p_appid()), p_force()],
                &["steamid", "appid"],
            ),
            KeyKind::Public,
            shared_game,
        ),
        def(
            "steam_get_badge_progress",
            "Community Badge Progress",
            "Quest progress toward a Steam community badge (default: the Community Ambassador badge, 2).",
            object_schema(
                vec![
                    ("steamid", p_steamid()),
                    ("badgeid", p_int("Community badge ID.", 1, 100000, Some(2))),
                    p_force(),
                ],
                &["steamid"],
            ),
            KeyKind::Public,
            badge_progress,
        ),
        def(
            "steam_get_user_group_list",
            "User Groups",
            "Steam groups a player belongs to.",
            object_schema(vec![("steamid", p_steamid()), p_force()], &["steamid"]),
            KeyKind::Public,
            user_groups,
        ),
        def(
            "steam_get_asset_class_info",
            "Asset Class Info",
            "Item metadata for in-game economy asset classes (names, descriptions, tags, market data).",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    (
                        "classids",
                        obj! { "type" => "array", "items" => obj! { "type" => "integer", "minimum" => 1 }, "minItems" => 1, "maxItems" => 50, "description" => "Asset class IDs." },
                    ),
                    ("language", p_lang()),
                    p_force(),
                ],
                &["appid", "classids"],
            ),
            KeyKind::Public,
            asset_class_info,
        ),
        ToolDef {
            scope: scopes::PARTNER,
            ..def(
                "steam_partner_get_store_items",
                "Partner Store Items",
                "Like steam_get_store_items but through the partner service, so it includes your unreleased or hidden apps.",
                object_schema(
                    vec![
                        ("appids", appids_schema(50)),
                        ("cc", p_cc()),
                        ("language", p_lang()),
                        p_force(),
                    ],
                    &["appids"],
                ),
                KeyKind::Publisher,
                partner_store_items,
            )
        },
        ToolDef {
            scope: scopes::PARTNER,
            ..def(
                "steam_partner_get_workshop_contributors",
                "Workshop Item Contributors",
                "Finalized contributors (revenue-share recipients) for a curated workshop item in your app.",
                object_schema(
                    vec![
                        ("appid", p_appid()),
                        ("gameitemid", p_u64("Game item ID of the workshop item.")),
                        p_force(),
                    ],
                    &["appid", "gameitemid"],
                ),
                KeyKind::Publisher,
                workshop_contributors,
            )
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_store_items() {
        let tags: BTreeMap<u64, String> = [(492, "Indie".to_string())].into_iter().collect();
        let it = crate::json::parse(r#"{"success":1,"appid":620,"name":"Portal 2","tagids":[492,7],"release":{"steam_release_date":1303186800},"best_purchase_option":{"formatted_final_price":"$9.99"},"reviews":{"summary_filtered":{"review_count":10,"percent_positive":98,"review_score":9,"review_score_label":"Overwhelmingly Positive"}},"platforms":{"windows":true,"steam_deck_compat_category":3}}"#).unwrap();
        let s = shape_store_item(&it, &tags);
        assert_eq!(s["tags"], crate::arr!["Indie", "7"]);
        assert_eq!(s["releaseDate"].as_str(), Some("2011-04-19"));
        assert_eq!(s["platforms"]["steamDeck"].as_str(), Some("verified"));
        assert_eq!(
            s["reviews"]["label"].as_str(),
            Some("Overwhelmingly Positive")
        );
        let missing = shape_store_item(
            &crate::json::parse(r#"{"success":15,"id":1}"#).unwrap(),
            &tags,
        );
        assert!(missing.get("error").is_some());
    }
}
