//! Steamworks partner tools (publisher Web API key) and financial reporting
//! (Financial API group key). Partner calls go to `partner.steam-api.com`.

use std::collections::BTreeMap;
use std::time::Duration;

use super::*;
use crate::mcp::context::scopes;
use crate::steam::{Call, Host, KeyKind, parse_steamid};
use crate::try_steam;

fn get(env: &ToolEnv, call: Call, args: &Map) -> Result<Lookup, SteamError> {
    env.steam().call(&call, force(args))
}

fn pcall(path: &str, ttl: &'static str) -> Call {
    Call::get(Host::Partner, path, KeyKind::Publisher, ttl)
}

fn need_steamid(args: &Map, key: &str) -> Result<u64, ToolOutcome> {
    let raw = args
        .get(key)
        .map(|v| v.to_param_string())
        .unwrap_or_default();
    parse_steamid(&raw).ok_or_else(|| tool_error(format!("'{raw}' is not a valid 64-bit SteamID.")))
}

macro_rules! tryo {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(o) => return o,
        }
    };
}

/// Flattens Steam's VDF-derived `{name: {...}}` maps into arrays.
fn keyed_map_to_array(v: &Value, key_field: &str) -> Vec<Value> {
    match v {
        Value::Object(m) => m
            .iter()
            .map(|(k, item)| {
                let mut item = item.clone();
                if item.is_object() {
                    item.set(key_field, k.as_str());
                    item
                } else {
                    obj! { key_field => k.as_str(), "value" => item }
                }
            })
            .collect(),
        Value::Array(a) => a.clone(),
        _ => Vec::new(),
    }
}

fn steam_result_ok(resp: &Value) -> bool {
    resp["result"].as_i64().is_none_or(|r| r == 1)
}

fn partner_apps_list(env: &ToolEnv, args: &Map) -> Result<(Vec<Value>, Lookup), SteamError> {
    let types = arg_list(args, "type_filter");
    let call = pcall("/ISteamApps/GetPartnerAppListForWebAPIKey/v2/", "medium")
        .opt_param("type_filter", (!types.is_empty()).then(|| types.join(",")));
    let l = get(env, call, args)?;
    let raw = l.value.at(&["applist", "apps", "app"]);
    let raw = if raw.is_null() {
        l.value.at(&["applist", "apps"])
    } else {
        raw
    };
    let apps = match raw {
        Value::Array(a) => a.clone(),
        Value::Object(_) => vec![raw.clone()],
        _ => Vec::new(),
    };
    Ok((apps, l))
}

/// Partner app list, shared with resources and completion.
pub fn partner_apps(env: &ToolEnv) -> Result<Vec<Value>, SteamError> {
    partner_apps_list(env, &Map::new()).map(|(a, _)| a)
}

fn list_apps(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let (apps, l) = try_steam!(partner_apps_list(env, args));
    let mut by_type: BTreeMap<String, u64> = BTreeMap::new();
    for a in &apps {
        *by_type
            .entry(a["app_type"].as_str().unwrap_or("unknown").to_string())
            .or_default() += 1;
    }
    let shaped: Vec<Value> = apps.iter().map(|a| obj! { "appid" => a["appid"].clone(), "name" => a["app_name"].clone(), "type" => a["app_type"].clone() }).collect();
    let breakdown: Vec<String> = by_type.iter().map(|(k, v)| format!("{v} {k}")).collect();
    ok_result(
        format!(
            "The publisher key can access {} app(s): {}.",
            shaped.len(),
            breakdown.join(", ")
        ),
        obj! { "count" => shaped.len(), "byType" => Value::Object(by_type.into_iter().map(|(k, v)| (k, Value::from(v))).collect()), "apps" => shaped },
        &[&l],
    )
}

fn check_ownership(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(need_steamid(args, "steamid"));
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let l = try_steam!(get(
        env,
        pcall("/ISteamUser/CheckAppOwnership/v2/", "volatile")
            .param("steamid", steamid)
            .param("appid", appid),
        args
    ));
    let o = l.value["appownership"].clone();
    let owns = o["ownsapp"].as_bool() == Some(true);
    ok_result(
        format!(
            "{steamid} {} app {appid}{}.",
            if owns { "owns" } else { "does not own" },
            if o["ownersteamid"]
                .as_str()
                .is_some_and(|x| x != steamid.to_string())
            {
                format!(
                    " (licensed via family sharing from {})",
                    o["ownersteamid"].as_str().unwrap_or("")
                )
            } else {
                String::new()
            }
        ),
        o,
        &[&l],
    )
}

fn publisher_ownership(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(need_steamid(args, "steamid"));
    let l = try_steam!(get(
        env,
        pcall("/ISteamUser/GetPublisherAppOwnership/v3/", "volatile").param("steamid", steamid),
        args
    ));
    let apps = l.value.at(&["appownership", "apps"]).clone();
    let owned = apps
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|x| x["ownsapp"].as_bool() == Some(true))
                .count()
        })
        .unwrap_or(0);
    ok_result(
        format!("{steamid} owns {owned} of the publisher's apps."),
        obj! { "steamid" => steamid.to_string(), "apps" => apps },
        &[&l],
    )
}

