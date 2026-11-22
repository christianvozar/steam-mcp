//! Extended read-only coverage of the Steam Web API and Steamworks partner
//! API: profile items, workshop details, store browsing, game-server
//! accounts, economy and market, inventory history, notification sessions,
//! lobbies, microtransaction user data and VAC status.
//!
//! Most tools here are thin, declarative wrappers ([`Simple`]) around a single
//! method: arguments map to Steam parameters, the payload is extracted from a
//! known path, and caching follows the TTL class. Required parameter names
//! were discovered live from Steam's own "Required parameter" errors where
//! the method allowed it (see `ref/steam-webapi.md`).

use super::public::steamid_arg;
use super::store::{app_names, shape_store_item, store_context, tag_names};
use super::*;
use crate::mcp::context::scopes;
use crate::steam::{Call, Host, KeyKind, parse_steamid};
use crate::try_steam;

macro_rules! tryo {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(o) => return o,
        }
    };
}

// ---------------------------------------------------------------------------
// Declarative single-method tools
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum PKind {
    /// One value.
    Scalar,
    /// A list sent as `name[0]`, `name[1]`, …
    Indexed,
    /// A list sent comma-separated.
    Comma,
    /// A 64-bit SteamID, validated.
    SteamId,
}

struct P {
    arg: &'static str,
    steam: &'static str,
    kind: PKind,
    /// Value sent when the argument is absent.
    default: Option<&'static str>,
}

const fn p(arg: &'static str, steam: &'static str, kind: PKind) -> P {
    P {
        arg,
        steam,
        kind,
        default: None,
    }
}

const fn pd(arg: &'static str, steam: &'static str, kind: PKind, default: &'static str) -> P {
    P {
        arg,
        steam,
        kind,
        default: Some(default),
    }
}

struct Simple {
    host: Host,
    path: &'static str,
    key: KeyKind,
    ttl: &'static str,
    post: bool,
    /// Never cache (one-shot or credential-like inputs).
    uncached: bool,
    params: &'static [P],
    /// Path to the payload in the response.
    result: &'static [&'static str],
    /// Noun for the summary line.
    label: &'static str,
}

fn run_simple(spec: &Simple, env: &ToolEnv, args: &Map) -> ToolOutcome {
    let mut call = if spec.post {
        Call::post(spec.host, spec.path, spec.key, spec.ttl)
    } else {
        Call::get(spec.host, spec.path, spec.key, spec.ttl)
    };
    for p in spec.params {
        match p.kind {
            PKind::SteamId => {
                let present = args.get(p.arg).is_some_and(|v| !v.is_null());
                if !present {
                    if let Some(d) = p.default {
                        call = call.param(p.steam, d);
                    }
                    continue;
                }
                let raw = args
                    .get(p.arg)
                    .map(|v| v.to_param_string())
                    .unwrap_or_default();
                match parse_steamid(&raw) {
                    Some(id) => call = call.param(p.steam, id),
                    None => {
                        return tool_error(format!(
                            "'{}' must be a 64-bit SteamID, got '{raw}'.",
                            p.arg
                        ));
                    }
                }
            }
            PKind::Scalar => match args.get(p.arg).filter(|v| !v.is_null()) {
                Some(v) => call = call.param(p.steam, v.to_param_string()),
                None => {
                    if let Some(d) = p.default {
                        call = call.param(p.steam, d);
                    }
                }
            },
            PKind::Indexed => {
                for (i, v) in arg_list(args, p.arg).iter().enumerate() {
                    call = call.param(&format!("{}[{i}]", p.steam), v);
                }
            }
            PKind::Comma => {
                let list = arg_list(args, p.arg);
                if !list.is_empty() {
                    call = call.param(p.steam, list.join(","));
                }
            }
        }
    }
    let value = if spec.uncached {
        try_steam!(env.steam().call_uncached(&call))
    } else {
        let l = try_steam!(env.steam().call(&call, force(args)));
        if l.negative {
            return not_found(&format!("Data from {}", spec.path.trim_matches('/')));
        }
        return finish_simple(spec, (*l.value).clone(), &[&l]);
    };
    finish_simple(spec, value, &[])
}

fn finish_simple(spec: &Simple, value: Value, lookups: &[&Lookup]) -> ToolOutcome {
    let payload = value.at(spec.result).clone();
    let payload = if payload.is_null() && spec.result.len() > 1 {
        value.at(&spec.result[..1]).clone()
    } else {
        payload
    };
    if let Some(err) = payload.get("error").or_else(|| {
        value
            .at(&["response", "error"])
            .as_object()
            .map(|_| &value["response"]["error"])
    }) {
        if !err.is_null() && payload.as_object().is_some_and(|m| m.len() <= 2) {
            return tool_error(format!(
                "Steam reported an error from {}: {err}",
                spec.path.trim_matches('/')
            ));
        }
    }
    let count = match &payload {
        Value::Array(a) => Some(a.len()),
        Value::Object(m) => m.values().find_map(|v| v.as_array().map(|a| a.len())),
        _ => None,
    };
    let empty = match &payload {
        Value::Null => true,
        Value::Object(m) => {
            m.is_empty()
                || m.values()
                    .all(|v| v.is_null() || v.as_object().is_some_and(|o| o.is_empty()))
        }
        Value::Array(a) => a.is_empty(),
        _ => false,
    };
    let text = match (count, empty) {
        (_, true) => format!(
            "No {} returned (none exist, or they are private or not visible to this key).",
            spec.label
        ),
        (Some(n), _) => format!("{n} {} returned.", spec.label),
        (None, _) => format!("{} returned.", crate::util::truncate(spec.label, 80)),
    };
    // structuredContent must be an object: lists are wrapped under their
    // Steam field name, scalars under "value".
    let data = match payload {
        Value::Object(_) => payload,
        Value::Array(_) => {
            let field = spec
                .result
                .last()
                .copied()
                .filter(|f| *f != "response")
                .unwrap_or("items");
            crate::obj! { field => payload }
        }
        other => crate::obj! { "value" => other },
    };
    ok_result(text, data, lookups)
}

macro_rules! simple_tool {
    ($fname:ident, $spec:expr) => {
        fn $fname(env: &ToolEnv, args: &Map) -> ToolOutcome {
            const SPEC: Simple = $spec;
            run_simple(&SPEC, env, args)
        }
    };
}

use PKind::{Comma, Indexed, Scalar, SteamId};

