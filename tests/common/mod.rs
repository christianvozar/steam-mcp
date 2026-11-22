//! Shared test harness: a mock Steam server and MCP request helpers.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::BufReader;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use steam_mcp::config::{Config, ConfigHandle, Sources};
use steam_mcp::http::server::{read_request, write_response};
use steam_mcp::json::{self, Value};
use steam_mcp::mcp::context::{Principal, Sink, Transport};
use steam_mcp::mcp::server::{Core, Reply};
use steam_mcp::obj;
use steam_mcp::util::CancelToken;
use steam_mcp::util::encoding::parse_query;

pub const KEY: &str = "TESTUSERKEY";
pub const PUB_KEY: &str = "TESTPUBLISHERKEY";
pub const FIN_KEY: &str = "TESTFINANCIALKEY";

#[derive(Clone)]
pub struct Mock {
    pub base: String,
    pub hits: Arc<Mutex<HashMap<String, usize>>>,
    pub posts: Arc<Mutex<Vec<(String, String)>>>,
    pub players: Arc<AtomicU64>,
    pub sales_delay_ms: Arc<AtomicU64>,
    /// JWKS document served at /jwks.
    pub jwks: Arc<Mutex<String>>,
    /// Audience reported by /introspect for active tokens.
    pub introspect_aud: Arc<Mutex<String>>,
}

impl Mock {
    pub fn hits(&self, path: &str) -> usize {
        *self.hits.lock().unwrap().get(path).unwrap_or(&0)
    }
}

