//! Live verification against the real Steam APIs. Ignored by default.
//!
//! ```sh
//! STEAM_API_KEY=… STEAM_PUBLISHER_KEY=… STEAM_FINANCIAL_KEY=… \
//! STEAM_LIVE_APPID=<one of your apps> STEAM_LIVE_STEAMID=<a public profile> \
//!   cargo test --release --test live -- --ignored --nocapture
//! ```
//!
//! Every read-only tool whose credential is configured is called once. The
//! run writes `target/live-report.md` and fails only on endpoint-level
//! problems (unknown method/version, rejected parameter names), which mean
//! the code is wrong. Data-level outcomes (private profiles, no permission
//! for an app) are reported, not failed.
//!
//! Optional: `STEAM_LIVE_DATE` (a sales date), `STEAM_LIVE_LEADERBOARD`,
//! `STEAM_LIVE_PUBLIC_APPID` (default 620, Portal 2).

use std::fmt::Write as _;
use std::sync::Arc;

use steam_mcp::config::{Config, ConfigHandle, Sources};
use steam_mcp::json::Value;
use steam_mcp::mcp::context::{NullSink, Principal, Transport};
use steam_mcp::mcp::server::{Core, Reply};
use steam_mcp::obj;
use steam_mcp::util::CancelToken;

fn env(k: &str, d: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| d.to_string())
}

fn call(core: &Arc<Core>, id: i64, name: &str, args: Value) -> Value {
    let msg = obj! {
        "jsonrpc" => "2.0", "id" => id, "method" => "tools/call",
        "params" => obj! { "name" => name, "arguments" => args, "_meta" => obj! {
            "io.modelcontextprotocol/protocolVersion" => "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities" => obj! {},
        } },
    };
    match core.handle_message(
        &msg,
        Principal::local(),
        Transport::Internal,
        Arc::new(NullSink),
        CancelToken::new(),
    ) {
        Reply::Response(r, _) => r,
        _ => Value::Null,
    }
}

/// Endpoint-level failures that indicate a code bug rather than data access.
fn endpoint_problem(text: &str) -> bool {
    text.contains("does not recognise")
        || text.contains("rejected the request parameters")
        || text.contains("non-JSON response")
}

