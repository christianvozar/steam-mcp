//! Multi-principal agent controls (ADR 0018): quotas and fairness, upstream
//! attribution, idempotent writes, pre-approved writes, dry runs, batches,
//! usage metering and the audit log.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use steam_mcp::json::Value;
use steam_mcp::mcp::context::Principal;
use steam_mcp::{arr, obj};

type CoreRef = std::sync::Arc<steam_mcp::mcp::server::Core>;

fn agent(id: &str, scope: &str) -> Principal {
    Principal {
        id: id.into(),
        scopes: vec![scope.into()],
        authenticated: true,
    }
}

fn call_as(core: &CoreRef, who: &Principal, name: &str, args: Value) -> Value {
    send_as(
        core,
        &request(
            1,
            "tools/call",
            obj! { "name" => name, "arguments" => args },
            obj! {},
        ),
        who.clone(),
    )
    .0
}

fn call_meta(core: &CoreRef, who: &Principal, name: &str, args: Value, meta: Value) -> Value {
    send_as(
        core,
        &request(
            1,
            "tools/call",
            obj! { "name" => name, "arguments" => args, "_meta" => meta },
            obj! {},
        ),
        who.clone(),
    )
    .0
}

fn text(r: &Value) -> String {
    r["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn posts_to(mock: &Mock, path: &str) -> usize {
    mock.posts
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p == path)
        .count()
}

#[test]
fn daily_quota_and_concurrency_cap_are_per_principal() {
    let mock = start_mock();
    let core = core_with(
        &mock,
        &[
            ("STEAM_MCP_PRINCIPAL_LIMITS", "bot-*=::1:3:"),
            ("STEAM_MCP_PRINCIPAL_QUEUE_MS", "50"),
        ],
    );
    let bot = agent("bot-1", "steam:public");
    for _ in 0..3 {
        assert_eq!(
            call_as(
                &core,
                &bot,
                "steam_get_current_players",
                obj! { "appid" => 570 }
            )["result"]["isError"]
                .as_bool(),
            Some(false)
        );
    }
    let r = call_as(
        &core,
        &bot,
        "steam_get_current_players",
        obj! { "appid" => 570 },
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-31029), "{r}");
    assert_eq!(
        r["error"]["data"]["reason"].as_str(),
        Some("daily_tool_calls")
    );
    assert_eq!(
        call_as(
            &core,
            &agent("alice", "steam:public"),
            "steam_get_current_players",
            obj! { "appid" => 570 }
        )["result"]["isError"]
            .as_bool(),
        Some(false)
    );

    // Concurrency: bot-2 may run one call at a time.
    mock.sales_delay_ms.store(400, Ordering::SeqCst);
    let bot2 = agent("bot-2", "steam:admin");
    let (c, b) = (core.clone(), bot2.clone());
    let slow = std::thread::spawn(move || {
        call_as(
            &c,
            &b,
            "steam_partner_get_detailed_sales",
            obj! { "date" => "2026/07/01" },
        )
    });
    std::thread::sleep(Duration::from_millis(150));
    let r = call_as(
        &core,
        &bot2,
        "steam_get_current_players",
        obj! { "appid" => 570 },
    );
    assert_eq!(
        r["error"]["data"]["reason"].as_str(),
        Some("concurrency"),
        "{r}"
    );
    assert_eq!(
        slow.join().unwrap()["result"]["isError"].as_bool(),
        Some(false)
    );
    assert_eq!(
        call_as(
            &core,
            &bot2,
            "steam_get_current_players",
            obj! { "appid" => 570 }
        )["result"]["isError"]
            .as_bool(),
        Some(false)
    );
}

#[test]
fn bursts_queue_instead_of_failing() {
    let mock = start_mock();
    mock.sales_delay_ms.store(100, Ordering::SeqCst);
    let core = core_with(&mock, &[("STEAM_MCP_PRINCIPAL_LIMITS", "fan-out=::2")]);
    let who = agent("fan-out", "steam:admin");
    let handles: Vec<_> = (0..6)
        .map(|i| {
            let (c, w) = (core.clone(), who.clone());
            std::thread::spawn(move || {
                call_as(
                    &c,
                    &w,
                    "steam_partner_get_detailed_sales",
                    obj! { "date" => format!("2026/07/0{}", i + 1) },
                )
            })
        })
        .collect();
    for h in handles {
        let r = h.join().unwrap();
        assert_eq!(
            r["result"]["isError"].as_bool(),
            Some(false),
            "a burst of 6 with a cap of 2 waits for slots: {r}"
        );
    }
}