fn ownership_changes(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let pkg = arg_str(args, "packagerowversion").unwrap_or("0");
    let cdk = arg_str(args, "cdkeyrowversion").unwrap_or("0");
    let l = try_steam!(get(
        env,
        pcall(
            "/ISteamUser/GetPublisherAppOwnershipChanges/v1/",
            "volatile"
        )
        .param("packagerowversion", pkg)
        .param("cdkeyrowversion", cdk),
        args
    ));
    let r = &l.value["ownershipchanges"];
    let changes = r["ownershipchanges"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    ok_result(
        format!(
            "{changes} ownership change(s) since the given row versions; more available: {}.",
            r["moredata"]
        ),
        r.clone(),
        &[&l],
    )
}

fn price_info(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(need_steamid(args, "steamid"));
    let appids = arg_list(args, "appids");
    if appids.is_empty() {
        return tool_error("'appids' must contain at least one app ID.");
    }
    let l = try_steam!(get(
        env,
        pcall("/ISteamUser/GetAppPriceInfo/v1/", "short")
            .param("steamid", steamid)
            .param("appids", appids.join(",")),
        args
    ));
    ok_result(
        format!(
            "Price information for {} app(s) as seen by {steamid}.",
            appids.len()
        ),
        l.value["result"]
            .clone()
            .with("steamid", steamid.to_string()),
        &[&l],
    )
}

fn deleted_steamids(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let rv = arg_str(args, "rowversion").unwrap_or("0");
    let l = try_steam!(get(
        env,
        pcall("/ISteamUser/GetDeletedSteamIDs/v1/", "medium").param("rowversion", rv),
        args
    ));
    let r = l.value["response"].clone();
    ok_result(
        format!(
            "{} deleted account(s) since row version {rv}.",
            r["deletedids"].as_array().map(|a| a.len()).unwrap_or(0)
        ),
        r,
        &[&l],
    )
}

fn app_betas(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let l = try_steam!(get(
        env,
        pcall("/ISteamApps/GetAppBetas/v1/", "partner_live").param("appid", appid),
        args
    ));
    let r = &l.value["response"];
    if !steam_result_ok(r) {
        return tool_error(format!(
            "Steam could not list betas for app {appid} (result {}).",
            r["result"]
        ));
    }
    let betas = keyed_map_to_array(&r["betas"], "name");
    let names: Vec<String> = betas
        .iter()
        .map(|b| {
            format!(
                "{} (build {})",
                b["name"].as_str().unwrap_or("?"),
                b["BuildID"]
            )
        })
        .collect();
    ok_result(
        format!(
            "App {appid} has {} branch(es): {}.",
            betas.len(),
            names.join(", ")
        ),
        obj! { "appid" => appid, "betas" => betas },
        &[&l],
    )
}

fn app_builds(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let count = arg_u64(args, "count").unwrap_or(10).clamp(1, 500);
    let l = try_steam!(get(
        env,
        pcall("/ISteamApps/GetAppBuilds/v1/", "partner_live")
            .param("appid", appid)
            .param("count", count),
        args
    ));
    let r = &l.value["response"];
    if !steam_result_ok(r) {
        return tool_error(format!(
            "Steam could not list builds for app {appid} (result {}).",
            r["result"]
        ));
    }
    let mut builds = keyed_map_to_array(&r["builds"], "id");
    builds.sort_by_key(|b| std::cmp::Reverse(b["BuildID"].as_u64_lenient().unwrap_or(0)));
    for b in builds.iter_mut() {
        if let Some(t) = b["CreationTime"].as_u64() {
            b.set("createdAt", crate::util::time::iso8601_secs(t));
        }
    }
    let latest = builds
        .first()
        .map(|b| {
            format!(
                "latest build {} — {}",
                b["BuildID"],
                b["Description"].as_str().unwrap_or("")
            )
        })
        .unwrap_or_else(|| "no builds".into());
    ok_result(
        format!("App {appid}: {} build(s); {latest}.", builds.len()),
        obj! { "appid" => appid, "builds" => builds },
        &[&l],
    )
}

fn depot_versions(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let l = try_steam!(get(
        env,
        pcall("/ISteamApps/GetAppDepotVersions/v1/", "partner_live").param("appid", appid),
        args
    ));
    let r = l.value["response"].clone();
    ok_result(
        format!("Depot versions for app {appid}."),
        r.with("appid", appid),
        &[&l],
    )
}

fn players_banned(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let l = try_steam!(get(
        env,
        pcall("/ISteamApps/GetPlayersBanned/v1/", "short").param("appid", appid),
        args
    ));
    let bans = l.value.at(&["response", "bans"]).clone();
    ok_result(
        format!(
            "{} banned player record(s) for app {appid}.",
            bans.as_array().map(|a| a.len()).unwrap_or(0)
        ),
        obj! { "appid" => appid, "bans" => bans, "raw" => l.value.as_ref().clone() },
        &[&l],
    )
}

fn to_unix(v: Option<&Value>) -> Option<u64> {
    v.and_then(|v| {
        v.as_u64_lenient()
            .or_else(|| v.as_str().and_then(crate::util::time::parse_datetime))
    })
}

fn cheating_reports(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let now = crate::util::time::now_secs();
    let begin = to_unix(args.get("timebegin")).unwrap_or(now.saturating_sub(7 * 86_400));
    let end = to_unix(args.get("timeend")).unwrap_or(now);
    if end <= begin {
        return tool_error("'timeend' must be after 'timebegin'.");
    }
    let call = pcall("/ISteamApps/GetCheatingReports/v1/", "volatile")
        .param("appid", appid)
        .param("timebegin", begin)
        .param("timeend", end)
        .param("includereports", arg_bool(args, "includereports", true))
        .param("includebans", arg_bool(args, "includebans", true))
        .param("reportidmin", arg_u64(args, "reportidmin").unwrap_or(0));
    let l = try_steam!(get(env, call, args));
    let r = l.value["response"].clone();
    ok_result(
        format!(
            "Cheating reports for app {appid} between {} and {}: {} report(s).",
            crate::util::time::iso8601_secs(begin),
            crate::util::time::iso8601_secs(end),
            r["reports"].as_array().map(|a| a.len()).unwrap_or(0)
        ),
        r.with("appid", appid),
        &[&l],
    )
}

fn partner_server_list(env: &ToolEnv, args: &Map) -> ToolOutcome {
    // `appid` is required by Steam (verified live 2026-11-22).
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let filter = arg_str(args, "filter").unwrap_or("");
    let limit = arg_u64(args, "limit").unwrap_or(100).clamp(1, 20_000);
    let l = try_steam!(get(
        env,
        pcall("/ISteamApps/GetServerList/v1/", "volatile")
            .param("appid", appid)
            .param("filter", filter)
            .param("limit", limit),
        args
    ));
    let servers = l.value.at(&["response", "servers"]).clone();
    ok_result(
        format!(
            "{} server(s) returned.",
            servers.as_array().map(|a| a.len()).unwrap_or(0)
        ),
        obj! { "appid" => appid, "filter" => filter, "servers" => servers },
        &[&l],
    )
}

fn leaderboards(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let l = try_steam!(get(
        env,
        pcall("/ISteamLeaderboards/GetLeaderboardsForGame/v2/", "medium").param("appid", appid),
        args
    ));
    let boards = l.value.at(&["response", "leaderboards"]).clone();
    ok_result(
        format!(
            "App {appid} has {} leaderboard(s).",
            boards.as_array().map(|a| a.len()).unwrap_or(0)
        ),
        obj! { "appid" => appid, "leaderboards" => boards },
        &[&l],
    )
}

fn leaderboard_entries(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let board = arg_u64(args, "leaderboardid").unwrap_or(0);
    let start = arg_u64(args, "rangestart").unwrap_or(0);
    let end = arg_u64(args, "rangeend").unwrap_or(start + 99);
    if end < start || end - start > 5000 {
        return tool_error(
            "'rangeend' must be >= 'rangestart' and the range at most 5000 entries.",
        );
    }
    let request = match arg_str(args, "datarequest").unwrap_or("global") {
        "around_user" => "RequestAroundUser",
        "friends" => "RequestFriends",
        _ => "RequestGlobal",
    };
    let mut call = pcall("/ISteamLeaderboards/GetLeaderboardEntries/v1/", "short")
        .param("appid", appid)
        .param("leaderboardid", board)
        .param("rangestart", start)
        .param("rangeend", end)
        .param("datarequest", request);
    if request != "RequestGlobal" {
        let steamid = tryo!(need_steamid(args, "steamid"));
        call = call.param("steamid", steamid);
    }
    let l = try_steam!(get(env, call, args));
    let r = l.value["leaderboardEntryInformation"].clone();
    let r = if r.is_null() {
        l.value["response"].clone()
    } else {
        r
    };
    ok_result(
        format!("Leaderboard {board} entries {start}–{end} for app {appid}."),
        r.with("appid", appid),
        &[&l],
    )
}

fn microtxn_iface(args: &Map) -> &'static str {
    if arg_bool(args, "sandbox", false) {
        "ISteamMicroTxnSandbox"
    } else {
        "ISteamMicroTxn"
    }
}