#[test]
#[ignore = "calls the real Steam APIs; run with --ignored and real keys"]
fn live_verification_report() {
    steam_mcp::util::log::set_level(steam_mcp::util::log::Level::Warn);
    let cfg = Config::load(&Sources::default()).expect("configuration");
    let core = Core::new(ConfigHandle::new(cfg.clone(), Sources::default())).expect("core");

    let app = env("STEAM_LIVE_PUBLIC_APPID", "620");
    let app: u64 = app.parse().expect("STEAM_LIVE_PUBLIC_APPID");
    let partner_app: u64 = env("STEAM_LIVE_APPID", "480")
        .parse()
        .expect("STEAM_LIVE_APPID");
    let steamid = env("STEAM_LIVE_STEAMID", "76561197960287930");
    let date = std::env::var("STEAM_LIVE_DATE").ok();
    let board: u64 = env("STEAM_LIVE_LEADERBOARD", "0").parse().unwrap_or(0);

    let mut plan: Vec<(&str, Value)> = vec![
        ("steam_get_app_details", obj! { "appid" => app }),
        ("steam_get_current_players", obj! { "appid" => app }),
        ("steam_get_review_summary", obj! { "appid" => app }),
        (
            "steam_get_app_reviews",
            obj! { "appid" => app, "num_per_page" => 5, "max_pages" => 2 },
        ),
        ("steam_get_app_news", obj! { "appid" => app, "count" => 3 }),
        (
            "steam_get_global_achievement_percentages",
            obj! { "appid" => app },
        ),
        ("steam_get_game_schema", obj! { "appid" => app }),
        ("steam_get_most_played_games", obj! { "limit" => 5 }),
        (
            "steam_get_workshop_item_details",
            obj! { "publishedfileids" => steam_mcp::arr!["2064938225"] },
        ),
        (
            "steam_query_workshop_files",
            obj! { "appid" => 440, "numperpage" => 3 },
        ),
        ("steam_get_store_featured", obj! {}),
        (
            "steam_get_package_details",
            obj! { "packageids" => steam_mcp::arr![469] },
        ),
        ("steam_get_servers_at_address", obj! { "addr" => "8.8.8.8" }),
        (
            "steam_check_app_up_to_date",
            obj! { "appid" => 730, "version" => 1 },
        ),
        (
            "steam_get_game_servers",
            obj! { "appid" => 730, "limit" => 3 },
        ),
        (
            "steam_get_supported_api_list",
            obj! { "filter" => "ISteamUser" },
        ),
        ("steam_search_apps", obj! { "query" => "portal" }),
        ("steam_get_app_overview", obj! { "appid" => app }),
        (
            "steam_compare_apps",
            obj! { "appids" => steam_mcp::arr![570, 730] },
        ),
        (
            "steam_get_player_summaries",
            obj! { "steamids" => steam_mcp::arr![steamid.as_str()] },
        ),
        (
            "steam_resolve_vanity_url",
            obj! { "vanity" => "gabelogannewell" },
        ),
        (
            "steam_get_owned_games",
            obj! { "steamid" => steamid.as_str(), "limit" => 5 },
        ),
        (
            "steam_get_recently_played_games",
            obj! { "steamid" => steamid.as_str() },
        ),
        (
            "steam_get_player_achievements",
            obj! { "steamid" => steamid.as_str(), "appid" => app },
        ),
        (
            "steam_get_user_stats_for_game",
            obj! { "steamid" => steamid.as_str(), "appid" => app },
        ),
        (
            "steam_get_friend_list",
            obj! { "steamid" => steamid.as_str() },
        ),
        (
            "steam_get_player_bans",
            obj! { "steamids" => steam_mcp::arr![steamid.as_str()] },
        ),
        (
            "steam_get_steam_level",
            obj! { "steamid" => steamid.as_str() },
        ),
        (
            "steam_get_badges",
            obj! { "steamid" => steamid.as_str(), "limit" => 3 },
        ),
        (
            "steam_get_player_profile",
            obj! { "steamid" => steamid.as_str() },
        ),
        (
            "steam_get_wishlist",
            obj! { "steamid" => steamid.as_str(), "limit" => 3 },
        ),
        (
            "steam_get_store_items",
            obj! { "appids" => steam_mcp::arr![app, 570] },
        ),
        ("steam_search_store", obj! { "query" => "portal" }),
        ("steam_get_store_tags", obj! { "popular_only" => true }),
        ("steam_get_weekly_top_sellers", obj! { "count" => 3 }),
        ("steam_get_top_concurrent_games", obj! { "limit" => 3 }),
        ("steam_get_top_steam_deck_games", obj! { "limit" => 3 }),
        ("steam_get_top_releases", obj! { "pages" => 1 }),
        ("steam_get_gc_version", obj! { "appid" => 440 }),
        ("steam_get_sdr_config", obj! { "appid" => 730 }),
        (
            "steam_get_wishlist_count",
            obj! { "steamid" => steamid.as_str() },
        ),
        (
            "steam_get_followed_games",
            obj! { "steamid" => steamid.as_str() },
        ),
        (
            "steam_get_workshop_vote_summary",
            obj! { "publishedfileids" => steam_mcp::arr![2064938225u64] },
        ),
        (
            "steam_get_single_game_playtime",
            obj! { "steamid" => steamid.as_str(), "appid" => app },
        ),
        (
            "steam_is_playing_shared_game",
            obj! { "steamid" => steamid.as_str(), "appid" => app },
        ),
        (
            "steam_get_badge_progress",
            obj! { "steamid" => steamid.as_str() },
        ),
        (
            "steam_get_user_group_list",
            obj! { "steamid" => steamid.as_str() },
        ),
        (
            "steam_get_asset_class_info",
            obj! { "appid" => 440, "classids" => steam_mcp::arr![101785959u64] },
        ),
        (
            "steam_get_profile_items",
            obj! { "steamid" => steamid.as_str() },
        ),
        ("steam_get_store_categories", obj! {}),
        (
            "steam_query_store",
            obj! { "tags" => steam_mcp::arr!["Indie"], "count" => 3 },
        ),
        (
            "steam_get_wishlist_detailed",
            obj! { "steamid" => steamid.as_str(), "count" => 3 },
        ),
        (
            "steam_get_dlc_for_apps",
            obj! { "appids" => steam_mcp::arr![app] },
        ),
        (
            "steam_get_top_achievements_for_games",
            obj! { "steamid" => steamid.as_str(), "appids" => steam_mcp::arr![app] },
        ),
        (
            "steam_get_workshop_details",
            obj! { "publishedfileids" => steam_mcp::arr!["2064938225"] },
        ),
        (
            "steam_get_user_workshop_files",
            obj! { "steamid" => steamid.as_str(), "appid" => 440 },
        ),
        ("steam_get_game_server_accounts", obj! {}),
        (
            "steam_get_server_steamids_by_ip",
            obj! { "ips" => steam_mcp::arr!["162.254.192.1:27015"] },
        ),
        ("steam_partner_list_apps", obj! {}),
        (
            "steam_partner_get_news_authed",
            obj! { "appid" => partner_app, "count" => 3 },
        ),
        (
            "steam_partner_get_market_prices",
            obj! { "appid" => partner_app },
        ),
        ("steam_partner_get_market_popular", obj! { "rows" => 3 }),
        (
            "steam_partner_get_market_eligibility",
            obj! { "steamid" => steamid.as_str() },
        ),
        (
            "steam_partner_get_inventory_item_defs",
            obj! { "appid" => partner_app },
        ),
        ("steam_partner_get_inventory_price_sheet", obj! {}),
        (
            "steam_partner_get_microtxn_user_info",
            obj! { "steamid" => steamid.as_str(), "sandbox" => true },
        ),
        (
            "steam_partner_get_microtxn_agreements",
            obj! { "steamid" => steamid.as_str(), "appid" => partner_app },
        ),
        (
            "steam_partner_get_notification_sessions",
            obj! { "appid" => partner_app, "steamid" => steamid.as_str() },
        ),
        (
            "steam_partner_get_store_items",
            obj! { "appids" => steam_mcp::arr![partner_app] },
        ),
        (
            "steam_partner_check_app_ownership",
            obj! { "steamid" => steamid.as_str(), "appid" => partner_app },
        ),
        (
            "steam_partner_get_publisher_app_ownership",
            obj! { "steamid" => steamid.as_str() },
        ),
        ("steam_partner_get_ownership_changes", obj! {}),
        (
            "steam_partner_get_app_price_info",
            obj! { "steamid" => steamid.as_str(), "appids" => steam_mcp::arr![partner_app] },
        ),
        ("steam_partner_get_deleted_steamids", obj! {}),
        (
            "steam_partner_get_app_betas",
            obj! { "appid" => partner_app },
        ),
        (
            "steam_partner_get_app_builds",
            obj! { "appid" => partner_app, "count" => 3 },
        ),
        (
            "steam_partner_get_app_depot_versions",
            obj! { "appid" => partner_app },
        ),
        (
            "steam_partner_get_players_banned",
            obj! { "appid" => partner_app },
        ),
        (
            "steam_partner_get_cheating_reports",
            obj! { "appid" => partner_app },
        ),
        (
            "steam_partner_get_server_list",
            obj! { "appid" => partner_app, "limit" => 3 },
        ),
        (
            "steam_partner_get_leaderboards",
            obj! { "appid" => partner_app },
        ),
        (
            "steam_partner_get_microtxn_report",
            obj! { "appid" => partner_app, "maxresults" => 5 },
        ),
        (
            "steam_partner_query_microtxn",
            obj! { "appid" => partner_app, "orderid" => "1" },
        ),
        (
            "steam_partner_get_item_definitions",
            obj! { "appid" => partner_app, "max_items" => 3 },
        ),
        (
            "steam_partner_get_inventory",
            obj! { "appid" => partner_app, "steamid" => steamid.as_str() },
        ),
        (
            "steam_partner_get_asset_prices",
            obj! { "appid" => partner_app },
        ),
        ("steam_partner_get_changed_sales_dates", obj! {}),
    ];
    if board > 0 {
        plan.push((
            "steam_partner_get_leaderboard_entries",
            obj! { "appid" => partner_app, "leaderboardid" => board, "rangeend" => 4 },
        ));
    }
    if let Some(d) = &date {
        plan.push((
            "steam_partner_get_detailed_sales",
            obj! { "date" => d.as_str(), "max_rows" => 5 },
        ));
        plan.push((
            "steam_partner_sales_summary",
            obj! { "date" => d.as_str(), "max_pages" => 2 },
        ));
    }

    let mut report = String::from(
        "# steam-mcp live verification\n\n| Tool | Result | Detail |\n| --- | --- | --- |\n",
    );
    let mut problems = Vec::new();
    let (mut ok, mut data_err, mut skipped) = (0, 0, 0);
    for (i, (name, args)) in plan.into_iter().enumerate() {
        let r = call(&core, i as i64 + 1, name, args);
        let (status, detail) = if let Some(e) = r.get("error") {
            // Unknown tool = credential not configured, so the tool is hidden.
            if e["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("Unknown tool"))
            {
                skipped += 1;
                ("skipped", "credential not configured".to_string())
            } else {
                problems.push(format!("{name}: protocol error {e}"));
                ("PROTOCOL ERROR", e.to_string())
            }
        } else if r["result"]["isError"].as_bool() == Some(true) {
            let text = r["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_string();
            if endpoint_problem(&text) {
                problems.push(format!("{name}: {text}"));
                ("ENDPOINT PROBLEM", text)
            } else {
                data_err += 1;
                ("data error", text)
            }
        } else {
            ok += 1;
            (
                "ok",
                r["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
            )
        };
        let detail = steam_mcp::util::truncate(&detail.replace('|', "\\|").replace('\n', " "), 180);
        let _ = writeln!(report, "| `{name}` | {status} | {detail} |");
        println!("{status:16} {name}: {detail}");
    }
    let _ = writeln!(
        report,
        "\n{ok} ok, {data_err} data-level errors, {skipped} skipped (no credential), {} endpoint problems.",
        problems.len()
    );
    let _ = writeln!(report, "\nUpstream: {}", core.steam.stats_json());
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/target/live-report.md");
    std::fs::write(path, &report).expect("write report");
    println!("\nReport written to {path}");
    assert!(
        problems.is_empty(),
        "endpoint-level problems:\n{}",
        problems.join("\n")
    );
}