#[test]
fn upstream_fetches_are_charged_to_the_caller() {
    let mock = start_mock();
    let core = core_with(&mock, &[("STEAM_MCP_PRINCIPAL_LIMITS", "crawler=::::2")]);
    let crawler = agent("crawler", "steam:public");
    assert_eq!(
        call_as(
            &core,
            &crawler,
            "steam_get_app_details",
            obj! { "appid" => 570 }
        )["result"]["isError"]
            .as_bool(),
        Some(false)
    );
    assert_eq!(
        call_as(
            &core,
            &crawler,
            "steam_get_current_players",
            obj! { "appid" => 570 }
        )["result"]["isError"]
            .as_bool(),
        Some(false)
    );
    let r = call_as(
        &core,
        &crawler,
        "steam_get_app_news",
        obj! { "appid" => 570 },
    );
    assert!(text(&r).contains("daily upstream budget"), "{r}");
    let r = call_as(
        &core,
        &crawler,
        "steam_get_app_details",
        obj! { "appid" => 570 },
    );
    assert_eq!(
        r["result"]["_meta"]["io.github.christianvozar/cache"]["source"].as_str(),
        Some("hit"),
        "cached data is still served"
    );
    assert_eq!(
        call_as(
            &core,
            &agent("other", "steam:public"),
            "steam_get_app_news",
            obj! { "appid" => 570 }
        )["result"]["isError"]
            .as_bool(),
        Some(false)
    );
    let u = call_as(&core, &crawler, "usage_stats", obj! {});
    assert_eq!(
        u["result"]["structuredContent"]["principals"][0]["upstreamCalls"].as_u64(),
        Some(2),
        "{u}"
    );
    assert!(
        core.metrics_text()
            .contains("steam_mcp_principal_upstream_calls_total{principal=\"crawler\"} 2")
    );
    assert_eq!(
        call_as(
            &core,
            &crawler,
            "usage_stats",
            obj! { "principal" => "other" }
        )["result"]["isError"]
            .as_bool(),
        Some(true)
    );
}

#[test]
fn policy_approves_unattended_writes_within_limits() {
    let mock = start_mock();
    let core = core_with(
        &mock,
        &[
            ("STEAM_MCP_ENABLE_WRITES", "true"),
            (
                "STEAM_MCP_WRITE_POLICY",
                "lb-bot|steam_partner_reset_leaderboard|77|1; release-bot|steam_partner_set_app_build_live|480",
            ),
        ],
    );
    let bot = agent("lb-bot", "steam:write");
    let r = call_as(
        &core,
        &bot,
        "steam_partner_reset_leaderboard",
        obj! { "appid" => 480, "leaderboardid" => 77 },
    );
    assert_eq!(r["result"]["isError"].as_bool(), Some(false), "{r}");
    assert!(
        r["result"]["structuredContent"]["approvedBy"]
            .as_str()
            .unwrap()
            .starts_with("write policy rule 1")
    );
    // Daily cap of 1 reached; a different board is not covered at all.
    assert_eq!(
        call_as(
            &core,
            &bot,
            "steam_partner_reset_leaderboard",
            obj! { "appid" => 480, "leaderboardid" => 77 }
        )["error"]["code"]
            .as_i64(),
        Some(-32021)
    );
    assert_eq!(
        call_as(
            &core,
            &bot,
            "steam_partner_reset_leaderboard",
            obj! { "appid" => 480, "leaderboardid" => 78 }
        )["error"]["code"]
            .as_i64(),
        Some(-32021)
    );
    assert_eq!(
        posts_to(&mock, "/ISteamLeaderboards/ResetLeaderboard/v1/"),
        1
    );

    // The build-live tool honours the policy too (target = app ID).
    let rel = agent("release-bot", "steam:write");
    let r = call_as(
        &core,
        &rel,
        "steam_partner_set_app_build_live",
        obj! { "appid" => 480, "buildid" => 101, "betakey" => "public" },
    );
    assert_eq!(
        r["result"]["structuredContent"]["approvedBy"]
            .as_str()
            .map(|s| s.contains("release-bot")),
        Some(true),
        "{r}"
    );
    assert_eq!(posts_to(&mock, "/ISteamApps/SetAppBuildLive/v2/"), 1);
    let r = call_as(
        &core,
        &rel,
        "steam_partner_set_app_build_live",
        obj! { "appid" => 481, "buildid" => 101, "betakey" => "public" },
    );
    assert_eq!(
        r["error"]["code"].as_i64(),
        Some(-32021),
        "another app is not pre-approved"
    );

    let r = call_as(
        &core,
        &agent("ops", "steam:admin"),
        "audit_log",
        obj! { "principal" => "*" },
    );
    let entries = r["result"]["structuredContent"]["entries"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(entries.len(), 2, "{r}");
    assert_eq!(
        entries[0]["tool"].as_str(),
        Some("steam_partner_set_app_build_live"),
        "newest first"
    );
}

#[test]
fn idempotent_replays_never_store_returned_secrets() {
    let mock = start_mock();
    let core = core_with(
        &mock,
        &[
            ("STEAM_MCP_ENABLE_WRITES", "true"),
            (
                "STEAM_MCP_WRITE_POLICY",
                "gs-bot|steam_create_game_server_account|730",
            ),
        ],
    );
    let bot = agent("gs-bot", "steam:write");
    let args = obj! { "appid" => 730, "memo" => "eu-9", "idempotency_key" => "deploy-17/gslt" };
    let first = call_as(
        &core,
        &bot,
        "steam_create_game_server_account",
        args.clone(),
    );
    assert_eq!(
        first["result"]["structuredContent"]["response"]["login_token"].as_str(),
        Some("SECRETGSLT0123456789"),
        "{first}"
    );
    let again = call_as(
        &core,
        &bot,
        "steam_create_game_server_account",
        args.clone(),
    );
    assert_eq!(
        again["result"]["_meta"]["io.github.christianvozar/idempotentReplay"].as_bool(),
        Some(true),
        "{again}"
    );
    assert!(
        !again.to_string().contains("SECRETGSLT"),
        "the replay must not contain the token: {again}"
    );
    assert_eq!(
        again["result"]["structuredContent"]["response"]["steamid"].as_str(),
        Some("85568392920040001")
    );
    assert_eq!(
        posts_to(&mock, "/IGameServersService/CreateAccount/v1/"),
        1,
        "the account was created once"
    );
    let mut other = args.clone();
    other.set("memo", "eu-10");
    other.as_object_mut().unwrap().remove("idempotency_key");
    let r = call_meta(
        &core,
        &bot,
        "steam_create_game_server_account",
        other,
        obj! { "io.github.christianvozar/idempotencyKey" => "deploy-17/gslt" },
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32602), "{r}");
}