fn microtxn_report(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let kind = arg_str(args, "type").unwrap_or("GAMESALES");
    let time = match args
        .get("time")
        .and_then(|v| v.as_str())
        .and_then(crate::util::time::parse_datetime)
    {
        Some(t) => crate::util::time::iso8601_secs(t),
        None => {
            crate::util::time::iso8601_secs(crate::util::time::now_secs().saturating_sub(86_400))
        }
    };
    let max = arg_u64(args, "maxresults").unwrap_or(1000).clamp(1, 10_000);
    let call = pcall(&format!("/{}/GetReport/v5/", microtxn_iface(args)), "short")
        .param("appid", appid)
        .param("type", kind)
        .param("time", &time)
        .param("maxresults", max);
    let l = try_steam!(get(env, call, args));
    let r = &l.value["response"];
    if r["result"].as_str().is_some_and(|s| s != "OK") {
        return tool_error(format!(
            "Steam returned {}: {}",
            r["result"],
            r.at(&["error", "errordesc"]).as_str().unwrap_or("")
        ));
    }
    let orders = r.at(&["params", "orders"]).clone();
    ok_result(
        format!(
            "{kind} report for app {appid} since {time}: {} order(s).",
            orders.as_array().map(|a| a.len()).unwrap_or(0)
        ),
        r["params"]
            .clone()
            .with("appid", appid)
            .with("type", kind)
            .with("since", time),
        &[&l],
    )
}

fn query_txn(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let orderid = arg_str(args, "orderid")
        .map(String::from)
        .or_else(|| arg_u64(args, "orderid").map(|v| v.to_string()));
    let transid = arg_str(args, "transid")
        .map(String::from)
        .or_else(|| arg_u64(args, "transid").map(|v| v.to_string()));
    if orderid.is_none() && transid.is_none() {
        return tool_error("Provide 'orderid' or 'transid'.");
    }
    let call = pcall(
        &format!("/{}/QueryTxn/v3/", microtxn_iface(args)),
        "volatile",
    )
    .param("appid", appid)
    .opt_param("orderid", orderid)
    .opt_param("transid", transid);
    let l = try_steam!(get(env, call, args));
    let r = &l.value["response"];
    ok_result(
        format!(
            "Transaction status: {}.",
            r.at(&["params", "status"])
                .as_str()
                .unwrap_or(r["result"].as_str().unwrap_or("unknown"))
        ),
        r.clone(),
        &[&l],
    )
}

fn item_definitions(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let meta = try_steam!(get(
        env,
        pcall("/IInventoryService/GetItemDefMeta/v1/", "medium").param("appid", appid),
        args
    ));
    let Some(digest) = meta
        .value
        .at(&["response", "digest"])
        .as_str()
        .map(String::from)
    else {
        return tool_error(format!(
            "App {appid} has no published item definitions (no digest)."
        ));
    };
    // The archive is immutable per digest, so it can be cached for a long time.
    let archive = try_steam!(get(
        env,
        Call::get(
            Host::Public,
            "/IGameInventory/GetItemDefArchive/v1/",
            KeyKind::None,
            "static"
        )
        .param("appid", appid)
        .param("digest", &digest),
        args
    ));
    let all = archive.value.as_array().cloned().unwrap_or_default();
    let max = arg_u64(args, "max_items").unwrap_or(200).clamp(1, 50_000) as usize;
    let filter = arg_str(args, "filter").map(|s| s.to_ascii_lowercase());
    let items: Vec<Value> = all
        .iter()
        .filter(|i| {
            filter
                .as_ref()
                .is_none_or(|f| i.to_string().to_ascii_lowercase().contains(f))
        })
        .take(max)
        .cloned()
        .collect();
    ok_result(
        format!(
            "App {appid}: {} item definition(s) (digest {digest}), {} returned.",
            all.len(),
            items.len()
        ),
        obj! { "appid" => appid, "digest" => digest, "modified" => meta.value.at(&["response", "modified"]).clone(), "total" => all.len(), "items" => items },
        &[&meta, &archive],
    )
}

fn inventory(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let steamid = tryo!(need_steamid(args, "steamid"));
    let l = try_steam!(get(
        env,
        pcall("/IInventoryService/GetInventory/v1/", "volatile")
            .param("appid", appid)
            .param("steamid", steamid),
        args
    ));
    let r = l.value["response"].clone();
    // item_json is a JSON string; decode it for convenience.
    let items = r["item_json"]
        .as_str()
        .and_then(|s| crate::json::parse(s).ok())
        .unwrap_or(Value::Null);
    ok_result(
        format!(
            "Inventory of {steamid} in app {appid}: {} item(s).",
            items.as_array().map(|a| a.len()).unwrap_or(0)
        ),
        obj! { "appid" => appid, "steamid" => steamid.to_string(), "items" => items, "raw" => r },
        &[&l],
    )
}