simple_tool!(
    top_achievements,
    Simple {
        host: Host::Public,
        path: "/IPlayerService/GetTopAchievementsForGames/v1/",
        key: KeyKind::Public,
        ttl: "short",
        post: false,
        uncached: false,
        params: &[
            p("steamid", "steamid", SteamId),
            p("appids", "appids", Indexed),
            pd("max_achievements", "max_achievements", Scalar, "5"),
            pd("language", "language", Scalar, "english")
        ],
        result: &["response", "games"],
        label: "game(s) with top achievements"
    }
);
simple_tool!(
    workshop_details,
    Simple {
        host: Host::Public,
        path: "/IPublishedFileService/GetDetails/v1/",
        key: KeyKind::Public,
        ttl: "medium",
        post: false,
        uncached: false,
        params: &[
            p("publishedfileids", "publishedfileids", Indexed),
            pd("include_tags", "includetags", Scalar, "true"),
            pd("include_votes", "includevotes", Scalar, "true"),
            pd("include_children", "includechildren", Scalar, "false"),
            pd("include_metadata", "includemetadata", Scalar, "false"),
            pd(
                "include_for_sale_data",
                "includeforsaledata",
                Scalar,
                "false"
            ),
            pd("short_description", "short_description", Scalar, "true"),
            pd("strip_bbcode", "strip_description_bbcode", Scalar, "true"),
            pd(
                "return_playtime_stats",
                "return_playtime_stats",
                Scalar,
                "0"
            )
        ],
        result: &["response", "publishedfiledetails"],
        label: "workshop item(s)"
    }
);
simple_tool!(
    user_workshop_files,
    Simple {
        host: Host::Public,
        path: "/IPublishedFileService/GetUserFiles/v1/",
        key: KeyKind::Public,
        ttl: "short",
        post: false,
        uncached: false,
        params: &[
            p("steamid", "steamid", SteamId),
            p("appid", "appid", Scalar),
            pd("type", "type", Scalar, "myfiles"),
            pd("page", "page", Scalar, "1"),
            pd("numperpage", "numperpage", Scalar, "20"),
            pd("return_vote_data", "return_vote_data", Scalar, "true"),
            pd("return_tags", "return_tags", Scalar, "true"),
            pd(
                "return_short_description",
                "return_short_description",
                Scalar,
                "true"
            )
        ],
        result: &["response", "publishedfiledetails"],
        label: "workshop file(s)"
    }
);
simple_tool!(
    workshop_subsection,
    Simple {
        host: Host::Public,
        path: "/IPublishedFileService/GetSubSectionData/v1/",
        key: KeyKind::Public,
        ttl: "medium",
        post: false,
        uncached: false,
        params: &[
            p("publishedfileid", "publishedfileid", Scalar),
            p("sectionid", "specific_sectionid", Scalar),
            pd(
                "table_of_contents",
                "for_table_of_contents",
                Scalar,
                "false"
            )
        ],
        result: &["response", "sub_sections"],
        label: "workshop sub-section(s)"
    }
);
simple_tool!(
    ugc_file_details,
    Simple {
        host: Host::Public,
        path: "/ISteamRemoteStorage/GetUGCFileDetails/v1/",
        key: KeyKind::Public,
        ttl: "long",
        post: false,
        uncached: false,
        params: &[
            p("ugcid", "ugcid", Scalar),
            p("appid", "appid", Scalar),
            p("steamid", "steamid", SteamId)
        ],
        result: &["data"],
        label: "UGC file details"
    }
);
simple_tool!(
    server_steamids_by_ip,
    Simple {
        host: Host::Public,
        path: "/IGameServersService/GetServerSteamIDsByIP/v1/",
        key: KeyKind::Public,
        ttl: "short",
        post: false,
        uncached: false,
        params: &[p("ips", "server_ips", Comma)],
        result: &["response", "servers"],
        label: "server(s)"
    }
);
simple_tool!(
    server_ips_by_steamid,
    Simple {
        host: Host::Public,
        path: "/IGameServersService/GetServerIPsBySteamID/v1/",
        key: KeyKind::Public,
        ttl: "short",
        post: false,
        uncached: false,
        params: &[p("steamids", "server_steamids", Comma)],
        result: &["response", "servers"],
        label: "server(s)"
    }
);
simple_tool!(
    server_accounts,
    Simple {
        host: Host::Public,
        path: "/IGameServersService/GetAccountList/v1/",
        key: KeyKind::Public,
        ttl: "volatile",
        post: false,
        uncached: false,
        params: &[],
        result: &["response", "servers"],
        label: "game server account(s)"
    }
);
simple_tool!(
    server_account_info,
    Simple {
        host: Host::Public,
        path: "/IGameServersService/GetAccountPublicInfo/v1/",
        key: KeyKind::Public,
        ttl: "medium",
        post: false,
        uncached: false,
        params: &[p("steamid", "steamid", SteamId)],
        result: &["response"],
        label: "game server account information"
    }
);
simple_tool!(
    query_login_token,
    Simple {
        host: Host::Public,
        path: "/IGameServersService/QueryLoginToken/v1/",
        key: KeyKind::Public,
        ttl: "volatile",
        post: false,
        uncached: true,
        params: &[p("login_token", "login_token", Scalar)],
        result: &["response"],
        label: "login token status"
    }
);
simple_tool!(
    server_player_stats,
    Simple {
        host: Host::Public,
        path: "/ISteamGameServerStats/GetGameServerPlayerStatsForGame/v1/",
        key: KeyKind::Public,
        ttl: "short",
        post: false,
        uncached: false,
        params: &[
            p("gameid", "gameid", Scalar),
            p("appid", "appid", Scalar),
            p("rangestart", "rangestart", Scalar),
            p("rangeend", "rangeend", Scalar),
            pd("maxresults", "maxresults", Scalar, "100")
        ],
        result: &["response"],
        label: "game server player stats"
    }
);
simple_tool!(
    news_authed,
    Simple {
        host: Host::Partner,
        path: "/ISteamNews/GetNewsForAppAuthed/v2/",
        key: KeyKind::Publisher,
        ttl: "short",
        post: false,
        uncached: false,
        params: &[
            p("appid", "appid", Scalar),
            pd("count", "count", Scalar, "10"),
            pd("maxlength", "maxlength", Scalar, "600"),
            p("enddate", "enddate", Scalar),
            p("feeds", "feeds", Comma),
            p("tags", "tags", Comma)
        ],
        result: &["appnews", "newsitems"],
        label: "news item(s), including unpublished"
    }
);
simple_tool!(
    item_daily_revenue,
    Simple {
        host: Host::Partner,
        path: "/IWorkshopService/GetItemDailyRevenue/v1/",
        key: KeyKind::Publisher,
        ttl: "financial",
        post: false,
        uncached: false,
        params: &[
            p("appid", "appid", Scalar),
            p("item_id", "item_id", Scalar),
            p("date_start", "date_start", Scalar),
            p("date_end", "date_end", Scalar)
        ],
        result: &["response", "country_revenue"],
        label: "revenue row(s)"
    }
);
simple_tool!(
    market_prices,
    Simple {
        host: Host::Partner,
        path: "/ISteamEconomy/GetMarketPrices/v1/",
        key: KeyKind::Publisher,
        ttl: "short",
        post: false,
        uncached: false,
        params: &[p("appid", "appid", Scalar)],
        result: &["result"],
        label: "market price data"
    }
);
simple_tool!(
    can_trade,
    Simple {
        host: Host::Partner,
        path: "/ISteamEconomy/CanTrade/v1/",
        key: KeyKind::Publisher,
        ttl: "volatile",
        post: false,
        uncached: false,
        params: &[
            p("appid", "appid", Scalar),
            p("steamid", "steamid", SteamId),
            p("targetid", "targetid", SteamId)
        ],
        result: &["result"],
        label: "trade eligibility"
    }
);
simple_tool!(
    market_popular,
    Simple {
        host: Host::Partner,
        path: "/IEconMarketService/GetPopular/v1/",
        key: KeyKind::Publisher,
        ttl: "short",
        post: false,
        uncached: false,
        params: &[
            pd("language", "language", Scalar, "english"),
            pd("rows", "rows", Scalar, "20"),
            pd("start", "start", Scalar, "0"),
            p("appid", "filter_appid", Scalar),
            p("currency", "ecurrency", Scalar)
        ],
        result: &["response", "data"],
        label: "popular listing(s)"
    }
);
simple_tool!(
    market_asset_id,
    Simple {
        host: Host::Partner,
        path: "/IEconMarketService/GetAssetID/v1/",
        key: KeyKind::Publisher,
        ttl: "long",
        post: false,
        uncached: false,
        params: &[
            p("appid", "appid", Scalar),
            p("listingid", "listingid", Scalar)
        ],
        result: &["response"],
        label: "listing asset"
    }
);
simple_tool!(
    market_eligibility,
    Simple {
        host: Host::Partner,
        path: "/IEconMarketService/GetMarketEligibility/v1/",
        key: KeyKind::Publisher,
        ttl: "volatile",
        post: false,
        uncached: false,
        params: &[p("steamid", "steamid", SteamId)],
        result: &["response"],
        label: "market eligibility"
    }
);
simple_tool!(
    inventory_item_defs,
    Simple {
        host: Host::Partner,
        path: "/IInventoryService/GetItemDefs/v1/",
        key: KeyKind::Publisher,
        ttl: "medium",
        post: false,
        uncached: false,
        params: &[
            p("appid", "appid", Scalar),
            p("itemdefids", "itemdefids", Indexed),
            p("workshopids", "workshopids", Indexed),
            p("modifiedsince", "modifiedsince", Scalar)
        ],
        result: &["response"],
        label: "item definitions"
    }
);
simple_tool!(
    inventory_price_sheet,
    Simple {
        host: Host::Partner,
        path: "/IInventoryService/GetPriceSheet/v1/",
        key: KeyKind::Publisher,
        ttl: "medium",
        post: false,
        uncached: false,
        params: &[
            pd("currency", "ecurrency", Scalar, "1"),
            p("currency_code", "currency_code", Scalar)
        ],
        result: &["response"],
        label: "price sheet"
    }
);
simple_tool!(
    inventory_quantity,
    Simple {
        host: Host::Partner,
        path: "/IInventoryService/GetQuantity/v1/",
        key: KeyKind::Publisher,
        ttl: "volatile",
        post: false,
        uncached: false,
        params: &[
            p("appid", "appid", Scalar),
            p("steamid", "steamid", SteamId),
            p("itemdefids", "itemdefid", Indexed)
        ],
        result: &["response"],
        label: "item quantities"
    }
);
simple_tool!(
    inventory_history,
    Simple {
        host: Host::Partner,
        path: "/IGameInventory/GetUserHistory/v1/",
        key: KeyKind::Publisher,
        ttl: "volatile",
        post: false,
        uncached: false,
        params: &[
            p("appid", "appid", Scalar),
            p("steamid", "steamid", SteamId),
            pd("contextid", "contextid", Scalar, "2"),
            p("starttime", "starttime", Scalar),
            p("endtime", "endtime", Scalar)
        ],
        result: &["response"],
        label: "inventory history"
    }
);
simple_tool!(
    inventory_history_command,
    Simple {
        host: Host::Partner,
        path: "/IGameInventory/GetHistoryCommandDetails/v1/",
        key: KeyKind::Publisher,
        ttl: "short",
        post: false,
        uncached: false,
        params: &[
            p("appid", "appid", Scalar),
            p("steamid", "steamid", SteamId),
            p("command", "command", Scalar),
            pd("contextid", "contextid", Scalar, "2"),
            p("arguments", "arguments", Scalar)
        ],
        result: &["response"],
        label: "history command details"
    }
);
simple_tool!(
    asset_history,
    Simple {
        host: Host::Partner,
        path: "/IGameInventory/SupportGetAssetHistory/v1/",
        key: KeyKind::Publisher,
        ttl: "volatile",
        post: false,
        uncached: false,
        params: &[
            p("appid", "appid", Scalar),
            p("assetid", "assetid", Scalar),
            pd("contextid", "contextid", Scalar, "2")
        ],
        result: &["response"],
        label: "asset history"
    }
);
simple_tool!(
    notification_sessions,
    Simple {
        host: Host::Partner,
        path: "/IGameNotificationsService/EnumerateSessionsForApp/v1/",
        key: KeyKind::Publisher,
        ttl: "volatile",
        post: false,
        uncached: false,
        params: &[
            p("appid", "appid", Scalar),
            p("steamid", "steamid", SteamId),
            pd(
                "include_all_user_messages",
                "include_all_user_messages",
                Scalar,
                "false"
            ),
            pd(
                "include_auth_user_message",
                "include_auth_user_message",
                Scalar,
                "false"
            ),
            pd("language", "language", Scalar, "english")
        ],
        result: &["response", "sessions"],
        label: "notification session(s)"
    }
);
simple_tool!(
    lobby_data,
    Simple {
        host: Host::Partner,
        path: "/ILobbyMatchmakingService/GetLobbyData/v1/",
        key: KeyKind::Publisher,
        ttl: "volatile",
        post: false,
        uncached: false,
        params: &[
            p("appid", "appid", Scalar),
            p("lobby_steamid", "steamid_lobby", SteamId)
        ],
        result: &["response"],
        label: "lobby data"
    }
);
simple_tool!(
    microtxn_user_info,
    Simple {
        host: Host::Partner,
        path: "/ISteamMicroTxn/GetUserInfo/v2/",
        key: KeyKind::Publisher,
        ttl: "volatile",
        post: false,
        uncached: false,
        params: &[
            p("steamid", "steamid", SteamId),
            p("ipaddress", "ipaddress", Scalar)
        ],
        result: &["response", "params"],
        label: "microtransaction user info"
    }
);
simple_tool!(
    microtxn_user_info_sandbox,
    Simple {
        host: Host::Partner,
        path: "/ISteamMicroTxnSandbox/GetUserInfo/v2/",
        key: KeyKind::Publisher,
        ttl: "volatile",
        post: false,
        uncached: false,
        params: &[
            p("steamid", "steamid", SteamId),
            p("ipaddress", "ipaddress", Scalar)
        ],
        result: &["response", "params"],
        label: "sandbox microtransaction user info"
    }
);
simple_tool!(
    microtxn_agreements,
    Simple {
        host: Host::Partner,
        path: "/ISteamMicroTxn/GetUserAgreementInfo/v2/",
        key: KeyKind::Publisher,
        ttl: "volatile",
        post: false,
        uncached: false,
        params: &[
            p("steamid", "steamid", SteamId),
            p("appid", "appid", Scalar)
        ],
        result: &["response", "params", "agreements"],
        label: "agreement(s)"
    }
);
simple_tool!(
    vac_status,
    Simple {
        host: Host::Partner,
        path: "/ICheatReportingService/RequestVacStatusForUser/v1/",
        key: KeyKind::Publisher,
        ttl: "volatile",
        post: true,
        uncached: true,
        params: &[
            p("steamid", "steamid", SteamId),
            p("appid", "appid", Scalar),
            p("session_id", "session_id", Scalar)
        ],
        result: &["response"],
        label: "VAC status"
    }
);

