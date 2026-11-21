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
                    let (status, resp) = respond(&m, &req.method, &req.path, &q, &body);
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