fn authenticate_ticket(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let ticket = arg_str(args, "ticket").unwrap_or("");
    if ticket.is_empty() || !ticket.bytes().all(|b| b.is_ascii_hexdigit()) {
        return tool_error(
            "'ticket' must be the hex-encoded session ticket from GetAuthSessionTicket / GetAuthTicketForWebApi.",
        );
    }
    // Tickets are single-use credentials: never cache.
    let call = pcall("/ISteamUserAuth/AuthenticateUserTicket/v1/", "volatile")
        .param("appid", appid)
        .param("ticket", ticket)
        .opt_param("identity", arg_str(args, "identity"));
    let v = try_steam!(env.steam().call_uncached(&call));
    let r = &v["response"];
    if let Some(err) = r.get("error") {
        return tool_error(format!(
            "Ticket rejected: {} ({})",
            err["errordesc"].as_str().unwrap_or("unknown"),
            err["errorcode"]
        ));
    }
    let p = r["params"].clone();
    ok_result(
        format!(
            "Ticket valid for {} (owner {}), VAC banned: {}, publisher banned: {}.",
            p["steamid"].as_str().unwrap_or("?"),
            p["ownersteamid"].as_str().unwrap_or("?"),
            p["vacbanned"],
            p["publisherbanned"]
        ),
        p,
        &[],
    )
}

fn asset_prices(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let call = pcall("/ISteamEconomy/GetAssetPrices/v1/", "medium")
        .param("appid", appid)
        .opt_param("currency", arg_str(args, "currency"))
        .opt_param("language", arg_str(args, "language"));
    let l = try_steam!(get(env, call, args));
    let r = l.value["result"].clone();
    ok_result(
        format!(
            "{} priced asset(s) for app {appid}.",
            r["assets"].as_array().map(|a| a.len()).unwrap_or(0)
        ),
        r.with("appid", appid),
        &[&l],
    )
}

// ---------------------------------------------------------------------------
// Financial reporting
// ---------------------------------------------------------------------------

fn fcall(path: &str, ttl: &'static str) -> Call {
    Call::get(Host::Partner, path, KeyKind::Financial, ttl)
}