// ---------------------------------------------------------------------------
// Hand-shaped tools
// ---------------------------------------------------------------------------

const PROFILE_ITEMS: &[(&str, &str, &str)] = &[
    (
        "animated_avatar",
        "/IPlayerService/GetAnimatedAvatar/v1/",
        "avatar",
    ),
    (
        "avatar_frame",
        "/IPlayerService/GetAvatarFrame/v1/",
        "avatar_frame",
    ),
    (
        "mini_profile_background",
        "/IPlayerService/GetMiniProfileBackground/v1/",
        "profile_background",
    ),
    (
        "profile_background",
        "/IPlayerService/GetProfileBackground/v1/",
        "profile_background",
    ),
];

fn profile_items(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let language = lang(env, args);
    let (path, field) = match arg_str(args, "only") {
        None | Some("all") => ("/IPlayerService/GetProfileItemsEquipped/v1/", None),
        Some(o) => match PROFILE_ITEMS.iter().find(|(n, _, _)| *n == o) {
            Some((_, path, field)) => (*path, Some(*field)),
            None => return tool_error(format!("Unknown item '{o}'.")),
        },
    };
    let l = try_steam!(
        env.steam().call(
            &Call::get(Host::Public, path, KeyKind::None, "medium")
                .param("steamid", steamid)
                .param("language", &language),
            force(args)
        )
    );
    let resp = l.value["response"].clone();
    let items = match field {
        Some(f) => obj! { f => resp[f].clone() },
        None => resp,
    };
    let equipped: Vec<String> = items
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(_, v)| v.as_object().is_some_and(|o| !o.is_empty()))
                .map(|(k, v)| {
                    format!(
                        "{k}: {}",
                        v["item_title"]
                            .as_str()
                            .or(v["name"].as_str())
                            .unwrap_or("equipped")
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    ok_result(
        if equipped.is_empty() {
            format!("{steamid} has no visible equipped profile items.")
        } else {
            format!("{steamid} has equipped {}.", equipped.join("; "))
        },
        obj! { "steamid" => steamid.to_string(), "items" => items },
        &[&l],
    )
}

fn store_categories(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let language = lang(env, args);
    let call = Call::get(
        Host::Public,
        "/IStoreBrowseService/GetStoreCategories/v1/",
        KeyKind::None,
        "static",
    )
    .param(
        "input_json",
        obj! { "language" => language.as_str() }.to_string(),
    );
    let l = try_steam!(env.steam().call(&call, force(args)));
    let cats: Vec<Value> = l.value.at(&["response", "categories"]).as_array().map(|a| a.iter().map(|c| obj! { "categoryid" => c["categoryid"].clone(), "type" => c["type"].clone(), "name" => c["display_name"].clone(), "internalName" => c["internal_name"].clone() }).collect()).unwrap_or_default();
    ok_result(
        format!("{} store categories.", cats.len()),
        obj! { "categories" => cats },
        &[&l],
    )
}

fn query_store(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let language = lang(env, args);
    let cc_v = cc(env, args);
    // Tags may be IDs or names.
    let wanted = arg_list(args, "tags");
    let mut tagids: Vec<u64> = Vec::new();
    if !wanted.is_empty() {
        let names = tag_names(env, &language);
        for t in &wanted {
            match t.parse::<u64>() {
                Ok(id) => tagids.push(id),
                Err(_) => match names.iter().find(|(_, n)| n.eq_ignore_ascii_case(t)) {
                    Some((id, _)) => tagids.push(*id),
                    None => {
                        return tool_error(format!(
                            "Unknown store tag '{t}'. Use steam_get_store_tags to list tags."
                        ));
                    }
                },
            }
        }
    }
    let types = arg_list(args, "types");
    let types: Vec<String> = if types.is_empty() {
        vec!["games".into()]
    } else {
        types
    };
    let mut type_filters = Map::new();
    for t in &types {
        match t.as_str() {
            "games" | "dlc" | "software" | "videos" | "hardware" | "music" | "demos" => {
                type_filters.insert(format!("include_{t}"), true);
            }
            other => return tool_error(format!("Unknown type '{other}'.")),
        }
    }
    let mut filters = obj! { "type_filters" => Value::Object(type_filters) };
    if !tagids.is_empty() {
        filters.set(
            "tagids_must_match",
            tagids
                .iter()
                .map(|t| obj! { "tagids" => crate::arr![*t] })
                .collect::<Vec<_>>(),
        );
    }
    if arg_bool(args, "free_only", false) {
        filters.set("price_filters", obj! { "only_free_items" => true });
    }
    let count = arg_u64(args, "count").unwrap_or(20).clamp(1, 100);
    let input = obj! {
        "query" => obj! { "start" => arg_u64(args, "start").unwrap_or(0), "count" => count, "filters" => filters },
        "context" => store_context(&cc_v, &language),
        "data_request" => obj! { "include_basic_info" => true, "include_reviews" => true, "include_tag_count" => 10, "include_release" => true, "include_platforms" => true },
    };
    let l = try_steam!(
        env.steam().call(
            &Call::get(
                Host::Public,
                "/IStoreQueryService/Query/v1/",
                KeyKind::None,
                "short"
            )
            .param("input_json", input.canonical().to_string()),
            force(args)
        )
    );
    let tags = tag_names(env, &language);
    let total = l
        .value
        .at(&["response", "metadata", "total_matching_records"])
        .as_u64()
        .unwrap_or(0);
    let items: Vec<Value> = l
        .value
        .at(&["response", "store_items"])
        .as_array()
        .map(|a| a.iter().map(|it| shape_store_item(it, &tags)).collect())
        .unwrap_or_default();
    let top: Vec<String> = items
        .iter()
        .take(5)
        .filter_map(|i| i["name"].as_str().map(String::from))
        .collect();
    ok_result(
        format!(
            "{} matching store item(s); showing {}: {}",
            fmt_num(total),
            items.len(),
            top.join(", ")
        ),
        obj! { "totalMatches" => total, "filters" => obj! { "tagids" => tagids.into_iter().map(Value::from).collect::<Vec<_>>(), "types" => types, "freeOnly" => arg_bool(args, "free_only", false) }, "items" => items },
        &[&l],
    )
}

fn wishlist_sorted(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let steamid = tryo!(steamid_arg(args, "steamid"));
    let language = lang(env, args);
    let input = obj! {
        "steamid" => steamid.to_string(),
        "context" => store_context(&cc(env, args), &language),
        "data_request" => obj! { "include_basic_info" => true, "include_reviews" => true, "include_release" => true },
        "start_index" => arg_u64(args, "start").unwrap_or(0),
        "page_size" => arg_u64(args, "count").unwrap_or(50).clamp(1, 500),
    };
    let l = try_steam!(
        env.steam().call(
            &Call::get(
                Host::Public,
                "/IWishlistService/GetWishlistSortedFiltered/v1/",
                KeyKind::None,
                "medium"
            )
            .param("input_json", input.canonical().to_string()),
            force(args)
        )
    );
    let tags = tag_names(env, &language);
    let items: Vec<Value> = l
        .value
        .at(&["response", "items"])
        .as_array()
        .map(|a| {
            a.iter()
                .map(|w| {
                    let mut o = if w["store_item"].is_object() {
                        shape_store_item(&w["store_item"], &tags)
                    } else {
                        obj! { "appid" => w["appid"].clone() }
                    };
                    o.set("priority", w["priority"].clone());
                    o.set(
                        "dateAdded",
                        w["date_added"]
                            .as_u64()
                            .map(crate::util::time::iso8601_secs),
                    );
                    o
                })
                .collect()
        })
        .unwrap_or_default();
    // Fill in names for entries without store data.
    let ids: Vec<u64> = items
        .iter()
        .filter(|i| i["name"].is_null())
        .filter_map(|i| i["appid"].as_u64())
        .collect();
    let names = if ids.is_empty() {
        Default::default()
    } else {
        app_names(env, &ids)
    };
    let items: Vec<Value> = items
        .into_iter()
        .map(|mut i| {
            if i["name"].is_null() {
                if let Some(n) = i["appid"].as_u64().and_then(|a| names.get(&a)) {
                    i.set("name", n.as_str());
                }
            }
            i
        })
        .collect();
    ok_result(
        if items.is_empty() {
            format!("{steamid} has no visible wishlist items (empty or private).")
        } else {
            format!(
                "{} wishlist item(s) for {steamid} with store data.",
                items.len()
            )
        },
        obj! { "steamid" => steamid.to_string(), "items" => items },
        &[&l],
    )
}

fn dlc_for_apps(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let ids: Vec<u64> = arg_list(args, "appids")
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();
    if ids.is_empty() || ids.len() > 50 {
        return tool_error("'appids' must contain 1–50 app IDs.");
    }
    let input = obj! { "appids" => ids.iter().map(|a| Value::from(*a)).collect::<Vec<_>>(), "context" => store_context(&cc(env, args), &lang(env, args)) };
    let l = try_steam!(
        env.steam().call(
            &Call::get(
                Host::Public,
                "/IStoreBrowseService/GetDLCForApps/v1/",
                KeyKind::Public,
                "medium"
            )
            .param("input_json", input.canonical().to_string()),
            force(args)
        )
    );
    let dlc = l
        .value
        .at(&["response", "dlc_data"])
        .as_array()
        .cloned()
        .unwrap_or_default();
    let dlc_ids: Vec<u64> = dlc
        .iter()
        .filter_map(|d| d["appid"].as_u64())
        .take(500)
        .collect();
    let names = app_names(env, &dlc_ids);
    let shaped: Vec<Value> = dlc
        .iter()
        .map(|d| {
            let mut o = d.clone();
            if let Some(n) = d["appid"].as_u64().and_then(|a| names.get(&a)) {
                o.set("name", n.as_str());
            }
            o
        })
        .collect();
    ok_result(
        format!("{} DLC item(s) for {} app(s).", shaped.len(), ids.len()),
        obj! { "dlc" => shaped },
        &[&l],
    )
}

fn session_details(env: &ToolEnv, args: &Map) -> ToolOutcome {
    let appid = arg_u64(args, "appid").unwrap_or(0);
    let sessions: Vec<Value> = arg_list(args, "sessionids")
        .iter()
        .map(|s| obj! { "sessionid" => s.as_str(), "appid" => appid })
        .collect();
    if sessions.is_empty() {
        return tool_error("'sessionids' must contain at least one session ID.");
    }
    let input = obj! { "sessions" => sessions, "appid" => appid, "language" => lang(env, args) };
    let call = Call::get(
        Host::Partner,
        "/IGameNotificationsService/GetSessionDetailsForApp/v1/",
        KeyKind::Publisher,
        "volatile",
    )
    .param("input_json", input.canonical().to_string());
    let l = try_steam!(env.steam().call(&call, force(args)));
    let s = l.value.at(&["response", "sessions"]).clone();
    ok_result(
        format!(
            "Details for {} notification session(s).",
            s.as_array().map(|a| a.len()).unwrap_or(0)
        ),
        obj! { "appid" => appid, "sessions" => s },
        &[&l],
    )
}

fn microtxn_user(env: &ToolEnv, args: &Map) -> ToolOutcome {
    if arg_bool(args, "sandbox", false) {
        microtxn_user_info_sandbox(env, args)
    } else {
        microtxn_user_info(env, args)
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

fn def(
    name: &'static str,
    title: &'static str,
    desc: &str,
    scope: &'static str,
    key: KeyKind,
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
        scope,
        key,
        task: false,
        write: false,
        handler,
    }
}

fn ids(desc: &str, max: u64) -> Value {
    obj! { "type" => "array", "items" => obj! { "type" => "integer", "minimum" => 1 }, "minItems" => 1, "maxItems" => max, "description" => desc }
}

fn strings(desc: &str, max: u64) -> Value {
    obj! { "type" => "array", "items" => obj! { "type" => "string", "minLength" => 1 }, "minItems" => 1, "maxItems" => max, "description" => desc }
}

fn unix_or_date(desc: &str) -> Value {
    obj! { "type" => ["integer", "string"], "description" => format!("{desc} (Unix time).") }
}

pub fn tools() -> Vec<ToolDef> {
    use scopes::{PARTNER, PUBLIC};
    let n = KeyKind::None;
    let k = KeyKind::Public;
    let pk = KeyKind::Publisher;
    vec![
        def(
            "steam_get_profile_items",
            "Equipped Profile Items",
            "A player's equipped profile cosmetics: background, mini-profile background, avatar frame, animated avatar, profile modifier and Steam Deck keyboard skin. Works without a key.",
            PUBLIC,
            n,
            object_schema(
                vec![
                    ("steamid", p_steamid()),
                    (
                        "only",
                        p_enum(
                            "Return just one item type.",
                            &[
                                "all",
                                "animated_avatar",
                                "avatar_frame",
                                "mini_profile_background",
                                "profile_background",
                            ],
                            Some("all"),
                        ),
                    ),
                    ("language", p_lang()),
                    p_force(),
                ],
                &["steamid"],
            ),
            profile_items,
        ),
        def(
            "steam_get_top_achievements_for_games",
            "Top Achievements per Game",
            "A player's rarest unlocked achievements for up to 100 games.",
            PUBLIC,
            k,
            object_schema(
                vec![
                    ("steamid", p_steamid()),
                    ("appids", ids("App IDs.", 100)),
                    (
                        "max_achievements",
                        p_int("Achievements per game.", 1, 8, Some(5)),
                    ),
                    ("language", p_lang()),
                    p_force(),
                ],
                &["steamid", "appids"],
            ),
            top_achievements,
        ),
        def(
            "steam_get_workshop_details",
            "Workshop Details (Rich)",
            "Rich workshop item details: tags, votes, child items, metadata, previews and for-sale data.",
            PUBLIC,
            k,
            object_schema(
                vec![
                    ("publishedfileids", strings("Workshop item IDs.", 100)),
                    ("include_votes", p_bool("Include vote data.", true)),
                    ("include_children", p_bool("Include child items.", false)),
                    (
                        "include_metadata",
                        p_bool("Include developer metadata.", false),
                    ),
                    (
                        "include_for_sale_data",
                        p_bool("Include for-sale data.", false),
                    ),
                    p_force(),
                ],
                &["publishedfileids"],
            ),
            workshop_details,
        ),
        def(
            "steam_get_user_workshop_files",
            "User Workshop Files",
            "Workshop items a player created, favorited or subscribed to for an app.",
            PUBLIC,
            k,
            object_schema(
                vec![
                    ("steamid", p_steamid()),
                    ("appid", p_appid()),
                    (
                        "type",
                        p_enum(
                            "Which list.",
                            &["myfiles", "myfavorites", "subscribed"],
                            Some("myfiles"),
                        ),
                    ),
                    ("page", p_int("Page.", 1, 1000, Some(1))),
                    ("numperpage", p_int("Items per page.", 1, 100, Some(20))),
                    p_force(),
                ],
                &["steamid", "appid"],
            ),
            user_workshop_files,
        ),
        def(
            "steam_get_workshop_subsections",
            "Workshop Sub-sections",
            "Sub-section content (guides and documentation sections) of a workshop item.",
            PUBLIC,
            k,
            object_schema(
                vec![
                    ("publishedfileid", p_u64("Workshop item ID.")),
                    ("sectionid", p_u64("Only this section.")),
                    (
                        "table_of_contents",
                        p_bool("Return only the table of contents.", false),
                    ),
                    p_force(),
                ],
                &["publishedfileid"],
            ),
            workshop_subsection,
        ),
        def(
            "steam_get_ugc_file_details",
            "UGC File Details",
            "Metadata and download URL for a user-generated content file by UGC ID.",
            PUBLIC,
            k,
            object_schema(
                vec![
                    (
                        "ugcid",
                        obj! { "type" => ["string", "integer"], "description" => "UGC handle (64-bit)." },
                    ),
                    ("appid", p_appid()),
                    ("steamid", p_steamid()),
                    p_force(),
                ],
                &["ugcid", "appid"],
            ),
            ugc_file_details,
        ),
        def(
            "steam_get_store_categories",
            "Store Categories",
            "All store categories (features, player modes, controller support, accessibility) with IDs and names. Works without a key.",
            PUBLIC,
            n,
            object_schema(vec![("language", p_lang()), p_force()], &[]),
            store_categories,
        ),
        def(
            "steam_query_store",
            "Browse Store",
            "Browses the Steam store by tags (names or IDs), type and free-only, returning store items with prices, review scores and tags. Works without a key.",
            PUBLIC,
            n,
            object_schema(
                vec![
                    (
                        "tags",
                        obj! { "type" => "array", "items" => obj! { "type" => ["string", "integer"] }, "maxItems" => 10, "description" => "Tags every result must have, e.g. [\"Roguelike\", \"Co-op\"]." },
                    ),
                    (
                        "types",
                        obj! { "type" => "array", "items" => obj! { "type" => "string", "enum" => crate::arr!["games", "dlc", "software", "videos", "hardware", "music", "demos"] }, "description" => "Item types (default games)." },
                    ),
                    ("free_only", p_bool("Only free items.", false)),
                    ("start", p_int("Offset.", 0, 10000, Some(0))),
                    ("count", p_int("Results.", 1, 100, Some(20))),
                    ("cc", p_cc()),
                    ("language", p_lang()),
                    p_force(),
                ],
                &[],
            ),
            query_store,
        ),
        def(
            "steam_get_wishlist_detailed",
            "Wishlist with Store Data",
            "A player's public wishlist with prices, review scores and release dates for each item. Works without a key.",
            PUBLIC,
            n,
            object_schema(
                vec![
                    ("steamid", p_steamid()),
                    ("start", p_int("Offset.", 0, 100000, Some(0))),
                    ("count", p_int("Items.", 1, 500, Some(50))),
                    ("cc", p_cc()),
                    ("language", p_lang()),
                    p_force(),
                ],
                &["steamid"],
            ),
            wishlist_sorted,
        ),
        def(
            "steam_get_dlc_for_apps",
            "DLC for Apps",
            "All DLC for up to 50 apps.",
            PUBLIC,
            k,
            object_schema(
                vec![
                    ("appids", ids("App IDs.", 50)),
                    ("cc", p_cc()),
                    ("language", p_lang()),
                    p_force(),
                ],
                &["appids"],
            ),
            dlc_for_apps,
        ),
        def(
            "steam_get_server_steamids_by_ip",
            "Server SteamIDs by IP",
            "Game server SteamIDs registered at IP[:port] addresses.",
            PUBLIC,
            k,
            object_schema(
                vec![
                    ("ips", strings("Addresses, e.g. [\"1.2.3.4:27015\"].", 100)),
                    p_force(),
                ],
                &["ips"],
            ),
            server_steamids_by_ip,
        ),
        def(
            "steam_get_server_ips_by_steamid",
            "Server IPs by SteamID",
            "Addresses of game servers by their SteamIDs.",
            PUBLIC,
            k,
            object_schema(
                vec![
                    ("steamids", strings("Game server SteamIDs.", 100)),
                    p_force(),
                ],
                &["steamids"],
            ),
            server_ips_by_steamid,
        ),
        def(
            "steam_get_game_server_accounts",
            "Game Server Accounts",
            "Persistent game server accounts (GSLTs) owned by the key's account, with expiry and last logon.",
            PUBLIC,
            k,
            object_schema(vec![p_force()], &[]),
            server_accounts,
        ),
        def(
            "steam_get_game_server_account_info",
            "Game Server Account Info",
            "Public information about a game server account.",
            PUBLIC,
            k,
            object_schema(vec![("steamid", p_steamid()), p_force()], &["steamid"]),
            server_account_info,
        ),
        def(
            "steam_query_game_server_token",
            "Query Game Server Token",
            "Validates a game server login token (GSLT) and returns its account and ban status. Never cached.",
            PUBLIC,
            k,
            object_schema(
                vec![(
                    "login_token",
                    obj! { "type" => "string", "minLength" => 8, "description" => "The login token." },
                )],
                &["login_token"],
            ),
            query_login_token,
        ),
        def(
            "steam_get_game_server_player_stats",
            "Game Server Player Stats",
            "Player stats reported by game servers for an app.",
            PUBLIC,
            k,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("gameid", p_u64("Game ID (usually the app ID).")),
                    ("rangestart", unix_or_date("Start")),
                    ("rangeend", unix_or_date("End")),
                    ("maxresults", p_int("Maximum results.", 1, 1000, Some(100))),
                    p_force(),
                ],
                &["appid"],
            ),
            server_player_stats,
        ),
        def(
            "steam_partner_get_news_authed",
            "News (Including Unpublished)",
            "Your app's news including unpublished and hidden items.",
            PARTNER,
            pk,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("count", p_int("Items.", 1, 100, Some(10))),
                    (
                        "maxlength",
                        p_int("Truncate contents (0 = full).", 0, 20000, Some(600)),
                    ),
                    ("enddate", p_u64("Before this Unix time.")),
                    ("feeds", strings("Feed names.", 20)),
                    ("tags", strings("Tags.", 20)),
                    p_force(),
                ],
                &["appid"],
            ),
            news_authed,
        ),
        ToolDef {
            scope: scopes::FINANCIAL,
            ..def(
                "steam_partner_get_workshop_item_revenue",
                "Workshop Item Revenue",
                "Daily revenue by country for a curated workshop item.",
                PARTNER,
                pk,
                object_schema(
                    vec![
                        ("appid", p_appid()),
                        ("item_id", p_u64("Workshop item ID.")),
                        ("date_start", p_u64("Start (Unix time).")),
                        ("date_end", p_u64("End (Unix time).")),
                        p_force(),
                    ],
                    &["appid", "item_id", "date_start", "date_end"],
                ),
                item_daily_revenue,
            )
        },
        def(
            "steam_partner_get_market_prices",
            "Market Prices",
            "Community Market price data for your app's items.",
            PARTNER,
            pk,
            object_schema(vec![("appid", p_appid()), p_force()], &["appid"]),
            market_prices,
        ),
        def(
            "steam_partner_can_trade",
            "Can Trade",
            "Whether two users can trade items of your app.",
            PARTNER,
            pk,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("steamid", p_steamid()),
                    ("targetid", p_steamid()),
                    p_force(),
                ],
                &["appid", "steamid", "targetid"],
            ),
            can_trade,
        ),
        def(
            "steam_partner_get_market_popular",
            "Popular Market Listings",
            "Popular Community Market listings, optionally for one app.",
            PARTNER,
            pk,
            object_schema(
                vec![
                    ("appid", p_appid_plain("Only this app.")),
                    ("rows", p_int("Rows.", 1, 100, Some(20))),
                    ("start", p_int("Offset.", 0, 10000, Some(0))),
                    (
                        "currency",
                        p_int("Steam ECurrency code (1 = USD).", 1, 50, None),
                    ),
                    ("language", p_lang()),
                    p_force(),
                ],
                &[],
            ),
            market_popular,
        ),
        def(
            "steam_partner_get_market_asset_id",
            "Market Listing Asset",
            "The asset behind a Community Market listing.",
            PARTNER,
            pk,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("listingid", p_u64("Listing ID.")),
                    p_force(),
                ],
                &["appid", "listingid"],
            ),
            market_asset_id,
        ),
        def(
            "steam_partner_get_market_eligibility",
            "Market Eligibility",
            "Whether a user can use the Community Market, and why not.",
            PARTNER,
            pk,
            object_schema(vec![("steamid", p_steamid()), p_force()], &["steamid"]),
            market_eligibility,
        ),
        def(
            "steam_partner_get_inventory_item_defs",
            "Inventory Item Definitions (Service)",
            "Item definitions from the Inventory Service, optionally filtered by IDs or modification time.",
            PARTNER,
            pk,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("itemdefids", ids("Only these item definition IDs.", 500)),
                    ("workshopids", ids("Only these workshop IDs.", 500)),
                    (
                        "modifiedsince",
                        obj! { "type" => "string", "description" => "Only definitions modified since this time (RFC 3339)." },
                    ),
                    p_force(),
                ],
                &["appid"],
            ),
            inventory_item_defs,
        ),
        def(
            "steam_partner_get_inventory_price_sheet",
            "Inventory Price Sheet",
            "The Inventory Service store price sheet in a currency.",
            PARTNER,
            pk,
            object_schema(
                vec![
                    (
                        "currency",
                        p_int("Steam ECurrency code (1 = USD).", 1, 50, Some(1)),
                    ),
                    ("currency_code", p_str("ISO currency code alternative.")),
                    p_force(),
                ],
                &[],
            ),
            inventory_price_sheet,
        ),
        def(
            "steam_partner_get_inventory_quantity",
            "Inventory Quantities",
            "How many of given item definitions a user holds.",
            PARTNER,
            pk,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("steamid", p_steamid()),
                    ("itemdefids", ids("Item definition IDs.", 100)),
                    p_force(),
                ],
                &["appid", "steamid", "itemdefids"],
            ),
            inventory_quantity,
        ),
        def(
            "steam_partner_get_inventory_history",
            "Inventory History",
            "A user's inventory history for your app over a time range.",
            PARTNER,
            pk,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("steamid", p_steamid()),
                    ("contextid", p_u64("Inventory context (default 2).")),
                    ("starttime", p_u64("Start (Unix time).")),
                    ("endtime", p_u64("End (Unix time).")),
                    p_force(),
                ],
                &["appid", "steamid", "starttime", "endtime"],
            ),
            inventory_history,
        ),
        def(
            "steam_partner_get_inventory_history_command",
            "Inventory History Command",
            "Details of a single history command for a user's inventory.",
            PARTNER,
            pk,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("steamid", p_steamid()),
                    ("command", p_str("History command.")),
                    ("contextid", p_u64("Inventory context (default 2).")),
                    ("arguments", p_str("Command arguments.")),
                    p_force(),
                ],
                &["appid", "steamid", "command", "arguments"],
            ),
            inventory_history_command,
        ),
        def(
            "steam_partner_get_asset_history",
            "Asset History",
            "Support view of an inventory asset's history.",
            PARTNER,
            pk,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("assetid", p_u64("Asset ID.")),
                    ("contextid", p_u64("Inventory context (default 2).")),
                    p_force(),
                ],
                &["appid", "assetid"],
            ),
            asset_history,
        ),
        def(
            "steam_partner_get_notification_sessions",
            "Game Notification Sessions",
            "Asynchronous game notification sessions for a user in your app.",
            PARTNER,
            pk,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("steamid", p_steamid()),
                    (
                        "include_all_user_messages",
                        p_bool("Include all user messages.", false),
                    ),
                    ("language", p_lang()),
                    p_force(),
                ],
                &["appid", "steamid"],
            ),
            notification_sessions,
        ),
        def(
            "steam_partner_get_notification_session_details",
            "Notification Session Details",
            "Details of specific game notification sessions.",
            PARTNER,
            pk,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("sessionids", strings("Session IDs.", 50)),
                    ("language", p_lang()),
                    p_force(),
                ],
                &["appid", "sessionids"],
            ),
            session_details,
        ),
        def(
            "steam_partner_get_lobby_data",
            "Lobby Data",
            "Metadata and members of a matchmaking lobby in your app.",
            PARTNER,
            pk,
            object_schema(
                vec![
                    ("appid", p_appid()),
                    ("lobby_steamid", p_steamid()),
                    p_force(),
                ],
                &["appid", "lobby_steamid"],
            ),
            lobby_data,
        ),
        def(
            "steam_partner_get_microtxn_user_info",
            "Microtransaction User Info",
            "A user's country, currency and wallet state for microtransactions.",
            PARTNER,
            pk,
            object_schema(
                vec![
                    ("steamid", p_steamid()),
                    (
                        "ipaddress",
                        p_str("User IP for regional pricing (optional)."),
                    ),
                    ("sandbox", p_bool("Use the sandbox interface.", false)),
                    p_force(),
                ],
                &["steamid"],
            ),
            microtxn_user,
        ),
        def(
            "steam_partner_get_microtxn_agreements",
            "Recurring Agreements",
            "A user's recurring billing agreements (subscriptions) for your app.",
            PARTNER,
            pk,
            object_schema(
                vec![("steamid", p_steamid()), ("appid", p_appid()), p_force()],
                &["steamid", "appid"],
            ),
            microtxn_agreements,
        ),
        ToolDef {
            hints: Hints {
                read_only: true,
                destructive: false,
                idempotent: false,
                open_world: true,
            },
            ..def(
                "steam_partner_get_vac_status",
                "VAC Status",
                "VAC ban status for a user in a secure multiplayer session of your app. Never cached.",
                PARTNER,
                pk,
                object_schema(
                    vec![
                        ("steamid", p_steamid()),
                        ("appid", p_appid()),
                        ("session_id", p_u64("Secure session ID.")),
                    ],
                    &["steamid", "appid"],
                ),
                vac_status,
            )
        },
    ]
}