fn respond(
    mock: &Mock,
    method: &str,
    path: &str,
    q: &HashMap<String, String>,
    body: &str,
    authorization: Option<&str>,
) -> (u16, String) {
    *mock
        .hits
        .lock()
        .unwrap()
        .entry(path.to_string())
        .or_default() += 1;
    let key = q.get("key").map(String::as_str);
    let need = |k: &str| key == Some(k);
    if method == "POST" {
        mock.posts
            .lock()
            .unwrap()
            .push((path.to_string(), body.to_string()));
    }
    match path {
        "/api/appdetails" => {
            let id = q.get("appids").cloned().unwrap_or_default();
            if id == "570" || id == "730" {
                let name = if id == "570" { "Dota 2" } else { "Counter-Strike 2" };
                (200, obj! { id.as_str() => obj! { "success" => true, "data" => obj! {
                    "name" => name, "type" => "game", "is_free" => true,
                    "short_description" => "A [b]competitive[/b] game.",
                    "developers" => steam_mcp::arr!["Valve"], "publishers" => steam_mcp::arr!["Valve"],
                    "release_date" => obj! { "date" => "9 Jul, 2013", "coming_soon" => false },
                    "genres" => steam_mcp::arr![obj! { "description" => "Action" }],
                    "platforms" => obj! { "windows" => true },
                    "recommendations" => obj! { "total" => 2000000 },
                } } }.to_string())
            } else {
                (200, obj! { id.as_str() => obj! { "success" => false } }.to_string())
            }
        }
        "/ISteamUserStats/GetNumberOfCurrentPlayers/v1/" => {
            let n = mock.players.fetch_add(1, Ordering::SeqCst) + 1000;
            (200, obj! { "response" => obj! { "player_count" => n, "result" => 1 } }.to_string())
        }
        "/ISteamNews/GetNewsForApp/v2/" => (200, obj! { "appnews" => obj! { "appid" => 570, "count" => 1, "newsitems" => steam_mcp::arr![obj! { "gid" => "1", "title" => "Patch 7.37", "url" => "https://x", "contents" => "<p>Fixes</p>", "date" => 1785196800u64, "feedlabel" => "Community Announcements" }] } }.to_string()),
        "/ISteamUserStats/GetGlobalAchievementPercentagesForApp/v2/" => (200, obj! { "achievementpercentages" => obj! { "achievements" => steam_mcp::arr![obj! { "name" => "EASY", "percent" => "91.5" }, obj! { "name" => "HARD", "percent" => 0.4 }] } }.to_string()),
        // Mirrors the live storefront refusing a client: redirect to the home page.
        "/appreviews/9" => (302, String::new()),
        p if p.starts_with("/appreviews/") => {
            let cursor = q.get("cursor").cloned().unwrap_or_default();
            let page = if cursor == "*" { 0 } else { cursor.trim_start_matches("c").parse::<u64>().unwrap_or(0) };
            let reviews: Vec<Value> = if q.get("num_per_page").map(String::as_str) == Some("0") || page >= 3 {
                Vec::new()
            } else {
                (0..2).map(|i| obj! { "recommendationid" => format!("{page}{i}"), "voted_up" => i == 0, "review" => if i == 0 { "Great gameplay and great community" } else { "Too many bugs and crashes" }, "timestamp_created" => 1785196800u64, "author" => obj! { "steamid" => "76561197960287930", "playtime_at_review" => 600 } }).collect()
            };
            (200, obj! { "success" => 1, "query_summary" => obj! { "review_score" => 8, "review_score_desc" => "Very Positive", "total_positive" => 900, "total_negative" => 100, "total_reviews" => 1000 }, "reviews" => reviews, "cursor" => format!("c{}", page + 1) }.to_string())
        }
        "/ISteamUser/GetPlayerSummaries/v2/" => {
            if !need(KEY) {
                return (403, "<html>Forbidden</html>".into());
            }
            (200, obj! { "response" => obj! { "players" => steam_mcp::arr![obj! { "steamid" => "76561197960287930", "personaname" => "Rabscuttle", "communityvisibilitystate" => 3, "personastate" => 1 }] } }.to_string())
        }
        "/ISteamApps/GetPartnerAppListForWebAPIKey/v2/" => {
            if !need(PUB_KEY) {
                return (403, "Forbidden".into());
            }
            (200, obj! { "applist" => obj! { "apps" => obj! { "app" => steam_mcp::arr![obj! { "appid" => 480, "app_type" => "Game", "app_name" => "Spacewar" }, obj! { "appid" => 481, "app_type" => "DLC", "app_name" => "Spacewar Soundtrack" }] } } }.to_string())
        }
        "/ISteamApps/GetAppBetas/v1/" => (200, obj! { "response" => obj! { "result" => 1, "betas" => obj! { "public" => obj! { "BuildID" => 100, "Description" => "Release" }, "beta" => obj! { "BuildID" => 101, "ReqPassword" => true } } } }.to_string()),
        "/ISteamApps/GetAppBuilds/v1/" => (200, obj! { "response" => obj! { "result" => 1, "builds" => obj! { "100" => obj! { "BuildID" => 100, "Description" => "Release", "CreationTime" => 1785196800u64 }, "101" => obj! { "BuildID" => 101, "Description" => "RC", "CreationTime" => 1785283200u64 } } } }.to_string()),
        "/ISteamApps/SetAppBuildLive/v2/" => (200, obj! { "response" => obj! { "result" => 1 } }.to_string()),
        "/IPartnerFinancialsService/GetChangedDatesForPartner/v001/" => {
            if !need(FIN_KEY) {
                return (403, "Forbidden".into());
            }
            (200, obj! { "response" => obj! { "dates" => steam_mcp::arr!["2026/07/01", "2026/07/02"], "result_highwatermark" => "55" } }.to_string())
        }
        "/IPartnerFinancialsService/GetDetailedSales/v001/" => {
            std::thread::sleep(Duration::from_millis(mock.sales_delay_ms.load(Ordering::SeqCst)));
            let hw: u64 = q.get("highwatermark_id").and_then(|h| h.parse().ok()).unwrap_or(0);
            let row = |id: u64, app: u64, cc: &str, net: &str| obj! { "id" => id, "primary_appid" => app, "country_code" => cc, "platform" => "Windows", "net_sales_usd" => net, "gross_units_sold" => 1 };
            let (results, max) = match hw {
                0 => (steam_mcp::arr![row(1, 480, "US", "10.50"), row(2, 480, "DE", "5.25")], "10"),
                10 => (steam_mcp::arr![row(3, 481, "US", "1.99")], "11"),
                _ => (steam_mcp::arr![], "11"),
            };
            (200, obj! { "response" => obj! { "results" => results, "max_id" => max, "app_info" => steam_mcp::arr![obj! { "appid" => 480, "app_name" => "Spacewar" }] } }.to_string())
        }
        "/ISteamWebAPIUtil/GetSupportedAPIList/v1/" => (200, obj! { "apilist" => obj! { "interfaces" => steam_mcp::arr![obj! { "name" => "ISteamNews", "methods" => steam_mcp::arr![obj! { "name" => "GetNewsForApp", "version" => 2, "httpmethod" => "GET" }] }] } }.to_string()),
        "/ISteamChartsService/GetMostPlayedGames/v1/" => (200, obj! { "response" => obj! { "rollup_date" => 1785196800u64, "ranks" => steam_mcp::arr![obj! { "rank" => 1, "appid" => 730 }, obj! { "rank" => 2, "appid" => 570 }] } }.to_string()),
        "/api/featuredcategories" => (200, obj! { "top_sellers" => obj! { "items" => steam_mcp::arr![obj! { "id" => 730, "name" => "Counter-Strike 2" }] } }.to_string()),
        "/ISteamApps/GetAppList/v2/" => (200, obj! { "applist" => obj! { "apps" => steam_mcp::arr![obj! { "appid" => 570, "name" => "Dota 2" }, obj! { "appid" => 730, "name" => "Counter-Strike 2" }, obj! { "appid" => 10, "name" => "Counter-Strike" }, obj! { "appid" => 1000, "name" => "Dota Underlords" }] } }.to_string()),
        "/IStoreService/GetAppList/v1/" => {
            let last: u64 = q.get("last_appid").and_then(|v| v.parse().ok()).unwrap_or(0);
            let (apps, more) = if last == 0 {
                (steam_mcp::arr![obj! { "appid" => 10, "name" => "Counter-Strike" }, obj! { "appid" => 570, "name" => "Dota 2" }], true)
            } else {
                (steam_mcp::arr![obj! { "appid" => 730, "name" => "Counter-Strike 2" }, obj! { "appid" => 1000, "name" => "Dota Underlords" }], false)
            };
            (200, obj! { "response" => obj! { "apps" => apps, "have_more_results" => more, "last_appid" => if more { 570 } else { 1000 } } }.to_string())
        }
        "/IStoreBrowseService/GetItems/v1/" | "/IPartnerStoreBrowseService/GetItems/v1/" => {
            let input = json::parse(q.get("input_json").map(String::as_str).unwrap_or("{}")).unwrap_or_default();
            let items: Vec<Value> = input["ids"].as_array().cloned().unwrap_or_default().iter().map(|id| {
                let appid = id["appid"].as_u64().unwrap_or(0);
                if appid == 999 {
                    obj! { "id" => appid, "success" => 15, "appid" => 0 }
                } else {
                    obj! {
                        "id" => appid, "appid" => appid, "success" => 1, "name" => format!("App {appid}"),
                        "tagids" => steam_mcp::arr![492, 19],
                        "release" => obj! { "steam_release_date" => 1303186800u64 },
                        "best_purchase_option" => obj! { "formatted_final_price" => "$9.99", "discount_pct" => 0 },
                        "reviews" => obj! { "summary_filtered" => obj! { "review_count" => 200, "percent_positive" => 90, "review_score" => 8, "review_score_label" => "Very Positive" } },
                        "platforms" => obj! { "windows" => true, "steam_deck_compat_category" => 2 },
                    }
                }
            }).collect();
            (200, obj! { "response" => obj! { "store_items" => items } }.to_string())
        }
        "/IStoreQueryService/SearchSuggestions/v1/" => (200, obj! { "response" => obj! { "metadata" => obj! { "total_matching_records" => 2 }, "store_items" => steam_mcp::arr![obj! { "success" => 1, "appid" => 570, "name" => "Dota 2" }, obj! { "success" => 1, "appid" => 1000, "name" => "Dota Underlords" }] } }.to_string()),
        "/IStoreService/GetTagList/v1/" => (200, obj! { "response" => obj! { "tags" => steam_mcp::arr![obj! { "tagid" => 492, "name" => "Indie" }, obj! { "tagid" => 19, "name" => "Action" }] } }.to_string()),
        "/ICommunityService/GetApps/v1/" => {
            let input = json::parse(q.get("input_json").map(String::as_str).unwrap_or("{}")).unwrap_or_default();
            let apps: Vec<Value> = input["appids"].as_array().cloned().unwrap_or_default().iter().filter_map(|a| a.as_u64()).map(|a| obj! { "appid" => a, "name" => format!("Name {a}") }).collect();
            (200, obj! { "response" => obj! { "apps" => apps } }.to_string())
        }
        "/ISteamChartsService/GetGamesByConcurrentPlayers/v1/" => (200, obj! { "response" => obj! { "last_update" => 1795370971u64, "ranks" => steam_mcp::arr![obj! { "rank" => 1, "appid" => 730, "concurrent_in_game" => 1000000, "peak_in_game" => 1200000 }] } }.to_string()),
        "/IGCVersion_440/GetServerVersion/v1/" => (200, obj! { "result" => obj! { "success" => true, "active_version" => 11191711, "min_allowed_version" => 11191711 } }.to_string()),
        "/IPlayerService/GetProfileItemsEquipped/v1/" => (200, obj! { "response" => obj! { "profile_background" => obj! { "communityitemid" => "1", "item_title" => "Night Sky" }, "avatar_frame" => obj! {} } }.to_string()),
        "/IStoreBrowseService/GetStoreCategories/v1/" => (200, obj! { "response" => obj! { "categories" => steam_mcp::arr![obj! { "categoryid" => 2, "type" => 2, "internal_name" => "singleplayer", "display_name" => "Single-player" }] } }.to_string()),
        "/IStoreQueryService/Query/v1/" => {
            let input = q.get("input_json").cloned().unwrap_or_default();
            assert!(input.contains("\"tagids\":[492]"), "tag names resolve to ids: {input}");
            (200, obj! { "response" => obj! { "metadata" => obj! { "total_matching_records" => 1234 }, "store_items" => steam_mcp::arr![obj! { "success" => 1, "appid" => 620, "name" => "Portal 2", "tagids" => steam_mcp::arr![492] }] } }.to_string())
        }
        "/IGameServersService/GetAccountList/v1/" => (200, obj! { "response" => obj! { "servers" => steam_mcp::arr![obj! { "steamid" => "85568392920040000", "appid" => 730, "memo" => "eu-1", "is_expired" => false }] } }.to_string()),
        "/ISteamMicroTxn/GetUserAgreementInfo/v2/" => {
            if !need(PUB_KEY) {
                return (403, "Forbidden".into());
            }
            (200, obj! { "response" => obj! { "result" => "OK", "params" => obj! { "agreements" => steam_mcp::arr![obj! { "agreementid" => "7", "status" => "Active" }] } } }.to_string())
        }
        "/ICheatReportingService/RequestVacStatusForUser/v1/" => (200, obj! { "response" => obj! { "success" => true, "session_verified" => true } }.to_string()),
        "/ISteamLeaderboards/GetLeaderboardsForGame/v2/" => (200, obj! { "response" => obj! { "result" => 1, "leaderboards" => steam_mcp::arr![obj! { "id" => 77, "name" => "Fastest", "entries" => 3 }] } }.to_string()),
        "/ISteamLeaderboards/ResetLeaderboard/v1/" => (200, obj! { "result" => obj! { "result" => 1 } }.to_string()),
        "/ISteamLeaderboards/DeleteLeaderboard/v1/" => (200, obj! { "result" => obj! { "result" => 8, "message" => "Leaderboard not found" } }.to_string()),
        "/IGameServersService/CreateAccount/v1/" => (200, obj! { "response" => obj! { "steamid" => "85568392920040001", "login_token" => "SECRETGSLT0123456789" } }.to_string()),
        "/IInventoryService/AddItem/v1/" => (200, obj! { "response" => obj! { "item_json" => "[{\"itemid\":\"9001\"}]" } }.to_string()),
        "/ISteamMicroTxnSandbox/InitTxn/v3/" => (200, obj! { "response" => obj! { "result" => "OK", "params" => obj! { "orderid" => "42", "transid" => "777" } } }.to_string()),
        "/ISteamMicroTxn/RefundTxn/v2/" => (200, obj! { "response" => obj! { "result" => "Failure", "error" => obj! { "errorcode" => 100, "errordesc" => "Order not found" } } }.to_string()),
        "/ISteamMicroTxnSandbox/QueryTxn/v3/" => (200, obj! { "response" => obj! { "result" => "OK", "params" => obj! { "orderid" => "42", "status" => "Approved", "items" => steam_mcp::arr![obj! { "itemid" => 5, "qty" => 1, "amount" => 199 }] } } }.to_string()),
        "/ISteamUserStats/SetUserStatsForGame/v1/" => (200, obj! { "result" => obj! { "result" => 1 } }.to_string()),
        "/IPublishedFileService/UpdateTags/v1/" => (200, obj! { "response" => obj! {} }.to_string()),
        "/jwks" => (200, mock.jwks.lock().unwrap().clone()),
        "/introspect" => {
            let expected = format!(
                "Basic {}",
                steam_mcp::util::encoding::b64_encode(b"mcp-client:s3cret")
            );
            if authorization != Some(expected.as_str()) {
                return (401, r#"{"error":"invalid_client"}"#.into());
            }
            let now = steam_mcp::util::time::now_secs();
            match q.get("token").map(String::as_str) {
                Some("good-token") => (
                    200,
                    obj! {
                        "active" => true,
                        "sub" => "svc-account",
                        "aud" => mock.introspect_aud.lock().unwrap().clone(),
                        "iss" => "https://auth.example.com",
                        "scope" => "steam:partner",
                        "exp" => now + 300,
                    }
                    .to_string(),
                ),
                Some("other-aud") => (
                    200,
                    obj! { "active" => true, "sub" => "x", "aud" => "https://elsewhere.example.com", "exp" => now + 300 }
                        .to_string(),
                ),
                _ => (200, r#"{"active":false}"#.into()),
            }
        }
        "/flaky" => (503, "down".into()),
        _ => (404, "not found".into()),
    }
}

/// Starts a mock Steam server on a random port.
pub fn start_mock() -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mock = Mock {
        base: format!("http://{addr}"),
        hits: Arc::new(Mutex::new(HashMap::new())),
        posts: Arc::new(Mutex::new(Vec::new())),
        players: Arc::new(AtomicU64::new(0)),
        sales_delay_ms: Arc::new(AtomicU64::new(0)),
        jwks: Arc::new(Mutex::new("{\"keys\":[]}".into())),
        introspect_aud: Arc::new(Mutex::new(String::new())),
    };
    let m = mock.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let m = m.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                while let Ok(Some(req)) = read_request(&mut reader, 65536, 1 << 20) {
                    let body = String::from_utf8_lossy(&req.body).to_string();
                    let mut q: HashMap<String, String> =
                        parse_query(req.query.as_deref().unwrap_or(""))
                            .into_iter()
                            .collect();
                    if req.method == "POST" {
                        q.extend(parse_query(&body));
                    }
                    let (status, resp) = respond(
                        &m,
                        &req.method,
                        &req.path,
                        &q,
                        &body,
                        req.headers.get("authorization"),
                    );
                    if write_response(
                        reader.get_mut(),
                        status,
                        &[("Content-Type", "application/json".into())],
                        resp.as_bytes(),
                        true,
                    )
                    .is_err()
                    {
                        break;
                    }
                }
            });
        }
    });
    mock
}