#[test]
fn dry_runs_describe_the_request_without_acting() {
    let mock = start_mock();
    let core = core_with(
        &mock,
        &[
            ("STEAM_MCP_ENABLE_WRITES", "true"),
            (
                "STEAM_MCP_WRITE_POLICY",
                "release-bot|steam_partner_set_app_build_live|480",
            ),
        ],
    );
    let r = call_as(
        &core,
        &agent("release-bot", "steam:write"),
        "steam_partner_set_app_build_live",
        obj! { "appid" => 480, "buildid" => 101, "betakey" => "beta", "dry_run" => true },
    );
    let s = &r["result"]["structuredContent"];
    assert_eq!(s["dryRun"].as_bool(), Some(true), "{r}");
    assert_eq!(
        s["request"]["path"].as_str(),
        Some("/ISteamApps/SetAppBuildLive/v2/")
    );
    assert_eq!(s["request"]["host"].as_str(), Some("partner"));
    assert!(s["preApprovedBy"].as_str().unwrap().contains("release-bot"));
    assert!(!r.to_string().contains(PUB_KEY), "keys are never shown");
    let r = call_meta(
        &core,
        &agent("someone", "steam:write"),
        "steam_partner_reset_leaderboard",
        obj! { "appid" => 480, "leaderboardid" => 77 },
        obj! { "io.github.christianvozar/dryRun" => true },
    );
    assert!(text(&r).contains("needs confirmation"), "{r}");
    assert!(mock.posts.lock().unwrap().is_empty());
}

#[test]
fn batch_calls_run_reads_in_parallel_with_normal_checks() {
    let mock = start_mock();
    let core = core_with(&mock, &[("STEAM_MCP_ENABLE_WRITES", "true")]);
    let viewer = agent("dash", "steam:public");
    let calls = arr![
        obj! { "tool" => "steam_get_app_details", "arguments" => obj! { "appid" => 570 }, "id" => "details" },
        obj! { "tool" => "steam_get_current_players", "arguments" => obj! { "appid" => 570 } },
        obj! { "tool" => "steam_partner_list_apps" },
        obj! { "tool" => "steam_partner_reset_leaderboard", "arguments" => obj! { "appid" => 480, "leaderboardid" => 77 } },
        obj! { "tool" => "steam_get_app_details", "arguments" => obj! { "appid" => "x" } },
    ];
    let r = call_as(&core, &viewer, "batch_call", obj! { "calls" => calls });
    let s = &r["result"]["structuredContent"];
    assert_eq!(s["succeeded"].as_u64(), Some(2), "{r}");
    let res = s["results"].as_array().unwrap();
    assert_eq!(res[0]["id"].as_str(), Some("details"));
    assert_eq!(
        res[0]["structuredContent"]["summary"]["name"].as_str(),
        Some("Dota 2")
    );
    assert_eq!(res[2]["error"]["code"].as_i64(), Some(-31003));
    assert!(
        res[3]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("only read-only")
    );
    assert_eq!(res[4]["ok"].as_bool(), Some(false));
    assert!(mock.posts.lock().unwrap().is_empty());
}
