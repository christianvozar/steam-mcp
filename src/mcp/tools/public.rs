//! Public Steam Web API and storefront tools.

use super::*;
use crate::mcp::context::scopes;
use crate::steam::catalog;
use crate::steam::{Call, Host, KeyKind, parse_steamid};
use crate::try_steam;
use crate::util::{strip_markup, truncate};

fn get(env: &ToolEnv, call: Call, args: &Map) -> Result<Lookup, SteamError> {
    env.steam().call(&call, force(args))
}

fn steamid_arg(args: &Map, key: &str) -> Result<u64, ToolOutcome> {
    let raw = arg_str(args, key)
        .map(String::from)
        .or_else(|| args.get(key).map(|v| v.to_param_string()))
        .unwrap_or_default();
    parse_steamid(&raw).ok_or_else(|| tool_error(format!("'{raw}' is not a valid 64-bit SteamID (17 digits starting with 7656119). Use steam_resolve_vanity_url for profile names.")))
}

fn steamids_arg(args: &Map, key: &str, max: usize) -> Result<Vec<u64>, ToolOutcome> {
    let list = arg_list(args, key);
    if list.is_empty() {
        return Err(tool_error(format!(
            "'{key}' must contain at least one SteamID"
        )));
    }
    if list.len() > max {
        return Err(tool_error(format!(
            "'{key}' accepts at most {max} SteamIDs"
        )));
    }
    list.iter()
        .map(|s| {
            parse_steamid(s)
                .ok_or_else(|| tool_error(format!("'{s}' is not a valid 64-bit SteamID")))
        })
        .collect()
}

macro_rules! tryo {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(o) => return o,
        }
    };
}

fn store_url(appid: u64) -> String {
    format!("https://store.steampowered.com/app/{appid}/")
}

fn appdetails_negative(v: &Value) -> bool {
    v.as_object()
        .is_none_or(|m| m.is_empty() || m.values().all(|x| x["success"].as_bool() != Some(true)))
}

fn reviews_negative(v: &Value) -> bool {
    v["success"].as_i64() != Some(1)
}