pub fn config(mock: &Mock, extra: &[(&str, &str)]) -> Config {
    let mut kv: HashMap<String, String> = [
        ("STEAM_API_KEY", KEY),
        ("STEAM_PUBLISHER_KEY", PUB_KEY),
        ("STEAM_FINANCIAL_KEY", FIN_KEY),
        ("STEAM_MCP_UPSTREAM_RETRIES", "0"),
        ("STEAM_MCP_UPSTREAM_RPS", "1000"),
        ("STEAM_MCP_UPSTREAM_BURST", "1000"),
        ("STEAM_MCP_CLIENT_RPS", "1000"),
        ("STEAM_MCP_CLIENT_BURST", "1000"),
        ("STEAM_MCP_USE_PROXY_ENV", "false"),
        ("STEAM_MCP_STATE_SECRET", "test-secret"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    kv.insert("STEAM_MCP_API_BASE".into(), mock.base.clone());
    kv.insert("STEAM_MCP_PARTNER_BASE".into(), mock.base.clone());
    kv.insert("STEAM_MCP_STORE_BASE".into(), mock.base.clone());
    for (k, v) in extra {
        kv.insert(k.to_string(), v.to_string());
    }
    Config::from_map(&kv).unwrap()
}

pub fn core_with(mock: &Mock, extra: &[(&str, &str)]) -> Arc<Core> {
    steam_mcp::util::log::set_level(steam_mcp::util::log::Level::Error);
    Core::new(ConfigHandle::new(config(mock, extra), Sources::default())).unwrap()
}

/// Collects request-scoped notifications.
#[derive(Default)]
pub struct Capture(pub Mutex<Vec<Value>>);

impl Sink for Capture {
    fn send(&self, msg: &Value) -> bool {
        self.0.lock().unwrap().push(msg.clone());
        true
    }
}

impl Capture {
    pub fn messages(&self) -> Vec<Value> {
        self.0.lock().unwrap().clone()
    }
}

pub fn meta(caps: Value) -> Value {
    obj! {
        "io.modelcontextprotocol/protocolVersion" => "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities" => caps,
        "io.modelcontextprotocol/clientInfo" => obj! { "name" => "test", "version" => "1.0" },
    }
}

pub fn request(id: i64, method: &str, mut params: Value, caps: Value) -> Value {
    let mut m = meta(caps);
    if let Some(extra) = params.get("_meta").and_then(|x| x.as_object()).cloned() {
        for (k, v) in extra {
            m.set(&k, v);
        }
    }
    params.set("_meta", m);
    obj! { "jsonrpc" => "2.0", "id" => id, "method" => method, "params" => params }
}

/// Sends one message; returns (response, notifications).
pub fn send(core: &Arc<Core>, msg: &Value) -> (Value, Vec<Value>) {
    send_as(core, msg, Principal::local())
}

pub fn send_as(core: &Arc<Core>, msg: &Value, principal: Principal) -> (Value, Vec<Value>) {
    let cap = Arc::new(Capture::default());
    let reply = core.handle_message(
        msg,
        principal,
        Transport::Internal,
        cap.clone(),
        CancelToken::new(),
    );
    let resp = match reply {
        Reply::Response(r, _) => r,
        Reply::NoResponse => Value::Null,
        Reply::Subscription(s) => obj! { "subscription" => s.id.clone() },
    };
    (resp, cap.messages())
}

pub fn call_tool(core: &Arc<Core>, name: &str, args: Value) -> Value {
    send(
        core,
        &request(
            1,
            "tools/call",
            obj! { "name" => name, "arguments" => args },
            obj! {},
        ),
    )
    .0
}

pub fn parse(s: &str) -> Value {
    json::parse(s).unwrap()
}