/// Dates with changed sales data since a high-watermark, shared with completion.
pub fn changed_dates(
    env: &ToolEnv,
    highwatermark: &str,
    force_refresh: bool,
) -> Result<(Vec<String>, String, Lookup), SteamError> {
    let l = env.steam().call(
        &fcall(
            "/IPartnerFinancialsService/GetChangedDatesForPartner/v001/",
            "short",
        )
        .param("highwatermark", highwatermark),
        force_refresh,
    )?;
    let r = &l.value["response"];
    let dates = r["dates"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| d.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let hw = r["result_highwatermark"].to_param_string();
    Ok((dates, hw, l))
}

fn changed_sales_dates(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let hw = arg_str(args, "highwatermark").unwrap_or("0");
    let (dates, next, l) = try_steam!(changed_dates(env, hw, force(args)));
    ok_result(
        format!(
            "{} date(s) with new or changed sales data since watermark {hw}; next watermark {next}.",
            dates.len()
        ),
        obj! { "dates" => dates, "resultHighwatermark" => next },
        &[&l],
    )
}

struct SalesPages {
    results: Vec<Value>,
    lookups: BTreeMap<String, Vec<Value>>,
    pages: u64,
    last_max_id: String,
    complete: bool,
    cache: Vec<Lookup>,
}

/// Fetches detailed sales for a date, following `max_id` pagination.
fn fetch_sales(
    env: &ToolEnv,
    date: &str,
    start_hw: &str,
    all_pages: bool,
    max_pages: u64,
    args: &Map,
) -> Result<SalesPages, SteamError> {
    let mut hw = start_hw.to_string();
    let mut out = SalesPages {
        results: Vec::new(),
        lookups: BTreeMap::new(),
        pages: 0,
        last_max_id: hw.clone(),
        complete: false,
        cache: Vec::new(),
    };
    let mut budget = max_pages;
    loop {
        if env.cancelled() {
            break;
        }
        let call = fcall(
            "/IPartnerFinancialsService/GetDetailedSales/v001/",
            "financial",
        )
        .param("date", date)
        .param("highwatermark_id", &hw);
        let l = env.steam().call(&call, force(args))?;
        let r = &l.value["response"];
        let page_results = r["results"].as_array().cloned().unwrap_or_default();
        let n = page_results.len();
        out.results.extend(page_results);
        for (k, v) in r
            .as_object()
            .map(|m| m.iter().collect::<Vec<_>>())
            .unwrap_or_default()
        {
            if k.ends_with("_info") {
                if let Some(a) = v.as_array() {
                    out.lookups
                        .entry(k.clone())
                        .or_default()
                        .extend(a.iter().cloned());
                }
            }
        }
        let max_id = r["max_id"].to_param_string();
        out.pages += 1;
        out.cache.push(l);
        env.progress(
            out.pages as f64,
            None,
            &format!(
                "Fetched sales page {} for {date} ({} rows so far)",
                out.pages,
                out.results.len()
            ),
        );
        if n == 0 || max_id.is_empty() || max_id == hw {
            out.complete = true;
            out.last_max_id = max_id;
            break;
        }
        out.last_max_id = max_id.clone();
        hw = max_id;
        if !all_pages {
            break;
        }
        budget -= 1;
        if budget == 0 {
            // When running as a task with an elicitation-capable client, ask
            // whether to keep going instead of silently truncating.
            let can_ask = env.task.is_some() && env.ctx.supports_elicitation_form();
            if can_ask {
                let key = format!("continue_after_{}", out.pages);
                let request = obj! {
                    "method" => "elicitation/create",
                    "params" => obj! {
                        "mode" => "form",
                        "message" => format!("Fetched {} pages ({} rows) of sales for {date}. Continue fetching up to {max_pages} more pages?", out.pages, out.results.len()),
                        "requestedSchema" => obj! { "type" => "object", "properties" => obj! { "continue" => obj! { "type" => "boolean", "title" => "Continue fetching", "default" => true } }, "required" => crate::arr!["continue"] },
                    },
                };
                let task = env.task.as_ref().map(|t| t.clone());
                let answer =
                    task.and_then(|t| t.request_input(&key, request, Duration::from_secs(600)));
                let go = answer.is_some_and(|a| {
                    a["action"].as_str() == Some("accept")
                        && a.at(&["content", "continue"]).as_bool() != Some(false)
                });
                if go {
                    budget = max_pages;
                    continue;
                }
            }
            break;
        }
    }
    Ok(out)
}

fn detailed_sales(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let Some(date) = arg_str(args, "date") else {
        return tool_error(
            "'date' is required (use a date returned by steam_partner_get_changed_sales_dates).",
        );
    };
    let hw = arg_str(args, "highwatermark_id").unwrap_or("0");
    let all = arg_bool(args, "all_pages", false);
    let max_pages = arg_u64(args, "max_pages").unwrap_or(20).clamp(1, 500);
    let max_rows = arg_u64(args, "max_rows")
        .unwrap_or(1000)
        .clamp(1, 1_000_000) as usize;
    let s = try_steam!(fetch_sales(env, date, hw, all, max_pages, args));
    let total = s.results.len();
    let rows: Vec<Value> = s.results.into_iter().take(max_rows).collect();
    let refs: Vec<&Lookup> = s.cache.iter().collect();
    ok_result(
        format!(
            "{total} sales row(s) for {date} across {} page(s){}; next highwatermark_id {}.",
            s.pages,
            if s.complete { "" } else { " (more available)" },
            s.last_max_id
        ),
        obj! {
            "date" => date,
            "pages" => s.pages,
            "complete" => s.complete,
            "maxId" => s.last_max_id,
            "totalRows" => total,
            "truncated" => total > rows.len(),
            "results" => rows,
            "lookupTables" => Value::Object(s.lookups.into_iter().map(|(k, v)| (k, Value::Array(v))).collect()),
        },
        &refs,
    )
}

fn sales_summary(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let Some(date) = arg_str(args, "date") else {
        return tool_error("'date' is required.");
    };
    let group_by = {
        let g = arg_list(args, "group_by");
        if g.is_empty() {
            vec!["primary_appid".to_string()]
        } else {
            g
        }
    };
    let top = arg_u64(args, "top").unwrap_or(25).clamp(1, 1000) as usize;
    let s = try_steam!(fetch_sales(
        env,
        date,
        "0",
        true,
        arg_u64(args, "max_pages").unwrap_or(100).clamp(1, 1000),
        args
    ));
    // App and package names from lookup tables.
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    for (table, id_field, name_field) in [
        ("app_info", "appid", "app_name"),
        ("package_info", "packageid", "package_name"),
        ("bundle_info", "bundleid", "bundle_name"),
        ("country_info", "country_code", "country_name"),
    ] {
        for item in s.lookups.get(table).into_iter().flatten() {
            if let (id, Some(name)) = (item[id_field].to_param_string(), item[name_field].as_str())
            {
                names.insert(format!("{id_field}:{id}"), name.to_string());
            }
        }
    }
    let is_metric = |k: &str| k.ends_with("_usd") || k.contains("units");
    let mut groups: BTreeMap<String, (Map, BTreeMap<String, f64>, u64)> = BTreeMap::new();
    let mut totals: BTreeMap<String, f64> = BTreeMap::new();
    for row in &s.results {
        let key_parts: Vec<String> = group_by
            .iter()
            .map(|g| row[g.as_str()].to_param_string())
            .collect();
        let key = key_parts.join("|");
        let entry = groups.entry(key).or_insert_with(|| {
            let mut m = Map::new();
            for (g, v) in group_by.iter().zip(&key_parts) {
                m.insert(g.as_str(), v.as_str());
                let lookup_field = match g.as_str() {
                    "primary_appid" | "appid" => Some("appid"),
                    "packageid" => Some("packageid"),
                    "bundleid" => Some("bundleid"),
                    "country_code" => Some("country_code"),
                    _ => None,
                };
                if let Some(n) = lookup_field.and_then(|f| names.get(&format!("{f}:{v}"))) {
                    m.insert(format!("{g}_name"), n.as_str());
                }
            }
            (m, BTreeMap::new(), 0)
        });
        entry.2 += 1;
        if let Some(obj) = row.as_object() {
            for (k, v) in obj.iter() {
                if is_metric(k) {
                    if let Some(x) = v.as_f64_lenient() {
                        *entry.1.entry(k.clone()).or_default() += x;
                        *totals.entry(k.clone()).or_default() += x;
                    }
                }
            }
        }
    }
    let rank_metric = [
        "net_sales_usd",
        "gross_sales_usd",
        "net_units_sold",
        "gross_units_sold",
    ]
    .into_iter()
    .find(|m| totals.contains_key(*m))
    .unwrap_or("rows");
    let mut rows: Vec<(f64, Value)> = groups
        .into_values()
        .map(|(mut keys, metrics, count)| {
            let rank = if rank_metric == "rows" {
                count as f64
            } else {
                metrics.get(rank_metric).copied().unwrap_or(0.0)
            };
            for (k, v) in metrics {
                keys.insert(k, (v * 100.0).round() / 100.0);
            }
            keys.insert("rows", count);
            (rank, Value::Object(keys))
        })
        .collect();
    rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let group_count = rows.len();
    let top_rows: Vec<Value> = rows.into_iter().take(top).map(|(_, v)| v).collect();
    let totals_v: Map = totals
        .iter()
        .map(|(k, v)| (k.clone(), Value::from((v * 100.0).round() / 100.0)))
        .collect();
    let headline = [
        "net_sales_usd",
        "gross_sales_usd",
        "net_units_sold",
        "gross_units_sold",
    ]
    .iter()
    .filter_map(|k| totals.get(*k).map(|v| format!("{k} {v:.2}")))
    .collect::<Vec<_>>()
    .join(", ");
    let refs: Vec<&Lookup> = s.cache.iter().collect();
    ok_result(
        format!(
            "Sales summary for {date}: {} rows in {group_count} group(s) by {}; totals: {}.",
            s.results.len(),
            group_by.join("+"),
            if headline.is_empty() {
                "n/a".into()
            } else {
                headline
            }
        ),
        obj! { "date" => date, "groupBy" => group_by, "rankedBy" => rank_metric, "complete" => s.complete, "pages" => s.pages, "rows" => s.results.len(), "groups" => group_count, "totals" => Value::Object(totals_v), "top" => top_rows },
        &refs,
    )
}

// ---------------------------------------------------------------------------
// Writes (gated)
// ---------------------------------------------------------------------------

fn set_build_live(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let cfg = env.cfg();
    if !cfg.enable_writes {
        return tool_error(
            "State-changing tools are disabled. Set STEAM_MCP_ENABLE_WRITES=true to allow them.",
        );
    }
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let buildid = arg_u64(args, "buildid").unwrap_or(0);
    let betakey = arg_str(args, "betakey").unwrap_or("public");
    let description = arg_str(args, "description");
    if !env.ctx.supports_elicitation_form() {
        if env.ctx.supports_elicitation_url() {
            return steamworks_handoff(env, appid, buildid, betakey);
        }
        return ToolOutcome::Error(McpError::missing_capability(
            obj! { "elicitation" => obj! { "form" => obj! {} } },
            "setting a build live requires explicit user confirmation via form elicitation",
        ));
    }

    let confirmed_step = env
        .mrtr
        .state
        .as_ref()
        .is_some_and(|s| s["step"].as_str() == Some("confirm"));
    if let (true, Some(answer)) = (confirmed_step, env.mrtr.response("confirm")) {
        match answer["action"].as_str() {
            Some("accept") => {
                let typed = answer
                    .at(&["content", "appid_confirmation"])
                    .as_u64_lenient();
                if answer.at(&["content", "confirm"]).as_bool() == Some(true)
                    && typed == Some(appid)
                {
                    let call = Call::post(
                        Host::Partner,
                        "/ISteamApps/SetAppBuildLive/v2/",
                        KeyKind::Publisher,
                        "volatile",
                    )
                    .param("appid", appid)
                    .param("buildid", buildid)
                    .param("betakey", betakey)
                    .opt_param("description", description);
                    let v = try_steam!(env.steam().call_uncached(&call));
                    let prefix = "partner:/ISteamApps/GetApp";
                    let purged = env.steam().cache.invalidate_where(|k| {
                        k.starts_with(prefix) && k.contains(&format!("appid={appid}"))
                    });
                    crate::log_info!(
                        "audit",
                        "build set live",
                        appid = appid,
                        buildid = buildid,
                        betakey = betakey,
                        principal = env.ctx.principal.id
                    );
                    return ok_result(
                        format!(
                            "Build {buildid} is now live on branch '{betakey}' for app {appid} ({purged} cached entries refreshed)."
                        ),
                        obj! { "appid" => appid, "buildid" => buildid, "betakey" => betakey, "response" => v },
                        &[],
                    );
                }
                // Mismatched confirmation: ask again below.
            }
            _ => {
                return tool_error(
                    "The change was not confirmed by the user; no build was changed.",
                );
            }
        }
    }
    // Show the current state of the branch in the confirmation prompt.
    let current = env
        .steam()
        .call(
            &pcall("/ISteamApps/GetAppBetas/v1/", "partner_live").param("appid", appid),
            false,
        )
        .ok()
        .and_then(|l| {
            l.value
                .at(&["response", "betas", betakey, "BuildID"])
                .as_u64_lenient()
        });
    let message = format!(
        "Set build {buildid} live on branch '{betakey}' for app {appid}?{} Players on this branch will download it. Type the app ID to confirm.",
        current
            .map(|c| format!(" The branch currently runs build {c}."))
            .unwrap_or_default()
    );
    let mut requests = Map::new();
    requests.insert("confirm", obj! {
        "method" => "elicitation/create",
        "params" => obj! {
            "mode" => "form",
            "message" => message,
            "requestedSchema" => obj! {
                "type" => "object",
                "properties" => obj! {
                    "confirm" => obj! { "type" => "boolean", "title" => "Set this build live", "default" => false },
                    "appid_confirmation" => obj! { "type" => "integer", "title" => "App ID", "description" => format!("Type {appid} to confirm") },
                },
                "required" => crate::arr!["confirm", "appid_confirmation"],
            },
        },
    });
    ToolOutcome::InputRequired {
        requests,
        state: Some(obj! { "step" => "confirm" }),
    }
}

/// URL-mode elicitation fallback: the server never performs the write; it
/// sends the user to the Steamworks builds page to make the change there,
/// then reports the outcome when the client retries.
fn steamworks_handoff(env: &ToolEnv, appid: u64, buildid: u64, betakey: &str) -> ToolOutcome {
    let url = format!("https://partner.steamgames.com/apps/builds/{appid}");
    let handed_off = env
        .mrtr
        .state
        .as_ref()
        .is_some_and(|s| s["step"].as_str() == Some("url_handoff"));
    if let (true, Some(answer)) = (handed_off, env.mrtr.response("steamworks")) {
        return match answer["action"].as_str() {
            Some("accept") => {
                // The branch may have changed out of band: refresh partner build data.
                let prefix = "partner:/ISteamApps/GetApp";
                env.steam().cache.invalidate_where(|k| {
                    k.starts_with(prefix) && k.contains(&format!("appid={appid}"))
                });
                crate::log_info!(
                    "audit",
                    "build change handed off to Steamworks",
                    appid = appid,
                    buildid = buildid,
                    betakey = betakey,
                    principal = env.ctx.principal.id
                );
                ok_result(
                    format!(
                        "Opened Steamworks for app {appid}. Set build {buildid} live on '{betakey}' there; this server made no API change. Cached branch data was refreshed, so steam_partner_get_app_betas shows the result."
                    ),
                    obj! { "appid" => appid, "buildid" => buildid, "betakey" => betakey, "handoff" => true, "url" => url.as_str(), "apiChangeMade" => false },
                    &[],
                )
            }
            _ => tool_error("The user did not open Steamworks; no build was changed."),
        };
    }
    let mut requests = Map::new();
    requests.insert("steamworks", obj! {
        "method" => "elicitation/create",
        "params" => obj! {
            "mode" => "url",
            "message" => format!("This client cannot show a confirmation form. Open Steamworks to set build {buildid} live on branch '{betakey}' for app {appid} yourself."),
            "url" => url.as_str(),
        },
    });
    ToolOutcome::InputRequired {
        requests,
        state: Some(obj! { "step" => "url_handoff" }),
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

fn def(
    name: &'static str,
    title: &'static str,
    desc: &str,
    schema: Value,
    handler: Handler,
) -> ToolDef {
    ToolDef {
        name,
        title,
        description: desc.to_string(),
        input_schema: schema,
        output_schema: None,
        hints: READ,
        scope: scopes::PARTNER,
        key: KeyKind::Publisher,
        task: false,
        write: false,
        handler,
    }
}

fn fin(
    name: &'static str,
    title: &'static str,
    desc: &str,
    schema: Value,
    task: bool,
    handler: Handler,
) -> ToolDef {
    ToolDef {
        scope: scopes::FINANCIAL,
        key: KeyKind::Financial,
        task,
        ..def(name, title, desc, schema, handler)
    }
}

pub fn tools() -> Vec<ToolDef> {
    let types = [
        "game",
        "application",
        "tool",
        "demo",
        "dlc",
        "music",
        "config",
    ];
    vec![
        def(
            "steam_partner_list_apps",
            "Partner App List",
            "All apps the publisher key can access (games, DLC, demos, tools, soundtracks), with counts by type.",
            object_schema(
                vec![
                    (
                        "type_filter",
                        obj! { "type" => "array", "items" => obj! { "type" => "string", "enum" => types.to_vec() }, "description" => "Only these app types." },
                    ),
                    p_force(),
                ],
                &[],
            ),
            list_apps,
        ),
        def(
            "steam_partner_check_app_ownership",
            "Check App Ownership",
            "Whether a user owns one of your apps, including license source (family sharing), permanence and timestamp.",
            object_schema(
                vec![("steamid", p_steamid()), ("appid", p_appid()), p_force()],
                &["steamid", "appid"],
            ),
            check_ownership,
        ),
        def(
            "steam_partner_get_publisher_app_ownership",
            "Publisher App Ownership",
            "Ownership of all of the publisher's apps for one user.",
            object_schema(vec![("steamid", p_steamid()), p_force()], &["steamid"]),
            publisher_ownership,
        ),
        def(
            "steam_partner_get_ownership_changes",
            "Ownership Changes",
            "Incremental feed of ownership changes for your apps since previous package/CD-key row versions.",
            object_schema(
                vec![
                    (
                        "packagerowversion",
                        p_str("Package row version from the previous call (default 0)."),
                    ),
                    (
                        "cdkeyrowversion",
                        p_str("CD key row version from the previous call (default 0)."),
                    ),
                    p_force(),
                ],
                &[],
            ),
            ownership_changes,
        ),
        def(
            "steam_partner_get_app_price_info",
            "App Price Info",
            "Prices and discounts of your apps as seen by a specific user.",
            object_schema(
                vec![
                    ("steamid", p_steamid()),
                    (
                        "appids",
                        obj! { "type" => "array", "items" => obj! { "type" => "integer" }, "minItems" => 1, "maxItems" => 100, "description" => "App IDs." },
                    ),
                    p_force(),
                ],
                &["steamid", "appids"],
            ),
            price_info,
        ),
        def(
            "steam_partner_get_deleted_steamids",
            "Deleted SteamIDs",
            "Accounts deleted since a row version, for data-retention compliance.",
            object_schema(
                vec![
                    (
                        "rowversion",
                        p_str("Row version from the previous call (default 0)."),
                    ),
                    p_force(),
                ],
                &[],
            ),
            deleted_steamids,
        ),
        def(
            "steam_partner_get_app_betas",
            "App Branches",
            "Beta branches of an app with their live build IDs and password requirements.",
            object_schema(vec![("appid", p_appid()), p_force()], &["appid"]),
            app_betas,
        ),
        def(
            "steam_partner_get_app_builds",
            "App Builds",
            "Recent uploaded builds of an app with descriptions, creators and creation times.",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("count", p_int("Number of builds.", 1, 500, Some(10))),
                    p_force(),
                ],
                &["appid"],
            ),
            app_builds,
        ),
        def(
            "steam_partner_get_app_depot_versions",
            "Depot Versions",
            "Depot manifests and versions currently live for an app.",
            object_schema(vec![("appid", p_appid()), p_force()], &["appid"]),
            depot_versions,
        ),
        def(
            "steam_partner_get_players_banned",
            "Banned Players",
            "Players banned from an app via game bans.",
            object_schema(vec![("appid", p_appid()), p_force()], &["appid"]),
            players_banned,
        ),
        def(
            "steam_partner_get_cheating_reports",
            "Cheating Reports",
            "Cheating reports and bans for an app within a time window (default: last 7 days).",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    (
                        "timebegin",
                        obj! { "type" => ["integer", "string"], "description" => "Start (Unix time or RFC 3339)." },
                    ),
                    (
                        "timeend",
                        obj! { "type" => ["integer", "string"], "description" => "End (Unix time or RFC 3339)." },
                    ),
                    ("includereports", p_bool("Include reports.", true)),
                    ("includebans", p_bool("Include bans.", true)),
                    ("reportidmin", p_u64("Minimum report ID (for paging).")),
                    p_force(),
                ],
                &["appid"],
            ),
            cheating_reports,
        ),
        def(
            "steam_partner_get_server_list",
            "Partner Server List",
            "Game servers for your apps via the partner master-server query.",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    (
                        "filter",
                        p_str("Optional master server filter, e.g. \\dedicated\\1."),
                    ),
                    ("limit", p_int("Maximum servers.", 1, 20000, Some(100))),
                    p_force(),
                ],
                &["appid"],
            ),
            partner_server_list,
        ),
        def(
            "steam_partner_get_leaderboards",
            "Leaderboards",
            "All leaderboards defined for an app with IDs, sort methods and entry counts.",
            object_schema(vec![("appid", p_appid()), p_force()], &["appid"]),
            leaderboards,
        ),
        def(
            "steam_partner_get_leaderboard_entries",
            "Leaderboard Entries",
            "Entries from a leaderboard: a global rank range, entries around a user, or a user's friends.",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    (
                        "leaderboardid",
                        p_u64("Leaderboard ID from steam_partner_get_leaderboards."),
                    ),
                    ("rangestart", p_u64("First rank (default 0).")),
                    ("rangeend", p_u64("Last rank (default rangestart+99).")),
                    (
                        "datarequest",
                        p_enum(
                            "Which entries.",
                            &["global", "around_user", "friends"],
                            Some("global"),
                        ),
                    ),
                    ("steamid", p_steamid()),
                    p_force(),
                ],
                &["appid", "leaderboardid"],
            ),
            leaderboard_entries,
        ),
        def(
            "steam_partner_get_microtxn_report",
            "Microtransaction Report",
            "In-game purchase report (game sales, store sales, settlements or chargebacks) since a time.",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    (
                        "type",
                        p_enum(
                            "Report type.",
                            &["GAMESALES", "STEAMSTORESALES", "SETTLEMENT", "CHARGEBACK"],
                            Some("GAMESALES"),
                        ),
                    ),
                    (
                        "time",
                        obj! { "type" => "string", "description" => "Start time (RFC 3339 or YYYY-MM-DD; default 24h ago)." },
                    ),
                    ("maxresults", p_int("Maximum orders.", 1, 10000, Some(1000))),
                    ("sandbox", p_bool("Use the sandbox interface.", false)),
                    p_force(),
                ],
                &["appid"],
            ),
            microtxn_report,
        ),
        def(
            "steam_partner_query_microtxn",
            "Query Transaction",
            "Status of a single microtransaction by order ID or transaction ID.",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    (
                        "orderid",
                        obj! { "type" => ["string", "integer"], "description" => "Your order ID." },
                    ),
                    (
                        "transid",
                        obj! { "type" => ["string", "integer"], "description" => "Steam transaction ID." },
                    ),
                    ("sandbox", p_bool("Use the sandbox interface.", false)),
                    p_force(),
                ],
                &["appid"],
            ),
            query_txn,
        ),
        def(
            "steam_partner_get_item_definitions",
            "Inventory Item Definitions",
            "Steam Inventory Service item definitions for an app. The archive is cached by digest, so it is only re-downloaded when definitions change.",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    (
                        "filter",
                        p_str("Case-insensitive substring to match in definitions."),
                    ),
                    (
                        "max_items",
                        p_int("Maximum definitions returned.", 1, 50000, Some(200)),
                    ),
                    p_force(),
                ],
                &["appid"],
            ),
            item_definitions,
        ),
        def(
            "steam_partner_get_inventory",
            "User Inventory",
            "A user's Steam Inventory Service items for one of your apps.",
            object_schema(
                vec![("appid", p_appid()), ("steamid", p_steamid()), p_force()],
                &["appid", "steamid"],
            ),
            inventory,
        ),
        ToolDef {
            hints: Hints {
                read_only: true,
                destructive: false,
                idempotent: false,
                open_world: true,
            },
            ..def(
                "steam_partner_authenticate_ticket",
                "Authenticate User Ticket",
                "Validates a client session ticket and returns the authenticated SteamID, owner and ban flags. Never cached.",
                object_schema(
                    vec![
                        ("appid", p_appid()),
                        (
                            "ticket",
                            obj! { "type" => "string", "minLength" => 2, "description" => "Hex-encoded ticket." },
                        ),
                        (
                            "identity",
                            p_str("Identity string passed to GetAuthTicketForWebApi, if used."),
                        ),
                    ],
                    &["appid", "ticket"],
                ),
                authenticate_ticket,
            )
        },
        def(
            "steam_partner_get_asset_prices",
            "Asset Prices",
            "Item store asset prices for an app's in-game economy.",
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("currency", p_str("ISO 4217 currency code filter.")),
                    ("language", p_lang()),
                    p_force(),
                ],
                &["appid"],
            ),
            asset_prices,
        ),
        fin(
            "steam_partner_get_changed_sales_dates",
            "Changed Sales Dates",
            "Dates whose detailed sales data is new or changed since a high-watermark. Start with 0, then pass resultHighwatermark next time.",
            object_schema(
                vec![
                    (
                        "highwatermark",
                        p_str("Watermark from the previous call (default 0)."),
                    ),
                    p_force(),
                ],
                &[],
            ),
            false,
            changed_sales_dates,
        ),
        fin(
            "steam_partner_get_detailed_sales",
            "Detailed Sales",
            "Line-item sales for one date (units, gross/net USD, country, platform, package), with lookup tables. Can follow pagination for the full day; long fetches run as a task.",
            object_schema(
                vec![
                    (
                        "date",
                        obj! { "type" => "string", "minLength" => 8, "description" => "Date as returned by steam_partner_get_changed_sales_dates." },
                    ),
                    (
                        "highwatermark_id",
                        p_str("Start after this row ID (default 0)."),
                    ),
                    (
                        "all_pages",
                        p_bool("Follow pagination for the whole date.", false),
                    ),
                    (
                        "max_pages",
                        p_int("Page limit when all_pages is set.", 1, 500, Some(20)),
                    ),
                    (
                        "max_rows",
                        p_int("Maximum rows returned.", 1, 1000000, Some(1000)),
                    ),
                    p_force(),
                ],
                &["date"],
            ),
            true,
            detailed_sales,
        ),
        fin(
            "steam_partner_sales_summary",
            "Sales Summary",
            "Aggregates a full day of detailed sales into totals and top groups (default by primary app; any field such as country_code or platform can be used).",
            object_schema(
                vec![
                    (
                        "date",
                        obj! { "type" => "string", "minLength" => 8, "description" => "Date as returned by steam_partner_get_changed_sales_dates." },
                    ),
                    (
                        "group_by",
                        obj! { "type" => "array", "items" => obj! { "type" => "string" }, "description" => "Row fields to group by, e.g. [\"primary_appid\",\"country_code\"]." },
                    ),
                    (
                        "top",
                        p_int("Number of groups returned.", 1, 1000, Some(25)),
                    ),
                    ("max_pages", p_int("Page limit.", 1, 1000, Some(100))),
                    p_force(),
                ],
                &["date"],
            ),
            true,
            sales_summary,
        ),
        ToolDef {
            hints: Hints {
                read_only: false,
                destructive: true,
                idempotent: true,
                open_world: true,
            },
            scope: scopes::WRITE,
            write: true,
            ..def(
                "steam_partner_set_app_build_live",
                "Set Build Live",
                "Sets a build live on a branch (SetAppBuildLive). Requires STEAM_MCP_ENABLE_WRITES and asks the user to confirm via elicitation before making the change.",
                object_schema(
                    vec![
                        ("appid", p_appid()),
                        (
                            "buildid",
                            p_u64("Build ID from steam_partner_get_app_builds."),
                        ),
                        (
                            "betakey",
                            obj! { "type" => "string", "minLength" => 1, "description" => "Branch name ('public' for the default branch)." },
                        ),
                        (
                            "description",
                            p_str("Optional build description to record."),
                        ),
                    ],
                    &["appid", "buildid", "betakey"],
                ),
                set_build_live,
            )
        },
    ]
}