fn persona_state(code: i64) -> &'static str {
    match code {
        0 => "offline",
        1 => "online",
        2 => "busy",
        3 => "away",
        4 => "snooze",
        5 => "looking to trade",
        6 => "looking to play",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// Shared fetchers used by tools, resources and prompts.
// ---------------------------------------------------------------------------

pub fn app_details_call(appid: u64, cc: &str, lang: &str, filters: Option<&str>) -> Call {
    Call::get(Host::Store, "/api/appdetails", KeyKind::None, "medium")
        .param("appids", appid)
        .param("cc", cc)
        .param("l", lang)
        .opt_param("filters", filters)
        .negative_if(appdetails_negative)
}

pub fn current_players_call(appid: u64) -> Call {
    Call::get(
        Host::Public,
        "/ISteamUserStats/GetNumberOfCurrentPlayers/v1/",
        KeyKind::None,
        "realtime",
    )
    .param("appid", appid)
    .negative_if(|v| v.at(&["response", "result"]).as_i64() != Some(1))
}

pub fn review_summary_call(appid: u64, language: &str, purchase_type: &str) -> Call {
    Call::get(
        Host::Store,
        &format!("/appreviews/{appid}"),
        KeyKind::None,
        "short",
    )
    .param("json", 1)
    .param("num_per_page", 0)
    .param("language", language)
    .param("purchase_type", purchase_type)
    .negative_if(reviews_negative)
}

pub fn news_call(appid: u64, count: u64, maxlength: u64) -> Call {
    Call::get(
        Host::Public,
        "/ISteamNews/GetNewsForApp/v2/",
        KeyKind::None,
        "short",
    )
    .param("appid", appid)
    .param("count", count)
    .param("maxlength", maxlength)
}

pub fn achievements_pct_call(appid: u64) -> Call {
    Call::get(
        Host::Public,
        "/ISteamUserStats/GetGlobalAchievementPercentagesForApp/v2/",
        KeyKind::None,
        "long",
    )
    .param("gameid", appid)
}

pub fn schema_call(appid: u64, lang: &str) -> Call {
    Call::get(
        Host::Public,
        "/ISteamUserStats/GetSchemaForGame/v2/",
        KeyKind::Public,
        "static",
    )
    .param("appid", appid)
    .param("l", lang)
}

pub fn player_summaries_call(ids: &[u64]) -> Call {
    let joined: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
    Call::get(
        Host::Public,
        "/ISteamUser/GetPlayerSummaries/v2/",
        KeyKind::Public,
        "medium",
    )
    .param("steamids", joined.join(","))
}

pub fn owned_games_call(steamid: u64, include_appinfo: bool, include_free: bool) -> Call {
    Call::get(
        Host::Public,
        "/IPlayerService/GetOwnedGames/v1/",
        KeyKind::Public,
        "medium",
    )
    .param("steamid", steamid)
    .param("include_appinfo", include_appinfo)
    .param("include_played_free_games", include_free)
}

pub fn recent_games_call(steamid: u64, count: u64) -> Call {
    Call::get(
        Host::Public,
        "/IPlayerService/GetRecentlyPlayedGames/v1/",
        KeyKind::Public,
        "short",
    )
    .param("steamid", steamid)
    .param("count", count)
}

/// Shapes appdetails `data` into a compact summary.
pub fn summarize_app(appid: u64, data: &Value) -> Value {
    let price = &data["price_overview"];
    let names = |v: &Value, field: &str| -> Vec<Value> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x[field].as_str().map(Value::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    obj! {
        "appid" => appid,
        "name" => data["name"].clone(),
        "type" => data["type"].clone(),
        "isFree" => data["is_free"].clone(),
        "shortDescription" => data["short_description"].as_str().map(|s| truncate(&strip_markup(s), 600)),
        "developers" => data["developers"].clone(),
        "publishers" => data["publishers"].clone(),
        "releaseDate" => data.at(&["release_date", "date"]).clone(),
        "comingSoon" => data.at(&["release_date", "coming_soon"]).clone(),
        "price" => if price.is_object() { obj! {
            "currency" => price["currency"].clone(),
            "initial" => price["initial_formatted"].clone(),
            "final" => price["final_formatted"].clone(),
            "discountPercent" => price["discount_percent"].clone(),
        } } else { Value::Null },
        "platforms" => data["platforms"].clone(),
        "genres" => names(&data["genres"], "description"),
        "categories" => names(&data["categories"], "description"),
        "metacritic" => data.at(&["metacritic", "score"]).clone(),
        "recommendations" => data.at(&["recommendations", "total"]).clone(),
        "achievements" => data.at(&["achievements", "total"]).clone(),
        "dlcCount" => data["dlc"].as_array().map(|a| a.len()),
        "website" => data["website"].clone(),
        "storeUrl" => store_url(appid),
    }
}

/// Shapes an appreviews `query_summary`.
pub fn summarize_reviews(qs: &Value) -> Value {
    let pos = qs["total_positive"].as_u64().unwrap_or(0);
    let neg = qs["total_negative"].as_u64().unwrap_or(0);
    let total = qs["total_reviews"].as_u64().unwrap_or(pos + neg);
    let pct = if pos + neg == 0 {
        Value::Null
    } else {
        Value::from(((pos as f64 / (pos + neg) as f64) * 1000.0).round() / 10.0)
    };
    obj! {
        "reviewScore" => qs["review_score"].clone(),
        "reviewScoreDescription" => qs["review_score_desc"].clone(),
        "totalPositive" => pos,
        "totalNegative" => neg,
        "totalReviews" => total,
        "positivePercent" => pct,
    }
}

fn shape_review(r: &Value, max_chars: usize) -> Value {
    obj! {
        "recommendationId" => r["recommendationid"].clone(),
        "votedUp" => r["voted_up"].clone(),
        "language" => r["language"].clone(),
        "review" => r["review"].as_str().map(|s| truncate(s.trim(), max_chars)),
        "createdAt" => r["timestamp_created"].as_u64().map(crate::util::time::iso8601_secs),
        "updatedAt" => r["timestamp_updated"].as_u64().map(crate::util::time::iso8601_secs),
        "votesUp" => r["votes_up"].clone(),
        "votesFunny" => r["votes_funny"].clone(),
        "weightedVoteScore" => r["weighted_vote_score"].clone(),
        "commentCount" => r["comment_count"].clone(),
        "steamPurchase" => r["steam_purchase"].clone(),
        "receivedForFree" => r["received_for_free"].clone(),
        "writtenDuringEarlyAccess" => r["written_during_early_access"].clone(),
        "playtimeAtReviewMinutes" => r.at(&["author", "playtime_at_review"]).clone(),
        "playtimeForeverMinutes" => r.at(&["author", "playtime_forever"]).clone(),
        "authorSteamId" => r.at(&["author", "steamid"]).clone(),
        "authorNumReviews" => r.at(&["author", "num_reviews"]).clone(),
    }
}

/// Fetches up to `max_pages` pages of reviews, following cursors.
/// (query summary, reviews, next cursor, cache lookups).
pub type ReviewPages = (Value, Vec<Value>, Option<String>, Vec<Lookup>);

pub fn fetch_reviews(
    env: &ToolEnv,
    appid: u64,
    args: &Map,
    per_page: u64,
    max_pages: u64,
) -> Result<ReviewPages, SteamError> {
    let filter = arg_str(args, "filter").unwrap_or("recent");
    let language = arg_str(args, "language").unwrap_or("all");
    let review_type = arg_str(args, "review_type").unwrap_or("all");
    let purchase_type = arg_str(args, "purchase_type").unwrap_or("all");
    let mut cursor = arg_str(args, "cursor").unwrap_or("*").to_string();
    let mut reviews = Vec::new();
    let mut summary = Value::Null;
    let mut lookups = Vec::new();
    let mut next_cursor = None;
    for page in 0..max_pages {
        if env.cancelled() {
            break;
        }
        let call = Call::get(
            Host::Store,
            &format!("/appreviews/{appid}"),
            KeyKind::None,
            "short",
        )
        .param("json", 1)
        .param("filter", filter)
        .param("language", language)
        .param("review_type", review_type)
        .param("purchase_type", purchase_type)
        .param("num_per_page", per_page)
        .param("cursor", &cursor)
        .opt_param("day_range", arg_u64(args, "day_range"))
        .opt_param(
            "filter_offtopic_activity",
            args.get("filter_offtopic_activity")
                .and_then(|v| v.as_bool())
                .map(|b| b as u8),
        )
        .negative_if(reviews_negative);
        let l = env.steam().call(&call, force(args))?;
        if l.negative {
            return Err(SteamError::new(
                crate::steam::ErrorKind::NotFound,
                format!("No reviews are available for app {appid}"),
            ));
        }
        if summary.is_null() {
            summary = l.value["query_summary"].clone();
        }
        let page_reviews = l.value["reviews"].as_array().cloned().unwrap_or_default();
        let got = page_reviews.len();
        reviews.extend(page_reviews);
        let c = l.value["cursor"].as_str().map(String::from);
        lookups.push(l);
        env.progress(
            (page + 1) as f64,
            Some(max_pages as f64),
            &format!("Fetched review page {} of up to {max_pages}", page + 1),
        );
        match c {
            Some(c) if got > 0 && c != cursor => {
                next_cursor = Some(c.clone());
                cursor = c;
            }
            _ => {
                next_cursor = None;
                break;
            }
        }
    }
    Ok((summary, reviews, next_cursor, lookups))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn search_apps(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let query = arg_str(args, "query").unwrap_or("");
    let limit = arg_u64(args, "limit").unwrap_or(10).clamp(1, 100) as usize;
    env.progress(
        0.0,
        Some(1.0),
        "Loading the Steam app catalog (cached after first use)",
    );
    let (lookup, index) = try_steam!(env.steam().catalog(force(args)));
    let hits = catalog::search(&index, query, limit);
    let results: Vec<Value> = hits
        .iter()
        .map(|(e, score)| obj! { "appid" => e.appid, "name" => e.name.as_str(), "score" => *score as u64, "storeUrl" => store_url(e.appid) })
        .collect();
    env.progress(1.0, Some(1.0), "Search complete");
    let summary = if results.is_empty() {
        format!(
            "No apps matched '{query}' in the {}-app catalog.",
            fmt_num(index.len() as u64)
        )
    } else {
        let top: Vec<String> = hits
            .iter()
            .take(5)
            .map(|(e, _)| format!("{} ({})", e.name, e.appid))
            .collect();
        format!(
            "{} match(es) for '{query}': {}",
            results.len(),
            top.join(", ")
        )
    };
    ok_result(
        summary,
        obj! { "query" => query, "catalogSize" => index.len(), "results" => results },
        &[&lookup],
    )
}

fn app_details(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let filters = arg_list(args, "filters");
    let filters = (!filters.is_empty()).then(|| filters.join(","));
    let l = try_steam!(get(
        env,
        app_details_call(appid, &cc(env, args), &lang(env, args), filters.as_deref()),
        args
    ));
    if l.negative {
        return not_found(&format!("App {appid}"));
    }
    let data = l.value.at(&[&appid.to_string(), "data"]).clone();
    let summary = summarize_app(appid, &data);
    let text = format!(
        "{} ({}) — {}; released {}; {}",
        summary["name"].as_str().unwrap_or("?"),
        appid,
        summary["type"].as_str().unwrap_or("app"),
        summary["releaseDate"].as_str().unwrap_or("unknown"),
        if summary["isFree"].as_bool() == Some(true) {
            "free to play".to_string()
        } else {
            summary
                .at(&["price", "final"])
                .as_str()
                .map(|p| format!("price {p}"))
                .unwrap_or_else(|| "no price listed".into())
        }
    );
    let include_raw = arg_bool(args, "include_raw", false);
    let mut out = obj! { "summary" => summary };
    if include_raw {
        out.set("raw", data);
    }
    ok_result(text, out, &[&l])
}

fn app_reviews(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let per_page = arg_u64(args, "num_per_page").unwrap_or(20).clamp(1, 100);
    let max_pages = arg_u64(args, "max_pages").unwrap_or(1).clamp(1, 50);
    let max_chars = arg_u64(args, "max_review_chars")
        .unwrap_or(800)
        .clamp(50, 8000) as usize;
    let (qs, reviews, next, lookups) =
        try_steam!(fetch_reviews(env, appid, args, per_page, max_pages));
    let shaped: Vec<Value> = reviews.iter().map(|r| shape_review(r, max_chars)).collect();
    let up = reviews
        .iter()
        .filter(|r| r["voted_up"].as_bool() == Some(true))
        .count();
    let summary = summarize_reviews(&qs);
    let text = format!(
        "App {appid}: {} reviews returned ({} positive / {} negative in this sample). Overall: {} ({}% positive of {}).",
        shaped.len(),
        up,
        shaped.len() - up,
        summary["reviewScoreDescription"].as_str().unwrap_or("n/a"),
        summary["positivePercent"],
        summary["totalReviews"]
    );
    let refs: Vec<&Lookup> = lookups.iter().collect();
    ok_result(
        text,
        obj! { "appid" => appid, "querySummary" => summary, "reviews" => shaped, "nextCursor" => next },
        &refs,
    )
}

fn review_summary(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let language = arg_str(args, "language").unwrap_or("all");
    let purchase = arg_str(args, "purchase_type").unwrap_or("all");
    let l = try_steam!(get(
        env,
        review_summary_call(appid, language, purchase),
        args
    ));
    if l.negative {
        return not_found(&format!("Reviews for app {appid}"));
    }
    let s = summarize_reviews(&l.value["query_summary"]);
    let text = format!(
        "App {appid}: {} — {}% positive ({} positive, {} negative, {} total).",
        s["reviewScoreDescription"]
            .as_str()
            .unwrap_or("No user reviews"),
        s["positivePercent"],
        fmt_num(s["totalPositive"].as_u64().unwrap_or(0)),
        fmt_num(s["totalNegative"].as_u64().unwrap_or(0)),
        fmt_num(s["totalReviews"].as_u64().unwrap_or(0))
    );
    ok_result(
        text,
        s.with("appid", appid).with("language", language),
        &[&l],
    )
}

fn current_players(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let l = try_steam!(get(env, current_players_call(appid), args));
    if l.negative {
        return not_found(&format!("Player count for app {appid}"));
    }
    let n = l
        .value
        .at(&["response", "player_count"])
        .as_u64()
        .unwrap_or(0);
    ok_result(
        format!(
            "App {appid} has {} players in-game right now (cached up to {}s).",
            fmt_num(n),
            l.ttl_ms / 1000
        ),
        obj! { "appid" => appid, "playerCount" => n, "asOf" => crate::util::time::iso8601_ms(l.fetched_at_ms) },
        &[&l],
    )
}

fn app_news(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let count = arg_u64(args, "count").unwrap_or(10).clamp(1, 100);
    let maxlength = arg_u64(args, "maxlength").unwrap_or(600).min(20_000);
    let call = news_call(appid, count, maxlength)
        .opt_param("enddate", arg_u64(args, "enddate"))
        .opt_param(
            "feeds",
            (!arg_list(args, "feeds").is_empty()).then(|| arg_list(args, "feeds").join(",")),
        )
        .opt_param(
            "tags",
            (!arg_list(args, "tags").is_empty()).then(|| arg_list(args, "tags").join(",")),
        );
    let l = try_steam!(get(env, call, args));
    if l.negative {
        return not_found(&format!("News for app {appid}"));
    }
    let items: Vec<Value> = l
        .value
        .at(&["appnews", "newsitems"])
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|n| {
            obj! {
                "gid" => n["gid"].clone(),
                "title" => n["title"].clone(),
                "url" => n["url"].clone(),
                "author" => n["author"].clone(),
                "feed" => n["feedlabel"].clone(),
                "date" => n["date"].as_u64().map(crate::util::time::iso8601_secs),
                "tags" => n["tags"].clone(),
                "contents" => n["contents"].as_str().map(strip_markup),
            }
        })
        .collect();
    let titles: Vec<String> = items
        .iter()
        .take(5)
        .filter_map(|i| i["title"].as_str().map(String::from))
        .collect();
    ok_result(
        format!(
            "{} news item(s) for app {appid}. Latest: {}",
            items.len(),
            if titles.is_empty() {
                "none".into()
            } else {
                titles.join(" | ")
            }
        ),
        obj! { "appid" => appid, "count" => l.value.at(&["appnews", "count"]).clone(), "items" => items },
        &[&l],
    )
}

fn achievement_pct(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let l = try_steam!(get(env, achievements_pct_call(appid), args));
    let mut list: Vec<(String, f64)> = l
        .value
        .at(&["achievementpercentages", "achievements"])
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some((
                        x["name"].as_str()?.to_string(),
                        x["percent"].as_f64_lenient()?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    if l.negative || list.is_empty() {
        return not_found(&format!("Global achievement data for app {appid}"));
    }
    list.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let avg = list.iter().map(|x| x.1).sum::<f64>() / list.len() as f64;
    let rarest = list.last().cloned().unwrap_or_default();
    let commonest = list.first().cloned().unwrap_or_default();
    let items: Vec<Value> = list
        .iter()
        .map(|(n, p)| obj! { "name" => n.as_str(), "percent" => (p * 100.0).round() / 100.0 })
        .collect();
    ok_result(
        format!(
            "App {appid}: {} achievements; most common {} ({:.1}%), rarest {} ({:.2}%), mean unlock rate {:.1}%.",
            list.len(),
            commonest.0,
            commonest.1,
            rarest.0,
            rarest.1,
            avg
        ),
        obj! { "appid" => appid, "count" => list.len(), "meanPercent" => (avg * 100.0).round() / 100.0, "achievements" => items },
        &[&l],
    )
}

fn game_schema(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let l = try_steam!(get(env, schema_call(appid, &lang(env, args)), args));
    let game = l.value["game"].clone();
    if l.negative || !game.is_object() || game.as_object().is_some_and(|m| m.is_empty()) {
        return not_found(&format!("Stats schema for app {appid}"));
    }
    let icons = arg_bool(args, "include_icons", false);
    let ach: Vec<Value> = game
        .at(&["availableGameStats", "achievements"])
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut a| {
            if !icons {
                if let Some(m) = a.as_object_mut() {
                    m.remove("icon");
                    m.remove("icongray");
                }
            }
            a
        })
        .collect();
    let stats = game.at(&["availableGameStats", "stats"]).clone();
    let hidden = ach
        .iter()
        .filter(|a| a["hidden"].as_i64() == Some(1))
        .count();
    ok_result(
        format!(
            "{} (app {appid}) schema: {} achievements ({} hidden), {} stats.",
            game["gameName"].as_str().unwrap_or("?"),
            ach.len(),
            hidden,
            stats.as_array().map(|a| a.len()).unwrap_or(0)
        ),
        obj! { "appid" => appid, "gameName" => game["gameName"].clone(), "gameVersion" => game["gameVersion"].clone(), "achievements" => ach, "stats" => stats },
        &[&l],
    )
}

fn global_stats(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let names = arg_list(args, "names");
    if names.is_empty() {
        return tool_error(
            "'names' must list at least one aggregated stat API name (see steam_get_game_schema).",
        );
    }
    let mut call = Call::get(
        Host::Public,
        "/ISteamUserStats/GetGlobalStatsForGame/v1/",
        KeyKind::None,
        "long",
    )
    .param("appid", appid)
    .param("count", names.len());
    for (i, n) in names.iter().enumerate() {
        call = call.param(&format!("name[{i}]"), n);
    }
    let to_unix = |k: &str| {
        args.get(k).and_then(|v| {
            v.as_u64_lenient()
                .or_else(|| v.as_str().and_then(crate::util::time::parse_datetime))
        })
    };
    call = call
        .opt_param("startdate", to_unix("startdate"))
        .opt_param("enddate", to_unix("enddate"));
    let l = try_steam!(get(env, call, args));
    let resp = &l.value["response"];
    if resp["result"].as_i64() != Some(1) {
        return tool_error(format!(
            "Steam could not return global stats for app {appid}: {}. Only stats flagged as globally aggregated are available.",
            resp["error"].as_str().unwrap_or("unknown error")
        ));
    }
    ok_result(
        format!("Global stats for app {appid}: {}", names.join(", ")),
        obj! { "appid" => appid, "globalstats" => resp["globalstats"].clone() },
        &[&l],
    )
}

fn player_summaries(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let ids = tryo!(steamids_arg(args, "steamids", 100));
    let l = try_steam!(get(env, player_summaries_call(&ids), args));
    let players: Vec<Value> = l.value.at(&["response", "players"]).as_array().cloned().unwrap_or_default()
        .iter()
        .map(|p| obj! {
            "steamid" => p["steamid"].clone(),
            "personaName" => p["personaname"].clone(),
            "profileUrl" => p["profileurl"].clone(),
            "avatar" => p["avatarfull"].clone(),
            "personaState" => persona_state(p["personastate"].as_i64().unwrap_or(-1)),
            "visibility" => if p["communityvisibilitystate"].as_i64() == Some(3) { "public" } else { "private" },
            "profileConfigured" => p["profilestate"].as_i64() == Some(1),
            "lastLogoff" => p["lastlogoff"].as_u64().map(crate::util::time::iso8601_secs),
            "accountCreated" => p["timecreated"].as_u64().map(crate::util::time::iso8601_secs),
            "countryCode" => p["loccountrycode"].clone(),
            "currentGameId" => p["gameid"].clone(),
            "currentGame" => p["gameextrainfo"].clone(),
            "realName" => p["realname"].clone(),
        })
        .collect();
    let names: Vec<String> = players
        .iter()
        .filter_map(|p| p["personaName"].as_str().map(String::from))
        .take(10)
        .collect();
    ok_result(
        format!(
            "{} of {} profile(s) found: {}",
            players.len(),
            ids.len(),
            names.join(", ")
        ),
        obj! { "players" => players },
        &[&l],
    )
}

fn resolve_vanity(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let raw = arg_str(args, "vanity").unwrap_or("");
    // Accept full profile URLs.
    let vanity = raw
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(raw)
        .to_string();
    if let Some(id) = parse_steamid(&vanity) {
        return ok_result(
            format!("'{raw}' is already a SteamID64: {id}"),
            obj! { "steamid" => id.to_string(), "vanity" => vanity },
            &[],
        );
    }
    let url_type = match arg_str(args, "url_type").unwrap_or("individual") {
        "group" => 2,
        "game_group" => 3,
        _ => 1,
    };
    let call = Call::get(
        Host::Public,
        "/ISteamUser/ResolveVanityURL/v1/",
        KeyKind::Public,
        "static",
    )
    .param("vanityurl", &vanity)
    .param("url_type", url_type)
    .negative_if(|v| v.at(&["response", "success"]).as_i64() != Some(1));
    let l = try_steam!(get(env, call, args));
    if l.negative {
        return tool_error(format!(
            "No Steam profile or group uses the vanity name '{vanity}'."
        ));
    }
    let id = l
        .value
        .at(&["response", "steamid"])
        .as_str()
        .unwrap_or("")
        .to_string();
    ok_result(
        format!("'{vanity}' resolves to SteamID {id}"),
        obj! { "vanity" => vanity, "steamid" => id },
        &[&l],
    )
}

fn owned_games(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let mut call = owned_games_call(
        steamid,
        arg_bool(args, "include_appinfo", true),
        arg_bool(args, "include_played_free_games", true),
    )
    .opt_param(
        "include_free_sub",
        arg_bool(args, "include_free_sub", false).then_some(true),
    );
    for (i, id) in arg_list(args, "appids_filter").iter().enumerate() {
        call = call.param(&format!("appids_filter[{i}]"), id);
    }
    let l = try_steam!(get(env, call, args));
    let resp = &l.value["response"];
    let Some(games) = resp["games"].as_array() else {
        return tool_error(format!(
            "No game library is visible for {steamid}. The profile or its game details are probably private."
        ));
    };
    let mut games = games.clone();
    match arg_str(args, "sort_by").unwrap_or("playtime") {
        "recent" => {
            games.sort_by_key(|g| std::cmp::Reverse(g["rtime_last_played"].as_u64().unwrap_or(0)))
        }
        "name" => games.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        }),
        _ => games.sort_by_key(|g| std::cmp::Reverse(g["playtime_forever"].as_u64().unwrap_or(0))),
    }
    let total_minutes: u64 = games
        .iter()
        .filter_map(|g| g["playtime_forever"].as_u64())
        .sum();
    let never_played = games
        .iter()
        .filter(|g| g["playtime_forever"].as_u64() == Some(0))
        .count();
    let limit = arg_u64(args, "limit").unwrap_or(100).clamp(1, 5000) as usize;
    let shaped: Vec<Value> = games.iter().take(limit).map(|g| obj! {
        "appid" => g["appid"].clone(),
        "name" => g["name"].clone(),
        "playtimeForeverHours" => g["playtime_forever"].as_u64().map(|m| (m as f64 / 6.0).round() / 10.0),
        "playtime2WeeksHours" => g["playtime_2weeks"].as_u64().map(|m| (m as f64 / 6.0).round() / 10.0),
        "lastPlayed" => g["rtime_last_played"].as_u64().filter(|t| *t > 0).map(crate::util::time::iso8601_secs),
    }).collect();
    let top: Vec<String> = shaped
        .iter()
        .take(5)
        .filter_map(|g| {
            Some(format!(
                "{} ({}h)",
                g["name"].as_str()?,
                g["playtimeForeverHours"]
            ))
        })
        .collect();
    ok_result(
        format!(
            "{steamid} owns {} games ({} never played), {:.0} hours total. Top: {}",
            resp["game_count"],
            never_played,
            total_minutes as f64 / 60.0,
            top.join(", ")
        ),
        obj! { "steamid" => steamid.to_string(), "gameCount" => resp["game_count"].clone(), "totalPlaytimeHours" => (total_minutes as f64 / 6.0).round() / 10.0, "neverPlayed" => never_played, "games" => shaped, "truncated" => games.len() > limit },
        &[&l],
    )
}

fn recent_games(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let l = try_steam!(get(
        env,
        recent_games_call(steamid, arg_u64(args, "count").unwrap_or(10).clamp(1, 100)),
        args
    ));
    let games = l
        .value
        .at(&["response", "games"])
        .as_array()
        .cloned()
        .unwrap_or_default();
    let names: Vec<String> = games
        .iter()
        .filter_map(|g| g["name"].as_str().map(String::from))
        .collect();
    ok_result(
        format!(
            "{steamid} played {} game(s) in the last two weeks: {}",
            games.len(),
            if names.is_empty() {
                "none visible".into()
            } else {
                names.join(", ")
            }
        ),
        obj! { "steamid" => steamid.to_string(), "totalCount" => l.value.at(&["response", "total_count"]).clone(), "games" => games },
        &[&l],
    )
}

fn player_achievements(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let call = Call::get(
        Host::Public,
        "/ISteamUserStats/GetPlayerAchievements/v1/",
        KeyKind::Public,
        "short",
    )
    .param("steamid", steamid)
    .param("appid", appid)
    .param("l", lang(env, args));
    let l = try_steam!(get(env, call, args));
    let ps = &l.value["playerstats"];
    if ps["success"].as_bool() == Some(false) {
        return tool_error(format!(
            "Steam could not return achievements: {}",
            ps["error"]
                .as_str()
                .unwrap_or("profile may be private or the game has no stats")
        ));
    }
    let list = ps["achievements"].as_array().cloned().unwrap_or_default();
    let unlocked = list
        .iter()
        .filter(|a| a["achieved"].as_i64() == Some(1))
        .count();
    let shaped: Vec<Value> = list.iter().map(|a| obj! {
        "apiName" => a["apiname"].clone(),
        "name" => a["name"].clone(),
        "description" => a["description"].clone(),
        "achieved" => a["achieved"].as_i64() == Some(1),
        "unlockedAt" => a["unlocktime"].as_u64().filter(|t| *t > 0).map(crate::util::time::iso8601_secs),
    }).collect();
    let pct = if list.is_empty() {
        0.0
    } else {
        unlocked as f64 * 100.0 / list.len() as f64
    };
    ok_result(
        format!(
            "{steamid} unlocked {unlocked}/{} achievements ({pct:.1}%) in {}.",
            list.len(),
            ps["gameName"].as_str().unwrap_or("this game")
        ),
        obj! { "steamid" => steamid.to_string(), "appid" => appid, "gameName" => ps["gameName"].clone(), "unlocked" => unlocked, "total" => list.len(), "achievements" => shaped },
        &[&l],
    )
}

fn user_stats(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let call = Call::get(
        Host::Public,
        "/ISteamUserStats/GetUserStatsForGame/v2/",
        KeyKind::Public,
        "short",
    )
    .param("steamid", steamid)
    .param("appid", appid);
    let l = try_steam!(get(env, call, args));
    if l.negative {
        return not_found(&format!("Stats for {steamid} in app {appid}"));
    }
    let ps = &l.value["playerstats"];
    ok_result(
        format!(
            "{steamid}: {} stats and {} achievements recorded for {}.",
            ps["stats"].as_array().map(|a| a.len()).unwrap_or(0),
            ps["achievements"].as_array().map(|a| a.len()).unwrap_or(0),
            ps["gameName"].as_str().unwrap_or("this game")
        ),
        ps.clone(),
        &[&l],
    )
}

fn friend_list(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let call = Call::get(
        Host::Public,
        "/ISteamUser/GetFriendList/v1/",
        KeyKind::Public,
        "medium",
    )
    .param("steamid", steamid)
    .param(
        "relationship",
        arg_str(args, "relationship").unwrap_or("friend"),
    );
    let l = try_steam!(get(env, call, args));
    if l.negative {
        return tool_error(format!(
            "The friend list for {steamid} is private or unavailable."
        ));
    }
    let friends = l
        .value
        .at(&["friendslist", "friends"])
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut lookups = vec![l.clone()];
    let mut out: Vec<Value> = friends
        .iter()
        .map(|f| {
            obj! {
                "steamid" => f["steamid"].clone(),
                "relationship" => f["relationship"].clone(),
                "friendSince" => f["friend_since"].as_u64().map(crate::util::time::iso8601_secs),
            }
        })
        .collect();
    if arg_bool(args, "resolve_profiles", false) && !friends.is_empty() {
        let ids: Vec<u64> = friends
            .iter()
            .filter_map(|f| f["steamid"].as_str().and_then(parse_steamid))
            .take(100)
            .collect();
        if let Ok(sl) = get(env, player_summaries_call(&ids), args) {
            let names: std::collections::HashMap<String, String> = sl
                .value
                .at(&["response", "players"])
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|p| {
                            Some((
                                p["steamid"].as_str()?.to_string(),
                                p["personaname"].as_str()?.to_string(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            for f in out.iter_mut() {
                if let Some(n) = f["steamid"].as_str().and_then(|id| names.get(id)) {
                    f.set("personaName", n.as_str());
                }
            }
            lookups.push(sl);
        }
    }
    let refs: Vec<&Lookup> = lookups.iter().collect();
    ok_result(
        format!("{steamid} has {} friend(s) visible.", out.len()),
        obj! { "steamid" => steamid.to_string(), "friends" => out },
        &refs,
    )
}

fn player_bans(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let ids = tryo!(steamids_arg(args, "steamids", 100));
    let joined: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
    let call = Call::get(
        Host::Public,
        "/ISteamUser/GetPlayerBans/v1/",
        KeyKind::Public,
        "medium",
    )
    .param("steamids", joined.join(","));
    let l = try_steam!(get(env, call, args));
    let players = l.value["players"].as_array().cloned().unwrap_or_default();
    let flagged = players
        .iter()
        .filter(|p| {
            p["VACBanned"].as_bool() == Some(true)
                || p["NumberOfGameBans"].as_u64().unwrap_or(0) > 0
                || p["CommunityBanned"].as_bool() == Some(true)
                || p["EconomyBan"].as_str().is_some_and(|e| e != "none")
        })
        .count();
    ok_result(
        format!(
            "{} account(s) checked; {flagged} with VAC, game, community or trade bans.",
            players.len()
        ),
        obj! { "players" => players },
        &[&l],
    )
}

fn steam_level(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let call = Call::get(
        Host::Public,
        "/IPlayerService/GetSteamLevel/v1/",
        KeyKind::Public,
        "medium",
    )
    .param("steamid", steamid);
    let l = try_steam!(get(env, call, args));
    let level = l.value.at(&["response", "player_level"]).clone();
    if level.is_null() {
        return tool_error(format!(
            "The Steam level of {steamid} is not visible (private profile)."
        ));
    }
    ok_result(
        format!("{steamid} is Steam level {level}."),
        obj! { "steamid" => steamid.to_string(), "level" => level },
        &[&l],
    )
}

fn badges(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let call = Call::get(
        Host::Public,
        "/IPlayerService/GetBadges/v1/",
        KeyKind::Public,
        "medium",
    )
    .param("steamid", steamid);
    let l = try_steam!(get(env, call, args));
    let r = &l.value["response"];
    let all = r["badges"].as_array().cloned().unwrap_or_default();
    let limit = arg_u64(args, "limit").unwrap_or(50).clamp(1, 1000) as usize;
    ok_result(
        format!(
            "{steamid}: level {}, {} XP, {} badges.",
            r["player_level"],
            r["player_xp"],
            all.len()
        ),
        obj! {
            "steamid" => steamid.to_string(),
            "level" => r["player_level"].clone(),
            "xp" => r["player_xp"].clone(),
            "xpNeededToLevelUp" => r["player_xp_needed_to_level_up"].clone(),
            "badgeCount" => all.len(),
            "badges" => all.into_iter().take(limit).collect::<Vec<_>>(),
        },
        &[&l],
    )
}

fn player_profile(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let mut lookups = Vec::new();
    let mut section = |label: &str, step: f64, call: Call| -> Value {
        env.progress(step, Some(4.0), &format!("Fetching {label}"));
        match get(env, call, args) {
            Ok(l) => {
                let v = (*l.value).clone();
                lookups.push(l);
                v
            }
            Err(e) => obj! { "error" => e.message },
        }
    };
    let summary = section("profile", 0.0, player_summaries_call(&[steamid]));
    let level = section(
        "level",
        1.0,
        Call::get(
            Host::Public,
            "/IPlayerService/GetSteamLevel/v1/",
            KeyKind::Public,
            "medium",
        )
        .param("steamid", steamid),
    );
    let bans = section(
        "bans",
        2.0,
        Call::get(
            Host::Public,
            "/ISteamUser/GetPlayerBans/v1/",
            KeyKind::Public,
            "medium",
        )
        .param("steamids", steamid),
    );
    let recent = section("recent games", 3.0, recent_games_call(steamid, 5));
    env.progress(4.0, Some(4.0), "Profile assembled");
    let p = summary.at(&["response", "players", "0"]).clone();
    if p.is_null() {
        return tool_error(format!(
            "No Steam profile exists for {steamid}, or no Web API key is configured."
        ));
    }
    let ban = bans.at(&["players", "0"]).clone();
    let out = obj! {
        "steamid" => steamid.to_string(),
        "personaName" => p["personaname"].clone(),
        "profileUrl" => p["profileurl"].clone(),
        "visibility" => if p["communityvisibilitystate"].as_i64() == Some(3) { "public" } else { "private" },
        "personaState" => persona_state(p["personastate"].as_i64().unwrap_or(-1)),
        "currentGame" => p["gameextrainfo"].clone(),
        "countryCode" => p["loccountrycode"].clone(),
        "accountCreated" => p["timecreated"].as_u64().map(crate::util::time::iso8601_secs),
        "level" => level.at(&["response", "player_level"]).clone(),
        "bans" => ban,
        "recentGames" => recent.at(&["response", "games"]).clone(),
    };
    let refs: Vec<&Lookup> = lookups.iter().collect();
    ok_result(
        format!(
            "{} ({steamid}): level {}, {} profile, VAC banned: {}.",
            out["personaName"].as_str().unwrap_or("?"),
            out["level"],
            out["visibility"].as_str().unwrap_or("?"),
            out["bans"]["VACBanned"]
        ),
        out,
        &refs,
    )
}

fn most_played(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let l = try_steam!(get(
        env,
        Call::get(
            Host::Public,
            "/ISteamChartsService/GetMostPlayedGames/v1/",
            KeyKind::None,
            "short"
        ),
        args
    ));
    let limit = arg_u64(args, "limit").unwrap_or(25).clamp(1, 100) as usize;
    let names = env.steam().catalog_if_cached();
    let ranks: Vec<Value> = l.value.at(&["response", "ranks"]).as_array().cloned().unwrap_or_default()
        .iter()
        .take(limit)
        .map(|r| {
            let appid = r["appid"].as_u64().unwrap_or(0);
            let name = names.as_ref().and_then(|idx| idx.binary_search_by_key(&appid, |e| e.appid).ok().map(|i| idx[i].name.clone()));
            obj! { "rank" => r["rank"].clone(), "appid" => appid, "name" => name, "lastWeekRank" => r["last_week_rank"].clone(), "peakInGame" => r["peak_in_game"].clone() }
        })
        .collect();
    let top: Vec<String> = ranks
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
        format!("Most played on Steam: {}", top.join(", ")),
        obj! { "rollupDate" => l.value.at(&["response", "rollup_date"]).as_u64().map(crate::util::time::iso8601_secs), "ranks" => ranks },
        &[&l],
    )
}

fn workshop_details(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let ids = arg_list(args, "publishedfileids");
    if ids.is_empty() || ids.len() > 100 || ids.iter().any(|i| i.parse::<u64>().is_err()) {
        return tool_error("'publishedfileids' must contain 1–100 numeric workshop item IDs.");
    }
    let mut call = Call::post(
        Host::Public,
        "/ISteamRemoteStorage/GetPublishedFileDetails/v1/",
        KeyKind::None,
        "medium",
    )
    .param("itemcount", ids.len());
    for (i, id) in ids.iter().enumerate() {
        call = call.param(&format!("publishedfileids[{i}]"), id);
    }
    let l = try_steam!(get(env, call, args));
    let items: Vec<Value> = l
        .value
        .at(&["response", "publishedfiledetails"])
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut d| {
            if let Some(desc) = d["description"]
                .as_str()
                .map(|s| truncate(&strip_markup(s), 1500))
            {
                d.set("description", desc);
            }
            d
        })
        .collect();
    ok_result(
        format!("{} workshop item(s) returned.", items.len()),
        obj! { "items" => items },
        &[&l],
    )
}

const QUERY_TYPES: &[(&str, u32)] = &[
    ("ranked_by_vote", 0),
    ("ranked_by_publication_date", 1),
    ("ranked_by_trend", 3),
    ("ranked_by_total_unique_subscriptions", 9),
    ("ranked_by_votes_up", 11),
    ("ranked_by_text_search", 12),
    ("ranked_by_playtime_trend", 13),
    ("ranked_by_total_playtime", 14),
    ("ranked_by_last_updated_date", 21),
];

fn query_workshop(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let qt_name =
        arg_str(args, "query_type").unwrap_or(if arg_str(args, "search_text").is_some() {
            "ranked_by_text_search"
        } else {
            "ranked_by_trend"
        });
    let qt = QUERY_TYPES
        .iter()
        .find(|(n, _)| *n == qt_name)
        .map(|(_, v)| *v)
        .unwrap_or(3);
    let mut call = Call::get(
        Host::Public,
        "/IPublishedFileService/QueryFiles/v1/",
        KeyKind::Public,
        "short",
    )
    .param("appid", appid)
    .param("query_type", qt)
    .param(
        "numperpage",
        arg_u64(args, "numperpage").unwrap_or(20).clamp(1, 100),
    )
    .param("cursor", arg_str(args, "cursor").unwrap_or("*"))
    .opt_param("search_text", arg_str(args, "search_text"))
    .opt_param("days", arg_u64(args, "days"))
    .param("return_tags", true)
    .param("return_short_description", true)
    .param("return_vote_data", true)
    .param("return_previews", false);
    for (i, t) in arg_list(args, "required_tags").iter().enumerate() {
        call = call.param(&format!("requiredtags[{i}]"), t);
    }
    let l = try_steam!(get(env, call, args));
    let r = &l.value["response"];
    let items = r["publishedfiledetails"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let titles: Vec<String> = items
        .iter()
        .take(5)
        .filter_map(|i| i["title"].as_str().map(String::from))
        .collect();
    ok_result(
        format!(
            "{} of {} workshop items for app {appid} ({qt_name}): {}",
            items.len(),
            r["total"],
            titles.join(" | ")
        ),
        obj! { "appid" => appid, "total" => r["total"].clone(), "nextCursor" => r["next_cursor"].clone(), "items" => items },
        &[&l],
    )
}

fn supported_apis(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let partner = arg_str(args, "host") == Some("partner");
    let call = if partner {
        Call::get(
            Host::Partner,
            "/ISteamWebAPIUtil/GetSupportedAPIList/v1/",
            KeyKind::Publisher,
            "static",
        )
    } else {
        Call::get(
            Host::Public,
            "/ISteamWebAPIUtil/GetSupportedAPIList/v1/",
            KeyKind::Optional,
            "static",
        )
    };
    let l = try_steam!(get(env, call, args));
    let filter = arg_str(args, "filter").map(|s| s.to_ascii_lowercase());
    let include_params = arg_bool(args, "include_parameters", false);
    let ifaces: Vec<Value> = l.value.at(&["apilist", "interfaces"]).as_array().cloned().unwrap_or_default()
        .iter()
        .filter_map(|i| {
            let iname = i["name"].as_str()?.to_string();
            let methods: Vec<Value> = i["methods"].as_array()?.iter()
                .filter(|m| filter.as_ref().is_none_or(|f| iname.to_ascii_lowercase().contains(f) || m["name"].as_str().is_some_and(|n| n.to_ascii_lowercase().contains(f))))
                .map(|m| {
                    let mut o = obj! { "name" => m["name"].clone(), "version" => m["version"].clone(), "httpmethod" => m["httpmethod"].clone() };
                    if include_params {
                        let params: Vec<Value> = m["parameters"].as_array().map(|ps| ps.iter().map(|p| obj! { "name" => p["name"].clone(), "type" => p["type"].clone(), "optional" => p["optional"].clone() }).collect()).unwrap_or_default();
                        o.set("parameters", params);
                    }
                    o
                })
                .collect();
            (!methods.is_empty()).then(|| obj! { "name" => iname, "methods" => methods })
        })
        .collect();
    let mcount: usize = ifaces
        .iter()
        .map(|i| i["methods"].as_array().map(|m| m.len()).unwrap_or(0))
        .sum();
    ok_result(
        format!(
            "{} interface(s), {mcount} method(s) available to the configured key.",
            ifaces.len()
        ),
        obj! { "interfaces" => ifaces },
        &[&l],
    )
}

fn store_featured(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let call = Call::get(
        Host::Store,
        "/api/featuredcategories",
        KeyKind::None,
        "short",
    )
    .param("cc", cc(env, args))
    .param("l", lang(env, args));
    let l = try_steam!(get(env, call, args));
    let mut sections = Map::new();
    for key in ["specials", "coming_soon", "top_sellers", "new_releases"] {
        let items: Vec<Value> = l.value.at(&[key, "items"]).as_array().cloned().unwrap_or_default()
            .iter()
            .map(|i| obj! {
                "appid" => i["id"].clone(),
                "name" => i["name"].clone(),
                "discountPercent" => i["discount_percent"].clone(),
                "originalPrice" => i["original_price"].clone(),
                "finalPrice" => i["final_price"].clone(),
                "currency" => i["currency"].clone(),
                "discountExpiration" => i["discount_expiration"].as_u64().map(crate::util::time::iso8601_secs),
            })
            .collect();
        sections.insert(key, items);
    }
    let tops: Vec<String> = sections
        .get("top_sellers")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .take(5)
                .filter_map(|i| i["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    ok_result(
        format!(
            "Featured on the Steam store — top sellers: {}",
            tops.join(", ")
        ),
        Value::Object(sections),
        &[&l],
    )
}

fn package_details(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let ids = arg_list(args, "packageids");
    if ids.is_empty() || ids.len() > 50 || ids.iter().any(|i| i.parse::<u64>().is_err()) {
        return tool_error("'packageids' must contain 1–50 numeric package (sub) IDs.");
    }
    let call = Call::get(Host::Store, "/api/packagedetails", KeyKind::None, "medium")
        .param("packageids", ids.join(","))
        .param("cc", cc(env, args))
        .negative_if(appdetails_negative);
    let l = try_steam!(get(env, call, args));
    if l.negative {
        return not_found("The requested package(s)");
    }
    let mut out = Vec::new();
    for id in &ids {
        let e = &l.value[id.as_str()];
        if e["success"].as_bool() == Some(true) {
            let d = &e["data"];
            out.push(obj! { "packageid" => id.as_str(), "name" => d["name"].clone(), "price" => d["price"].clone(), "apps" => d["apps"].clone(), "platforms" => d["platforms"].clone(), "releaseDate" => d.at(&["release_date", "date"]).clone() });
        } else {
            out.push(obj! { "packageid" => id.as_str(), "error" => "not found" });
        }
    }
    ok_result(
        format!("{} package(s) returned.", out.len()),
        obj! { "packages" => out },
        &[&l],
    )
}

fn wishlist(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let call = Call::get(
        Host::Public,
        "/IWishlistService/GetWishlist/v1/",
        KeyKind::Optional,
        "medium",
    )
    .param("steamid", steamid);
    let l = try_steam!(get(env, call, args));
    let items = l
        .value
        .at(&["response", "items"])
        .as_array()
        .cloned()
        .unwrap_or_default();
    let names = env.steam().catalog_if_cached();
    let limit = arg_u64(args, "limit").unwrap_or(200).clamp(1, 5000) as usize;
    let mut shaped: Vec<Value> = items.iter().map(|i| {
        let appid = i["appid"].as_u64().unwrap_or(0);
        let name = names.as_ref().and_then(|idx| idx.binary_search_by_key(&appid, |e| e.appid).ok().map(|p| idx[p].name.clone()));
        obj! { "appid" => appid, "name" => name, "priority" => i["priority"].clone(), "dateAdded" => i["date_added"].as_u64().map(crate::util::time::iso8601_secs) }
    }).collect();
    shaped.sort_by_key(|i| i["priority"].as_u64().unwrap_or(u64::MAX));
    ok_result(
        format!("{steamid} has {} item(s) on their wishlist.", items.len()),
        obj! { "steamid" => steamid.to_string(), "count" => items.len(), "items" => shaped.into_iter().take(limit).collect::<Vec<_>>() },
        &[&l],
    )
}

fn servers_at_address(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let addr = arg_str(args, "addr").unwrap_or("");
    let call = Call::get(
        Host::Public,
        "/ISteamApps/GetServersAtAddress/v1/",
        KeyKind::None,
        "short",
    )
    .param("addr", addr);
    let l = try_steam!(get(env, call, args));
    let r = &l.value["response"];
    if r["success"].as_bool() == Some(false) {
        return tool_error(format!(
            "Steam could not look up '{addr}': {}",
            r["message"].as_str().unwrap_or("invalid address")
        ));
    }
    let servers = r["servers"].as_array().cloned().unwrap_or_default();
    ok_result(
        format!(
            "{} Steam game server(s) registered at {addr}.",
            servers.len()
        ),
        obj! { "addr" => addr, "servers" => servers },
        &[&l],
    )
}

fn up_to_date(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let version = arg_u64(args, "version").unwrap_or(0);
    let call = Call::get(
        Host::Public,
        "/ISteamApps/UpToDateCheck/v1/",
        KeyKind::None,
        "medium",
    )
    .param("appid", appid)
    .param("version", version);
    let l = try_steam!(get(env, call, args));
    let r = l.value["response"].clone();
    let text = if r["up_to_date"].as_bool() == Some(true) {
        format!("Version {version} of app {appid} is up to date.")
    } else {
        format!(
            "Version {version} of app {appid} is out of date; required version {}. {}",
            r["required_version"],
            r["message"].as_str().unwrap_or("")
        )
    };
    ok_result(text, r, &[&l])
}

fn game_servers(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let filter = arg_str(args, "filter")
        .map(String::from)
        .or_else(|| arg_u64(args, "appid").map(|a| format!("\\appid\\{a}")))
        .unwrap_or_default();
    if filter.is_empty() {
        return tool_error(
            "Provide 'filter' (master server filter syntax, e.g. \\appid\\730\\dedicated\\1) or 'appid'.",
        );
    }
    let limit = arg_u64(args, "limit").unwrap_or(100).clamp(1, 20_000);
    let call = Call::get(
        Host::Public,
        "/IGameServersService/GetServerList/v1/",
        KeyKind::Public,
        "realtime",
    )
    .param("filter", &filter)
    .param("limit", limit);
    let l = try_steam!(get(env, call, args));
    let servers = l
        .value
        .at(&["response", "servers"])
        .as_array()
        .cloned()
        .unwrap_or_default();
    let players: u64 = servers.iter().filter_map(|s| s["players"].as_u64()).sum();
    let bots: u64 = servers.iter().filter_map(|s| s["bots"].as_u64()).sum();
    ok_result(
        format!(
            "{} server(s) match '{filter}' with {} players ({} bots).",
            servers.len(),
            fmt_num(players),
            fmt_num(bots)
        ),
        obj! { "filter" => filter, "count" => servers.len(), "players" => players, "bots" => bots, "servers" => servers },
        &[&l],
    )
}

/// Collects the sections of an app overview; shared with resources and prompts.
pub fn app_overview_value(env: &ToolEnv, appid: u64, args: &Map) -> (Value, Vec<Lookup>) {
    let cc = cc(env, args);
    let lang = lang(env, args);
    let mut lookups = Vec::new();
    let mut errors = Map::new();
    let total = 5.0;
    env.progress(0.0, Some(total), "Fetching store details");
    let details = match get(env, app_details_call(appid, &cc, &lang, None), args) {
        Ok(l) if !l.negative => {
            let s = summarize_app(appid, l.value.at(&[&appid.to_string(), "data"]));
            lookups.push(l);
            s
        }
        Ok(_) => {
            errors.insert("details", "app not found on the store");
            Value::Null
        }
        Err(e) => {
            errors.insert("details", e.message);
            Value::Null
        }
    };
    env.progress(1.0, Some(total), "Fetching current players");
    let players = match get(env, current_players_call(appid), args) {
        Ok(l) if !l.negative => {
            let n = l.value.at(&["response", "player_count"]).clone();
            lookups.push(l);
            n
        }
        Ok(_) => Value::Null,
        Err(e) => {
            errors.insert("currentPlayers", e.message);
            Value::Null
        }
    };
    env.progress(2.0, Some(total), "Fetching review summary");
    let reviews = match get(env, review_summary_call(appid, "all", "all"), args) {
        Ok(l) if !l.negative => {
            let s = summarize_reviews(&l.value["query_summary"]);
            lookups.push(l);
            s
        }
        Ok(_) => Value::Null,
        Err(e) => {
            errors.insert("reviews", e.message);
            Value::Null
        }
    };
    env.progress(3.0, Some(total), "Fetching news");
    let news = match get(env, news_call(appid, 3, 300), args) {
        Ok(l) => {
            let items: Vec<Value> = l.value.at(&["appnews", "newsitems"]).as_array().cloned().unwrap_or_default().iter()
                .map(|n| obj! { "title" => n["title"].clone(), "date" => n["date"].as_u64().map(crate::util::time::iso8601_secs), "url" => n["url"].clone() })
                .collect();
            lookups.push(l);
            Value::Array(items)
        }
        Err(e) => {
            errors.insert("news", e.message);
            Value::Null
        }
    };
    env.progress(4.0, Some(total), "Fetching achievement statistics");
    let achievements = match get(env, achievements_pct_call(appid), args) {
        Ok(l) => {
            let list = l
                .value
                .at(&["achievementpercentages", "achievements"])
                .as_array()
                .cloned()
                .unwrap_or_default();
            let pcts: Vec<f64> = list
                .iter()
                .filter_map(|a| a["percent"].as_f64_lenient())
                .collect();
            lookups.push(l);
            if pcts.is_empty() {
                Value::Null
            } else {
                obj! { "count" => pcts.len(), "meanUnlockPercent" => ((pcts.iter().sum::<f64>() / pcts.len() as f64) * 10.0).round() / 10.0, "rarestPercent" => pcts.iter().cloned().fold(f64::INFINITY, f64::min) }
            }
        }
        Err(_) => Value::Null,
    };
    env.progress(5.0, Some(total), "Overview assembled");
    (
        obj! { "appid" => appid, "details" => details, "currentPlayers" => players, "reviews" => reviews, "recentNews" => news, "achievements" => achievements, "errors" => Value::Object(errors) },
        lookups,
    )
}

fn app_overview(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let (v, lookups) = app_overview_value(env, appid, args);
    if v["details"].is_null() && v["currentPlayers"].is_null() && v["reviews"].is_null() {
        return tool_error(format!(
            "No data could be retrieved for app {appid}: {}",
            v["errors"]
        ));
    }
    let text = format!(
        "{} ({appid}): {} players now; reviews {}; {}.",
        v.at(&["details", "name"]).as_str().unwrap_or("Unknown app"),
        v["currentPlayers"]
            .as_u64()
            .map(fmt_num)
            .unwrap_or_else(|| "n/a".into()),
        match v.at(&["reviews", "reviewScoreDescription"]).as_str() {
            Some(desc) => format!(
                "{desc} ({}% positive of {})",
                v.at(&["reviews", "positivePercent"]),
                v.at(&["reviews", "totalReviews"])
            ),
            None => "unavailable".into(),
        },
        v.at(&["details", "price", "final"])
            .as_str()
            .map(|p| format!("price {p}"))
            .unwrap_or_else(|| if v.at(&["details", "isFree"]).as_bool() == Some(true) {
                "free to play".into()
            } else {
                "no price listed".into()
            })
    );
    let refs: Vec<&Lookup> = lookups.iter().collect();
    ok_result(text, v, &refs)
}

fn compare_apps(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let ids: Vec<u64> = arg_list(args, "appids")
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();
    if ids.len() < 2 || ids.len() > 10 {
        return tool_error("'appids' must contain between 2 and 10 app IDs.");
    }
    let mut rows = Vec::new();
    let mut lookups = Vec::new();
    for (i, appid) in ids.iter().enumerate() {
        if env.cancelled() {
            break;
        }
        env.progress(
            i as f64,
            Some(ids.len() as f64),
            &format!("Comparing app {appid} ({} of {})", i + 1, ids.len()),
        );
        let (v, ls) = app_overview_value(
            &ToolEnv {
                core: env.core,
                ctx: &env.ctx.detached(env.ctx.cancel.clone()),
                mrtr: Mrtr::default(),
                task: env.task.clone(),
            },
            *appid,
            args,
        );
        lookups.extend(ls);
        rows.push(obj! {
            "appid" => *appid,
            "name" => v.at(&["details", "name"]).clone(),
            "releaseDate" => v.at(&["details", "releaseDate"]).clone(),
            "price" => v.at(&["details", "price", "final"]).clone(),
            "isFree" => v.at(&["details", "isFree"]).clone(),
            "currentPlayers" => v["currentPlayers"].clone(),
            "positivePercent" => v.at(&["reviews", "positivePercent"]).clone(),
            "totalReviews" => v.at(&["reviews", "totalReviews"]).clone(),
            "reviewScore" => v.at(&["reviews", "reviewScoreDescription"]).clone(),
            "achievementCount" => v.at(&["achievements", "count"]).clone(),
            "genres" => v.at(&["details", "genres"]).clone(),
        });
    }
    env.progress(
        ids.len() as f64,
        Some(ids.len() as f64),
        "Comparison complete",
    );
    let leader = rows
        .iter()
        .max_by_key(|r| r["currentPlayers"].as_u64().unwrap_or(0))
        .and_then(|r| r["name"].as_str().map(String::from))
        .unwrap_or_default();
    let refs: Vec<&Lookup> = lookups.iter().collect();
    ok_result(
        format!(
            "Compared {} apps; most concurrent players: {leader}.",
            rows.len()
        ),
        obj! { "apps" => rows },
        &refs,
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

pub fn tools() -> Vec<ToolDef> {
    let review_filters = vec![
        (
            "filter",
            p_enum(
                "Sort/filter: recent (newest), updated (recently edited), all (by helpfulness; required for day_range).",
                &["recent", "updated", "all"],
                Some("recent"),
            ),
        ),
        (
            "language",
            p_str("Steam language name to filter reviews, or 'all' (default)."),
        ),
        (
            "review_type",
            p_enum(
                "Restrict to positive or negative reviews.",
                &["all", "positive", "negative"],
                Some("all"),
            ),
        ),
        (
            "purchase_type",
            p_enum(
                "Restrict by how the reviewer acquired the game.",
                &["all", "steam", "non_steam_purchase"],
                Some("all"),
            ),
        ),
        (
            "day_range",
            p_int(
                "Only reviews from the last N days (with filter=all).",
                1,
                365,
                None,
            ),
        ),
    ];
    let mut reviews_props = vec![("appid", p_appid())];
    reviews_props.extend(review_filters);
    reviews_props.extend([
        ("num_per_page", p_int("Reviews per page.", 1, 100, Some(20))),
        (
            "max_pages",
            p_int(
                "Follow cursors for up to this many pages (long fetches may run as a task).",
                1,
                50,
                Some(1),
            ),
        ),
        (
            "cursor",
            p_str("Pagination cursor from a previous call's nextCursor (default '*')."),
        ),
        (
            "max_review_chars",
            p_int(
                "Truncate each review's text to this many characters.",
                50,
                8000,
                Some(800),
            ),
        ),
        (
            "filter_offtopic_activity",
            p_bool("Exclude off-topic review bombs (Steam default true).", true),
        ),
        p_force(),
    ]);

    let mut v = vec![
        ToolDef {
            task: true,
            ..def(
                "steam_search_apps",
                "Search Steam Apps",
                "Search the full Steam app catalog (games, DLC, software) by name and return matching app IDs. The catalog is fetched once and cached for a day, so repeated searches are instant. A numeric query looks up an app ID directly.",
                object_schema(
                    vec![
                        (
                            "query",
                            obj! { "type" => "string", "minLength" => 1, "maxLength" => 200, "description" => "App name or partial name, or a numeric app ID." },
                        ),
                        ("limit", p_int("Maximum results.", 1, 100, Some(10))),
                        p_force(),
                    ],
                    &["query"],
                ),
                KeyKind::Public,
                search_apps,
            )
        },
        def(
            "steam_get_app_details",
            "App Store Details",
            "Store page details for an app: name, type, description, developers, publishers, release date, price in the chosen country, platforms, genres, categories, Metacritic score, recommendation and achievement counts.",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("cc", p_cc()),
                    ("language", p_lang()),
                    (
                        "filters",
                        obj! { "type" => "array", "items" => obj! { "type" => "string" }, "description" => "Optional appdetails filters, e.g. [\"basic\",\"price_overview\"]." },
                    ),
                    (
                        "include_raw",
                        p_bool("Also return the full raw appdetails payload.", false),
                    ),
                    p_force(),
                ],
                &["appid"],
            ),
            KeyKind::None,
            app_details,
        ),
        ToolDef {
            task: true,
            ..def(
                "steam_get_app_reviews",
                "App Reviews",
                "User reviews for an app with the overall review summary. Supports filters, pagination via cursor, and multi-page collection.",
                object_schema(reviews_props, &["appid"]),
                KeyKind::None,
                app_reviews,
            )
        },
        def(
            "steam_get_review_summary",
            "Review Score Summary",
            "The aggregate Steam review score for an app (e.g. 'Very Positive'), with positive/negative totals and percentage.",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("language", p_str("Steam language name or 'all' (default).")),
                    (
                        "purchase_type",
                        p_enum(
                            "Which purchases to count.",
                            &["all", "steam", "non_steam_purchase"],
                            Some("all"),
                        ),
                    ),
                    p_force(),
                ],
                &["appid"],
            ),
            KeyKind::None,
            review_summary,
        ),
        ToolDef {
            output_schema: Some(
                obj! { "type" => "object", "properties" => obj! { "appid" => obj! { "type" => "integer" }, "playerCount" => obj! { "type" => "integer" }, "asOf" => obj! { "type" => "string" } }, "required" => crate::arr!["appid", "playerCount", "asOf"] },
            ),
            ..def(
                "steam_get_current_players",
                "Current Player Count",
                "The number of players currently in-game for an app (cached for about a minute).",
                object_schema(vec![("appid", p_appid()), p_force()], &["appid"]),
                KeyKind::None,
                current_players,
            )
        },
        def(
            "steam_get_app_news",
            "App News",
            "Recent news items and patch notes for an app, with markup stripped.",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("count", p_int("Number of items.", 1, 100, Some(10))),
                    (
                        "maxlength",
                        p_int(
                            "Truncate contents to this many characters (0 = full).",
                            0,
                            20000,
                            Some(600),
                        ),
                    ),
                    ("enddate", p_u64("Only items before this Unix timestamp.")),
                    (
                        "feeds",
                        obj! { "type" => "array", "items" => obj! { "type" => "string" }, "description" => "Feed names to include, e.g. [\"steam_community_announcements\"]." },
                    ),
                    (
                        "tags",
                        obj! { "type" => "array", "items" => obj! { "type" => "string" }, "description" => "Only items with these tags, e.g. [\"patchnotes\"]." },
                    ),
                    p_force(),
                ],
                &["appid"],
            ),
            KeyKind::None,
            app_news,
        ),
        def(
            "steam_get_global_achievement_percentages",
            "Global Achievement Rates",
            "Global unlock percentages for every achievement in a game, sorted from most to least common.",
            object_schema(vec![("appid", p_appid()), p_force()], &["appid"]),
            KeyKind::None,
            achievement_pct,
        ),
        def(
            "steam_get_game_schema",
            "Game Stats Schema",
            "Achievement and stat definitions for a game (display names, descriptions, hidden flags). With a publisher key, includes data for unreleased apps you own.",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("language", p_lang()),
                    (
                        "include_icons",
                        p_bool("Include achievement icon URLs.", false),
                    ),
                    p_force(),
                ],
                &["appid"],
            ),
            KeyKind::Public,
            game_schema,
        ),
        def(
            "steam_get_global_stats",
            "Global Aggregated Stats",
            "Global totals for stats a developer flagged as aggregated (e.g. total kills), optionally per day within a date range.",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    (
                        "names",
                        obj! { "type" => "array", "items" => obj! { "type" => "string" }, "minItems" => 1, "maxItems" => 50, "description" => "Aggregated stat API names." },
                    ),
                    (
                        "startdate",
                        obj! { "type" => ["integer", "string"], "description" => "Start (Unix time or YYYY-MM-DD) for daily history." },
                    ),
                    (
                        "enddate",
                        obj! { "type" => ["integer", "string"], "description" => "End (Unix time or YYYY-MM-DD)." },
                    ),
                    p_force(),
                ],
                &["appid", "names"],
            ),
            KeyKind::None,
            global_stats,
        ),
        def(
            "steam_get_player_summaries",
            "Player Profiles",
            "Public profile summaries for up to 100 SteamIDs: persona name, status, visibility, country, current game, account age.",
            object_schema(
                vec![("steamids", p_steamids(100)), p_force()],
                &["steamids"],
            ),
            KeyKind::Public,
            player_summaries,
        ),
        def(
            "steam_resolve_vanity_url",
            "Resolve Vanity URL",
            "Convert a custom profile or group URL name (or full steamcommunity.com URL) into a 64-bit SteamID.",
            object_schema(
                vec![
                    (
                        "vanity",
                        obj! { "type" => "string", "minLength" => 1, "description" => "Vanity name (e.g. 'gabelogannewell') or full profile URL." },
                    ),
                    (
                        "url_type",
                        p_enum(
                            "What kind of URL.",
                            &["individual", "group", "game_group"],
                            Some("individual"),
                        ),
                    ),
                    p_force(),
                ],
                &["vanity"],
            ),
            KeyKind::Public,
            resolve_vanity,
        ),
        def(
            "steam_get_owned_games",
            "Owned Games",
            "A player's game library with playtime, sorted by playtime, recency or name. Requires public game details.",
            object_schema(
                vec![
                    ("steamid", p_steamid()),
                    ("include_appinfo", p_bool("Include names.", true)),
                    (
                        "include_played_free_games",
                        p_bool("Include played free-to-play games.", true),
                    ),
                    ("include_free_sub", p_bool("Include free licenses.", false)),
                    (
                        "appids_filter",
                        obj! { "type" => "array", "items" => obj! { "type" => "integer" }, "description" => "Only these app IDs." },
                    ),
                    (
                        "sort_by",
                        p_enum(
                            "Ordering.",
                            &["playtime", "recent", "name"],
                            Some("playtime"),
                        ),
                    ),
                    (
                        "limit",
                        p_int("Maximum games returned.", 1, 5000, Some(100)),
                    ),
                    p_force(),
                ],
                &["steamid"],
            ),
            KeyKind::Public,
            owned_games,
        ),
        def(
            "steam_get_recently_played_games",
            "Recently Played",
            "Games a player has played in the last two weeks with playtime.",
            object_schema(
                vec![
                    ("steamid", p_steamid()),
                    ("count", p_int("Maximum games.", 1, 100, Some(10))),
                    p_force(),
                ],
                &["steamid"],
            ),
            KeyKind::Public,
            recent_games,
        ),
        def(
            "steam_get_player_achievements",
            "Player Achievements",
            "A player's achievement progress in one game, with unlock times.",
            object_schema(
                vec![
                    ("steamid", p_steamid()),
                    ("appid", p_appid()),
                    ("language", p_lang()),
                    p_force(),
                ],
                &["steamid", "appid"],
            ),
            KeyKind::Public,
            player_achievements,
        ),
        def(
            "steam_get_user_stats_for_game",
            "Player Game Stats",
            "A player's raw stat values and achievements for one game.",
            object_schema(
                vec![("steamid", p_steamid()), ("appid", p_appid()), p_force()],
                &["steamid", "appid"],
            ),
            KeyKind::Public,
            user_stats,
        ),
        def(
            "steam_get_friend_list",
            "Friend List",
            "A player's friends (requires a public friends list), optionally with persona names.",
            object_schema(
                vec![
                    ("steamid", p_steamid()),
                    (
                        "relationship",
                        p_enum("Relationship filter.", &["friend", "all"], Some("friend")),
                    ),
                    (
                        "resolve_profiles",
                        p_bool("Look up persona names for up to 100 friends.", false),
                    ),
                    p_force(),
                ],
                &["steamid"],
            ),
            KeyKind::Public,
            friend_list,
        ),
        def(
            "steam_get_player_bans",
            "Player Bans",
            "VAC, game, community and trade ban status for up to 100 SteamIDs.",
            object_schema(
                vec![("steamids", p_steamids(100)), p_force()],
                &["steamids"],
            ),
            KeyKind::Public,
            player_bans,
        ),
        def(
            "steam_get_steam_level",
            "Steam Level",
            "A player's Steam level.",
            object_schema(vec![("steamid", p_steamid()), p_force()], &["steamid"]),
            KeyKind::Public,
            steam_level,
        ),
        def(
            "steam_get_badges",
            "Badges",
            "A player's badges, XP and level progress.",
            object_schema(
                vec![
                    ("steamid", p_steamid()),
                    (
                        "limit",
                        p_int("Maximum badges returned.", 1, 1000, Some(50)),
                    ),
                    p_force(),
                ],
                &["steamid"],
            ),
            KeyKind::Public,
            badges,
        ),
        def(
            "steam_get_player_profile",
            "Player Profile Overview",
            "One-call profile overview: persona, visibility, status, level, ban status and recently played games.",
            object_schema(vec![("steamid", p_steamid()), p_force()], &["steamid"]),
            KeyKind::Public,
            player_profile,
        ),
        def(
            "steam_get_most_played_games",
            "Most Played Games",
            "Steam's current most-played games chart with rank changes and peak concurrent players.",
            object_schema(
                vec![
                    ("limit", p_int("Number of ranks.", 1, 100, Some(25))),
                    p_force(),
                ],
                &[],
            ),
            KeyKind::None,
            most_played,
        ),
        def(
            "steam_get_workshop_item_details",
            "Workshop Item Details",
            "Details for up to 100 Steam Workshop items: title, description, author, size, subscriptions, favorites, views, tags.",
            object_schema(
                vec![
                    (
                        "publishedfileids",
                        obj! { "type" => "array", "items" => obj! { "type" => "string" }, "minItems" => 1, "maxItems" => 100, "description" => "Workshop item IDs." },
                    ),
                    p_force(),
                ],
                &["publishedfileids"],
            ),
            KeyKind::None,
            workshop_details,
        ),
        def(
            "steam_query_workshop_files",
            "Search Workshop",
            "Search or rank an app's Steam Workshop items (trending, top rated, most subscribed, recently updated, or text search).",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    (
                        "query_type",
                        p_enum(
                            "Ranking.",
                            &QUERY_TYPES.iter().map(|q| q.0).collect::<Vec<_>>(),
                            None,
                        ),
                    ),
                    ("search_text", p_str("Full-text search terms.")),
                    (
                        "required_tags",
                        obj! { "type" => "array", "items" => obj! { "type" => "string" }, "description" => "Items must have all these tags." },
                    ),
                    ("days", p_int("Window for trend rankings.", 1, 365, None)),
                    ("numperpage", p_int("Results per page.", 1, 100, Some(20))),
                    (
                        "cursor",
                        p_str("Cursor from a previous call (default '*')."),
                    ),
                    p_force(),
                ],
                &["appid"],
            ),
            KeyKind::Public,
            query_workshop,
        ),
        def(
            "steam_get_supported_api_list",
            "Supported Web API Methods",
            "Lists Web API interfaces and methods available to the configured key (use host=partner to see partner-only methods), optionally filtered.",
            object_schema(
                vec![
                    (
                        "host",
                        p_enum(
                            "Which API host to inspect.",
                            &["public", "partner"],
                            Some("public"),
                        ),
                    ),
                    (
                        "filter",
                        p_str("Case-insensitive substring of interface or method name."),
                    ),
                    (
                        "include_parameters",
                        p_bool("Include parameter lists.", false),
                    ),
                    p_force(),
                ],
                &[],
            ),
            KeyKind::Optional,
            supported_apis,
        ),
        def(
            "steam_get_store_featured",
            "Store Featured",
            "The Steam store's featured categories: specials, top sellers, new releases and coming soon, for a country.",
            object_schema(vec![("cc", p_cc()), ("language", p_lang()), p_force()], &[]),
            KeyKind::None,
            store_featured,
        ),
        def(
            "steam_get_package_details",
            "Package Details",
            "Store details for packages (subs): name, price, included apps, platforms.",
            object_schema(
                vec![
                    (
                        "packageids",
                        obj! { "type" => "array", "items" => obj! { "type" => "integer" }, "minItems" => 1, "maxItems" => 50, "description" => "Package (sub) IDs." },
                    ),
                    ("cc", p_cc()),
                    p_force(),
                ],
                &["packageids"],
            ),
            KeyKind::None,
            package_details,
        ),
        def(
            "steam_get_wishlist",
            "Wishlist",
            "A player's public wishlist with priorities and dates added.",
            object_schema(
                vec![
                    ("steamid", p_steamid()),
                    ("limit", p_int("Maximum items.", 1, 5000, Some(200))),
                    p_force(),
                ],
                &["steamid"],
            ),
            KeyKind::Optional,
            wishlist,
        ),
        def(
            "steam_get_servers_at_address",
            "Servers at Address",
            "Steam game servers registered at an IP or IP:port.",
            object_schema(
                vec![
                    (
                        "addr",
                        obj! { "type" => "string", "minLength" => 3, "description" => "IP address or IP:port." },
                    ),
                    p_force(),
                ],
                &["addr"],
            ),
            KeyKind::None,
            servers_at_address,
        ),
        def(
            "steam_check_app_up_to_date",
            "Version Up-To-Date Check",
            "Checks whether a game server/client version number is current for an app.",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("version", p_u64("Version number reported by the build.")),
                    p_force(),
                ],
                &["appid", "version"],
            ),
            KeyKind::None,
            up_to_date,
        ),
        def(
            "steam_get_game_servers",
            "Game Server List",
            "Live game servers from the master server list, with player totals. Filter with master-server syntax or by app ID.",
            object_schema(
                vec![
                    ("appid", p_appid_plain("Shortcut for filter \\appid\\N.")),
                    (
                        "filter",
                        p_str("Master server filter, e.g. \\appid\\730\\dedicated\\1\\empty\\1."),
                    ),
                    ("limit", p_int("Maximum servers.", 1, 20000, Some(100))),
                    p_force(),
                ],
                &[],
            ),
            KeyKind::Public,
            game_servers,
        ),
        def(
            "steam_get_app_overview",
            "App Overview",
            "One-call health snapshot of an app: store details, current players, review score, latest news and achievement statistics. Each part is cached independently.",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("cc", p_cc()),
                    ("language", p_lang()),
                    p_force(),
                ],
                &["appid"],
            ),
            KeyKind::None,
            app_overview,
        ),
        ToolDef {
            task: true,
            ..def(
                "steam_compare_apps",
                "Compare Apps",
                "Side-by-side comparison of 2–10 apps: price, release date, current players, review score, achievement count and genres.",
                object_schema(
                    vec![
                        (
                            "appids",
                            obj! { "type" => "array", "items" => obj! { "type" => "integer", "minimum" => 1 }, "minItems" => 2, "maxItems" => 10, "description" => "App IDs to compare." },
                        ),
                        ("cc", p_cc()),
                        ("language", p_lang()),
                        p_force(),
                    ],
                    &["appids"],
                ),
                KeyKind::None,
                compare_apps,
            )
        },
    ];
    for t in v.iter_mut() {
        t.description = t.description.trim().to_string();
    }
    v
}
