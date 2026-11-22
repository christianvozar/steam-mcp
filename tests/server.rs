//! End-to-end tests of the MCP dispatcher against a mock Steam API.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use steam_mcp::json::Value;
use steam_mcp::mcp::context::Principal;
use steam_mcp::obj;

#[test]
fn discover_advertises_capabilities_and_caching() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    let (r, _) = send(&core, &request(1, "server/discover", obj! {}, obj! {}));
    let res = &r["result"];
    assert_eq!(res["resultType"].as_str(), Some("complete"));
    assert_eq!(res["supportedVersions"][0].as_str(), Some("2026-07-28"));
    assert_eq!(
        res["capabilities"]["resources"]["subscribe"].as_bool(),
        Some(true)
    );
    assert!(res["capabilities"]["extensions"]["io.modelcontextprotocol/tasks"].is_object());
    assert_eq!(res["cacheScope"].as_str(), Some("public"));
    assert!(res["ttlMs"].as_u64().unwrap() > 0);
    assert_eq!(
        res["_meta"]["io.modelcontextprotocol/serverInfo"]["name"].as_str(),
        Some("steam-mcp")
    );
}

#[test]
fn tool_results_are_cached_and_carry_provenance() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    let first = call_tool(&core, "steam_get_app_details", obj! { "appid" => 570 });
    let res = &first["result"];
    assert_eq!(res["isError"].as_bool(), Some(false), "{first}");
    assert_eq!(
        res["structuredContent"]["summary"]["name"].as_str(),
        Some("Dota 2")
    );
    assert_eq!(
        res["structuredContent"]["summary"]["shortDescription"].as_str(),
        Some("A competitive game.")
    );
    assert_eq!(
        res["_meta"]["io.github.christianvozar/cache"]["source"].as_str(),
        Some("miss")
    );
    // The JSON is also present as text for backwards compatibility.
    assert!(
        res["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("Dota 2")
    );

    let second = call_tool(&core, "steam_get_app_details", obj! { "appid" => "570" });
    assert_eq!(
        second["result"]["_meta"]["io.github.christianvozar/cache"]["source"].as_str(),
        Some("hit")
    );
    assert_eq!(
        mock.hits("/api/appdetails"),
        1,
        "second call must be served from cache"
    );

    let forced = call_tool(
        &core,
        "steam_get_app_details",
        obj! { "appid" => 570, "force_refresh" => true },
    );
    assert_eq!(forced["result"]["isError"].as_bool(), Some(false));
    assert_eq!(mock.hits("/api/appdetails"), 2);
}

#[test]
fn concurrent_identical_requests_are_coalesced() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    let handles: Vec<_> = (0..12)
        .map(|_| {
            let core = core.clone();
            std::thread::spawn(move || {
                call_tool(&core, "steam_get_current_players", obj! { "appid" => 730 })
            })
        })
        .collect();
    let counts: Vec<u64> = handles
        .into_iter()
        .map(|h| {
            h.join().unwrap()["result"]["structuredContent"]["playerCount"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert!(
        counts.iter().all(|c| *c == counts[0]),
        "all callers see the same value: {counts:?}"
    );
    assert_eq!(
        mock.hits("/ISteamUserStats/GetNumberOfCurrentPlayers/v1/"),
        1
    );
}

#[test]
fn negative_results_and_validation_errors_are_tool_errors() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    let r = call_tool(&core, "steam_get_app_details", obj! { "appid" => 999 });
    assert_eq!(r["result"]["isError"].as_bool(), Some(true));
    call_tool(&core, "steam_get_app_details", obj! { "appid" => 999 });
    assert_eq!(mock.hits("/api/appdetails"), 1, "negative result is cached");

    let r = call_tool(&core, "steam_get_app_details", obj! { "appid" => -5 });
    assert_eq!(r["result"]["isError"].as_bool(), Some(true));
    assert!(
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Invalid arguments")
    );

    let r = call_tool(
        &core,
        "steam_get_app_details",
        obj! { "appid" => 570, "bogus" => 1 },
    );
    assert_eq!(r["result"]["isError"].as_bool(), Some(true));

    let (r, _) = send(
        &core,
        &request(2, "tools/call", obj! { "name" => "no_such_tool" }, obj! {}),
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32602));
    // The SteamID pattern is enforced by the schema validator.
    let r = call_tool(
        &core,
        "steam_get_player_summaries",
        obj! { "steamids" => steam_mcp::arr!["7656119796028793x"] },
    );
    assert!(
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("must match pattern"),
        "{r}"
    );
}

#[test]
fn protocol_errors() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    let (r, _) = send(
        &core,
        &parse(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#),
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32602));
    let (r, _) = send(
        &core,
        &parse(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2024-11-05","io.modelcontextprotocol/clientCapabilities":{}}}}"#,
        ),
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32022));
    assert_eq!(
        r["error"]["data"]["supported"][0].as_str(),
        Some("2026-07-28")
    );
    let (r, _) = send(
        &core,
        &request(1, "logging/setLevel", obj! { "level" => "debug" }, obj! {}),
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32601));
    let (r, _) = send(
        &core,
        &parse(r#"{"jsonrpc":"2.0","id":null,"method":"tools/list"}"#),
    );
    assert!(
        r.get("error").is_none() && r.is_null(),
        "id:null is treated as a notification: {r}"
    );
    let (r, _) = send(
        &core,
        &parse(r#"{"jsonrpc":"1.0","id":1,"method":"tools/list"}"#),
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32600));
    let (r, _) = send(
        &core,
        &parse(r#"[{"jsonrpc":"2.0","id":1,"method":"tools/list"}]"#),
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32600));
    let (r, _) = send(
        &core,
        &request(1, "tools/list", obj! { "cursor" => "garbage" }, obj! {}),
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32602));
}

#[test]
fn tools_list_is_deterministic_paginated_and_cacheable() {
    let mock = start_mock();
    let core = core_with(&mock, &[("STEAM_MCP_PAGE_SIZE", "20")]);
    let mut names = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = match &cursor {
            Some(c) => obj! { "cursor" => c.as_str() },
            None => obj! {},
        };
        let (r, _) = send(&core, &request(1, "tools/list", params, obj! {}));
        let res = &r["result"];
        assert!(res["ttlMs"].as_u64().is_some());
        assert_eq!(res["cacheScope"].as_str(), Some("public"));
        for t in res["tools"].as_array().unwrap() {
            names.push(t["name"].as_str().unwrap().to_string());
            assert_eq!(t["inputSchema"]["type"].as_str(), Some("object"));
        }
        match res["nextCursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    // Writes are hidden unless enabled.
    assert!(!names.contains(&"steam_partner_set_app_build_live".to_string()));
    assert!(names.len() >= 55, "{}", names.len());
    let (again, _) = send(&core, &request(1, "tools/list", obj! {}, obj! {}));
    let first_page: Vec<&str> = again["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        first_page,
        names[..20].iter().map(String::as_str).collect::<Vec<_>>()
    );
}

#[test]
fn keyless_server_hides_keyed_tools() {
    let mock = start_mock();
    let core = core_with(
        &mock,
        &[
            ("STEAM_API_KEY", ""),
            ("STEAM_PUBLISHER_KEY", ""),
            ("STEAM_FINANCIAL_KEY", ""),
        ],
    );
    let (r, _) = send(&core, &request(1, "tools/list", obj! {}, obj! {}));
    let names: Vec<&str> = r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"steam_get_app_details"));
    assert!(!names.contains(&"steam_get_player_summaries"));
    assert!(!names.iter().any(|n| n.starts_with("steam_partner_")));
}

#[test]
fn progress_and_logging_notifications() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    let msg = request(
        5,
        "tools/call",
        obj! { "name" => "steam_get_app_reviews", "arguments" => obj! { "appid" => 570, "max_pages" => 3 }, "_meta" => obj! { "progressToken" => "tok", "io.modelcontextprotocol/logLevel" => "debug" } },
        obj! {},
    );
    let (r, notes) = send(&core, &msg);
    assert_eq!(
        r["result"]["structuredContent"]["reviews"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    let progress: Vec<f64> = notes
        .iter()
        .filter(|n| n["method"].as_str() == Some("notifications/progress"))
        .map(|n| n["params"]["progress"].as_f64().unwrap())
        .collect();
    assert_eq!(progress, vec![1.0, 2.0, 3.0]);
    assert!(
        notes
            .iter()
            .filter(|n| n["method"].as_str() == Some("notifications/progress"))
            .all(|n| n["params"]["progressToken"].as_str() == Some("tok"))
    );
    assert!(
        notes
            .iter()
            .any(|n| n["method"].as_str() == Some("notifications/message"))
    );

    // Without logLevel, no log notifications may be sent.
    let msg = request(
        6,
        "tools/call",
        obj! { "name" => "steam_get_app_news", "arguments" => obj! { "appid" => 570 } },
        obj! {},
    );
    let (_, notes) = send(&core, &msg);
    assert!(notes.is_empty());
}

#[test]
fn partner_tools_and_scopes() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    let r = call_tool(&core, "steam_partner_list_apps", obj! {});
    assert_eq!(
        r["result"]["structuredContent"]["count"].as_u64(),
        Some(2),
        "{r}"
    );
    let r = call_tool(
        &core,
        "steam_partner_get_app_builds",
        obj! { "appid" => 480 },
    );
    assert_eq!(
        r["result"]["structuredContent"]["builds"][0]["BuildID"].as_u64(),
        Some(101),
        "newest first"
    );

    let public_only = Principal {
        id: "viewer".into(),
        scopes: vec!["steam:public".into()],
        authenticated: true,
    };
    let (r, _) = send_as(
        &core,
        &request(1, "tools/list", obj! {}, obj! {}),
        public_only.clone(),
    );
    let names: Vec<&str> = r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(!names.iter().any(|n| n.starts_with("steam_partner_")));
    let (r, _) = send_as(
        &core,
        &request(
            2,
            "tools/call",
            obj! { "name" => "steam_partner_list_apps" },
            obj! {},
        ),
        public_only,
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-31003));
    assert_eq!(
        r["error"]["data"]["requiredScope"].as_str(),
        Some("steam:partner")
    );
}

#[test]
fn write_tool_requires_confirmation_via_mrtr() {
    let mock = start_mock();
    let core = core_with(&mock, &[("STEAM_MCP_ENABLE_WRITES", "true")]);
    let args = obj! { "appid" => 480, "buildid" => 101, "betakey" => "public" };
    // Without elicitation support the server must refuse with -32021.
    let (r, _) = send(
        &core,
        &request(
            1,
            "tools/call",
            obj! { "name" => "steam_partner_set_app_build_live", "arguments" => args.clone() },
            obj! {},
        ),
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32021));

    let caps = obj! { "elicitation" => obj! { "form" => obj! {} } };
    let (r, _) = send(
        &core,
        &request(
            2,
            "tools/call",
            obj! { "name" => "steam_partner_set_app_build_live", "arguments" => args.clone() },
            caps.clone(),
        ),
    );
    let res = &r["result"];
    assert_eq!(res["resultType"].as_str(), Some("input_required"), "{r}");
    let elicit = &res["inputRequests"]["confirm"];
    assert_eq!(elicit["method"].as_str(), Some("elicitation/create"));
    assert!(
        elicit["params"]["message"]
            .as_str()
            .unwrap()
            .contains("currently runs build 100")
    );
    let state = res["requestState"].as_str().unwrap().to_string();
    assert!(mock.posts.lock().unwrap().is_empty());

    // Tampered state is rejected.
    let mut bad = state.clone();
    bad.replace_range(10..11, if &state[10..11] == "A" { "B" } else { "A" });
    let accept = obj! { "confirm" => obj! { "action" => "accept", "content" => obj! { "confirm" => true, "appid_confirmation" => 480 } } };
    let (r, _) = send(
        &core,
        &request(
            3,
            "tools/call",
            obj! { "name" => "steam_partner_set_app_build_live", "arguments" => args.clone(), "inputResponses" => accept.clone(), "requestState" => bad },
            caps.clone(),
        ),
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32602));

    // State bound to other arguments is rejected.
    let other = obj! { "appid" => 480, "buildid" => 999, "betakey" => "public" };
    let (r, _) = send(
        &core,
        &request(
            4,
            "tools/call",
            obj! { "name" => "steam_partner_set_app_build_live", "arguments" => other, "inputResponses" => accept.clone(), "requestState" => state.as_str() },
            caps.clone(),
        ),
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32602));

    // Declined → no change.
    let decline = obj! { "confirm" => obj! { "action" => "decline" } };
    let (r, _) = send(
        &core,
        &request(
            5,
            "tools/call",
            obj! { "name" => "steam_partner_set_app_build_live", "arguments" => args.clone(), "inputResponses" => decline, "requestState" => state.as_str() },
            caps.clone(),
        ),
    );
    assert_eq!(r["result"]["isError"].as_bool(), Some(true));
    assert!(mock.posts.lock().unwrap().is_empty());

    // Accepted with the right app id → performs the POST.
    let (r, _) = send(
        &core,
        &request(
            6,
            "tools/call",
            obj! { "name" => "steam_partner_set_app_build_live", "arguments" => args, "inputResponses" => accept, "requestState" => state.as_str() },
            caps,
        ),
    );
    assert_eq!(r["result"]["isError"].as_bool(), Some(false), "{r}");
    let posts = mock.posts.lock().unwrap();
    assert_eq!(posts.len(), 1);
    assert!(posts[0].1.contains("buildid=101") && posts[0].1.contains("key=TESTPUBLISHERKEY"));
}

#[test]
fn sampling_mrtr_and_fallback() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    let args = obj! { "appid" => 570, "num_reviews" => 10 };
    // Fallback (no sampling capability): term frequency.
    let r = call_tool(&core, "steam_summarize_reviews", args.clone());
    assert_eq!(
        r["result"]["structuredContent"]["method"].as_str(),
        Some("term-frequency"),
        "{r}"
    );

    let caps = obj! { "sampling" => obj! {} };
    let (r, _) = send(
        &core,
        &request(
            2,
            "tools/call",
            obj! { "name" => "steam_summarize_reviews", "arguments" => args.clone() },
            caps.clone(),
        ),
    );
    let res = &r["result"];
    assert_eq!(res["resultType"].as_str(), Some("input_required"));
    assert_eq!(
        res["inputRequests"]["summary"]["method"].as_str(),
        Some("sampling/createMessage")
    );
    let state = res["requestState"].as_str().unwrap();
    let sampled = obj! { "summary" => obj! { "role" => "assistant", "content" => obj! { "type" => "text", "text" => "Players love the gameplay." }, "model" => "test-model", "stopReason" => "endTurn" } };
    let (r, _) = send(
        &core,
        &request(
            3,
            "tools/call",
            obj! { "name" => "steam_summarize_reviews", "arguments" => args, "inputResponses" => sampled, "requestState" => state },
            caps,
        ),
    );
    assert_eq!(
        r["result"]["structuredContent"]["summary"].as_str(),
        Some("Players love the gameplay.")
    );
    assert_eq!(
        r["result"]["structuredContent"]["model"].as_str(),
        Some("test-model")
    );
}

#[test]
fn roots_mrtr_export() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    let dir = std::env::temp_dir().join(format!(
        "steam-mcp-export-{}",
        steam_mcp::util::crypto::random_id(4)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let caps = obj! { "roots" => obj! {} };
    let args = obj! { "tool" => "steam_get_global_achievement_percentages", "arguments" => obj! { "appid" => 570 }, "filename" => "ach.csv" };
    let msg = |id: i64, extra: Value| {
        let mut p = obj! { "name" => "export_report", "arguments" => args.clone() };
        if let Value::Object(m) = extra {
            for (k, v) in m {
                p.set(&k, v);
            }
        }
        let mut r = request(id, "tools/call", p, caps.clone());
        // export_report is stdio-only; the harness uses the internal transport.
        r.set("jsonrpc", "2.0");
        r
    };
    let (r, _) = send(&core, &msg(1, obj! {}));
    assert_eq!(
        r["result"]["inputRequests"]["roots"]["method"].as_str(),
        Some("roots/list"),
        "{r}"
    );
    let state = r["result"]["requestState"].as_str().unwrap().to_string();
    let roots = obj! { "roots" => obj! { "roots" => steam_mcp::arr![obj! { "uri" => format!("file://{}", dir.display()), "name" => "exports" }] } };
    let (r, _) = send(
        &core,
        &msg(
            2,
            obj! { "inputResponses" => roots, "requestState" => state.as_str() },
        ),
    );
    assert_eq!(r["result"]["isError"].as_bool(), Some(false), "{r}");
    let written = std::fs::read_to_string(dir.join("ach.csv")).unwrap();
    assert!(written.starts_with("name,percent\n"), "{written}");
    assert!(written.contains("HARD,0.4"));
    assert_eq!(
        r["result"]["content"][1]["type"].as_str(),
        Some("resource_link")
    );
    // Never overwrites.
    let roots = obj! { "roots" => obj! { "roots" => steam_mcp::arr![obj! { "uri" => format!("file://{}", dir.display()) }] } };
    let (r, _) = send(
        &core,
        &msg(
            3,
            obj! { "inputResponses" => roots, "requestState" => state.as_str() },
        ),
    );
    assert_eq!(r["result"]["isError"].as_bool(), Some(true));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn long_tool_becomes_task() {
    let mock = start_mock();
    mock.sales_delay_ms.store(150, Ordering::SeqCst);
    let core = core_with(&mock, &[("STEAM_MCP_TASK_SYNC_WINDOW_MS", "50")]);
    let caps = obj! { "extensions" => obj! { "io.modelcontextprotocol/tasks" => obj! {} } };
    let args = obj! { "date" => "2026/07/01", "group_by" => steam_mcp::arr!["primary_appid"] };
    let (r, _) = send(
        &core,
        &request(
            1,
            "tools/call",
            obj! { "name" => "steam_partner_sales_summary", "arguments" => args.clone() },
            caps.clone(),
        ),
    );
    let res = &r["result"];
    assert_eq!(res["resultType"].as_str(), Some("task"), "{r}");
    let task_id = res["taskId"].as_str().unwrap().to_string();
    assert_eq!(res["status"].as_str(), Some("working"));
    assert!(res["pollIntervalMs"].as_u64().is_some());

    // Other principals cannot see the task; non-declaring clients get -32021.
    let other = Principal {
        id: "mallory".into(),
        scopes: vec!["steam:admin".into()],
        authenticated: true,
    };
    let (r, _) = send_as(
        &core,
        &request(
            2,
            "tasks/get",
            obj! { "taskId" => task_id.as_str() },
            caps.clone(),
        ),
        other,
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32602));
    let (r, _) = send(
        &core,
        &request(
            3,
            "tasks/get",
            obj! { "taskId" => task_id.as_str() },
            obj! {},
        ),
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32021));

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let final_task = loop {
        let (r, _) = send(
            &core,
            &request(
                4,
                "tasks/get",
                obj! { "taskId" => task_id.as_str() },
                caps.clone(),
            ),
        );
        assert_eq!(r["result"]["resultType"].as_str(), Some("complete"));
        if r["result"]["status"].as_str() == Some("completed") {
            break r;
        }
        assert!(std::time::Instant::now() < deadline, "task did not finish");
        std::thread::sleep(Duration::from_millis(50));
    };
    let summary = &final_task["result"]["result"]["structuredContent"];
    assert_eq!(summary["rows"].as_u64(), Some(3));
    assert_eq!(summary["top"][0]["primary_appid"].as_str(), Some("480"));
    assert_eq!(
        summary["top"][0]["primary_appid_name"].as_str(),
        Some("Spacewar")
    );
    assert_eq!(summary["top"][0]["net_sales_usd"].as_f64(), Some(15.75));
    assert_eq!(
        mock.hits("/IPartnerFinancialsService/GetDetailedSales/v001/"),
        3
    );

    // Without the extension, the same call is answered synchronously (and from cache).
    let r = call_tool(&core, "steam_partner_sales_summary", args);
    assert_eq!(r["result"]["resultType"].as_str(), Some("complete"));
    assert_eq!(
        mock.hits("/IPartnerFinancialsService/GetDetailedSales/v001/"),
        3
    );

    let (r, _) = send(
        &core,
        &request(
            5,
            "tasks/cancel",
            obj! { "taskId" => task_id.as_str() },
            caps,
        ),
    );
    assert_eq!(r["result"]["resultType"].as_str(), Some("complete"));
}

#[test]
fn resources_read_list_and_templates() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    let (r, _) = send(&core, &request(1, "resources/list", obj! {}, obj! {}));
    let uris: Vec<&str> = r["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["uri"].as_str().unwrap())
        .collect();
    assert!(uris.contains(&"steam://partner/apps"));
    assert!(
        uris.contains(&"steam://app/480/overview"),
        "partner apps are listed: {uris:?}"
    );
    assert!(r["result"]["ttlMs"].as_u64().is_some());

    let (r, _) = send(
        &core,
        &request(
            2,
            "resources/read",
            obj! { "uri" => "steam://app/570/players" },
            obj! {},
        ),
    );
    let res = &r["result"];
    assert_eq!(
        res["contents"][0]["mimeType"].as_str(),
        Some("application/json")
    );
    assert!(
        res["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("playerCount")
    );
    assert!(res["ttlMs"].as_u64().unwrap() <= 60_000);
    assert_eq!(res["cacheScope"].as_str(), Some("public"));

    let (r, _) = send(
        &core,
        &request(
            3,
            "resources/read",
            obj! { "uri" => "steam://app/999/details" },
            obj! {},
        ),
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32602));
    assert_eq!(
        r["error"]["data"]["uri"].as_str(),
        Some("steam://app/999/details")
    );
    let (r, _) = send(
        &core,
        &request(
            4,
            "resources/read",
            obj! { "uri" => "file:///etc/passwd" },
            obj! {},
        ),
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32602));

    let (r, _) = send(
        &core,
        &request(5, "resources/templates/list", obj! {}, obj! {}),
    );
    assert!(
        r["result"]["resourceTemplates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["uriTemplate"].as_str() == Some("steam://app/{appid}/details"))
    );
}

#[test]
fn prompts_embed_resources_and_elicit_missing_args() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    let (r, _) = send(&core, &request(1, "prompts/list", obj! {}, obj! {}));
    assert!(r["result"]["prompts"].as_array().unwrap().len() >= 8);

    let (r, _) = send(
        &core,
        &request(
            2,
            "prompts/get",
            obj! { "name" => "game_health_report", "arguments" => obj! { "appid" => "570" } },
            obj! {},
        ),
    );
    let msgs = r["result"]["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["content"]["type"].as_str(), Some("resource"));
    assert_eq!(
        msgs[0]["content"]["resource"]["uri"].as_str(),
        Some("steam://app/570/overview")
    );
    assert!(
        msgs.last().unwrap()["content"]["text"]
            .as_str()
            .unwrap()
            .contains("570")
    );

    // Missing argument without elicitation → -32602.
    let (r, _) = send(
        &core,
        &request(
            3,
            "prompts/get",
            obj! { "name" => "game_health_report" },
            obj! {},
        ),
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32602));
    // With elicitation → MRTR, then retry succeeds.
    let caps = obj! { "elicitation" => obj! {} };
    let (r, _) = send(
        &core,
        &request(
            4,
            "prompts/get",
            obj! { "name" => "game_health_report" },
            caps.clone(),
        ),
    );
    assert_eq!(r["result"]["resultType"].as_str(), Some("input_required"));
    let state = r["result"]["requestState"].as_str().unwrap();
    let answer = obj! { "prompt_arguments" => obj! { "action" => "accept", "content" => obj! { "appid" => "730" } } };
    let (r, _) = send(
        &core,
        &request(
            5,
            "prompts/get",
            obj! { "name" => "game_health_report", "inputResponses" => answer, "requestState" => state },
            caps,
        ),
    );
    assert_eq!(
        r["result"]["messages"][0]["content"]["resource"]["uri"].as_str(),
        Some("steam://app/730/overview"),
        "{r}"
    );
}

#[test]
fn completion_uses_partner_apps_and_catalog() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    // Partner apps complete immediately.
    let (r, _) = send(
        &core,
        &request(
            1,
            "completion/complete",
            obj! { "ref" => obj! { "type" => "ref/prompt", "name" => "release_readiness_check" }, "argument" => obj! { "name" => "appid", "value" => "48" } },
            obj! {},
        ),
    );
    let vals: Vec<&str> = r["result"]["completion"]["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(vals, vec!["480", "481"]);
    // Warm the catalog, then names complete to app IDs.
    let r = call_tool(&core, "steam_search_apps", obj! { "query" => "dota" });
    assert_eq!(
        r["result"]["structuredContent"]["results"][0]["appid"].as_u64(),
        Some(570)
    );
    assert_eq!(
        mock.hits("/IStoreService/GetAppList/v1/"),
        2,
        "catalog paged once"
    );
    let (r, _) = send(
        &core,
        &request(
            2,
            "completion/complete",
            obj! { "ref" => obj! { "type" => "ref/resource", "uri" => "steam://app/{appid}/details" }, "argument" => obj! { "name" => "appid", "value" => "counter" } },
            obj! {},
        ),
    );
    let vals: Vec<&str> = r["result"]["completion"]["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(vals, vec!["10", "730"]);
    let (r, _) = send(
        &core,
        &request(
            3,
            "completion/complete",
            obj! { "ref" => obj! { "type" => "ref/prompt", "name" => "nope" }, "argument" => obj! { "name" => "appid", "value" => "" } },
            obj! {},
        ),
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32602));
    let (r, _) = send(
        &core,
        &request(
            4,
            "completion/complete",
            obj! { "ref" => obj! { "type" => "ref/prompt", "name" => "partner_sales_report" }, "argument" => obj! { "name" => "date", "value" => "2026/07" } },
            obj! {},
        ),
    );
    assert_eq!(
        r["result"]["completion"]["values"][0].as_str(),
        Some("2026/07/02")
    );
    // context.arguments excludes the app under analysis and apps already listed.
    let (r, _) = send(
        &core,
        &request(
            5,
            "completion/complete",
            obj! { "ref" => obj! { "type" => "ref/prompt", "name" => "competitor_comparison" }, "argument" => obj! { "name" => "competitors", "value" => "10,counter" }, "context" => obj! { "arguments" => obj! { "appid" => "570" } } },
            obj! {},
        ),
    );
    let vals: Vec<&str> = r["result"]["completion"]["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(vals, vec!["10,730"], "{r}");
}

#[test]
fn subscriptions_ack_notify_and_close() {
    let mock = start_mock();
    let core = core_with(&mock, &[("STEAM_MCP_TTL_REALTIME", "0,0")]);
    let cap = std::sync::Arc::new(Capture::default());
    let msg = request(
        42,
        "subscriptions/listen",
        obj! { "notifications" => obj! { "toolsListChanged" => true, "resourceSubscriptions" => steam_mcp::arr!["steam://app/570/players", "steam://nope"] } },
        obj! {},
    );
    let reply = core.handle_message(
        &msg,
        Principal::local(),
        steam_mcp::mcp::context::Transport::Internal,
        cap.clone(),
        steam_mcp::util::CancelToken::new(),
    );
    let sub = match reply {
        steam_mcp::mcp::server::Reply::Subscription(s) => s,
        _ => panic!("expected subscription"),
    };
    let first = &cap.messages()[0];
    assert_eq!(
        first["method"].as_str(),
        Some("notifications/subscriptions/acknowledged")
    );
    assert_eq!(
        first["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"].as_i64(),
        Some(42)
    );
    assert_eq!(
        first["params"]["notifications"]["resourceSubscriptions"],
        steam_mcp::arr!["steam://app/570/players"],
        "unknown URIs are omitted"
    );

    // The player count changes upstream on every fetch (TTL 0 here), so a
    // watcher tick detects a resource update.
    core.watch_tick();
    let updated: Vec<Value> = cap
        .messages()
        .into_iter()
        .filter(|m| m["method"].as_str() == Some("notifications/resources/updated"))
        .collect();
    assert_eq!(updated.len(), 1, "{:?}", cap.messages());
    assert_eq!(
        updated[0]["params"]["uri"].as_str(),
        Some("steam://app/570/players")
    );
    assert_eq!(
        updated[0]["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"].as_i64(),
        Some(42)
    );

    // Task notifications require the extension.
    let (r, _) = send(
        &core,
        &request(
            43,
            "subscriptions/listen",
            obj! { "notifications" => obj! { "taskIds" => steam_mcp::arr!["x"] } },
            obj! {},
        ),
    );
    assert_eq!(r["error"]["code"].as_i64(), Some(-32021));

    // Client cancellation ends the subscription.
    send(
        &core,
        &parse(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":42}}"#),
    );
    assert!(sub.is_closed());
    assert!(core.hub.is_empty());
}

#[test]
fn graceful_shutdown_completes_subscriptions_and_persists_cache() {
    let mock = start_mock();
    let path = std::env::temp_dir().join(format!(
        "steam-mcp-cache-{}.jsonl",
        steam_mcp::util::crypto::random_id(4)
    ));
    let core = core_with(&mock, &[("STEAM_MCP_CACHE_FILE", path.to_str().unwrap())]);
    call_tool(&core, "steam_get_app_details", obj! { "appid" => 570 });
    let cap = std::sync::Arc::new(Capture::default());
    let msg = request(
        9,
        "subscriptions/listen",
        obj! { "notifications" => obj! { "promptsListChanged" => true } },
        obj! {},
    );
    core.handle_message(
        &msg,
        Principal::local(),
        steam_mcp::mcp::context::Transport::Internal,
        cap.clone(),
        steam_mcp::util::CancelToken::new(),
    );
    core.shutdown();
    let last = cap.messages().last().cloned().unwrap();
    assert_eq!(last["id"].as_i64(), Some(9));
    assert_eq!(last["result"]["resultType"].as_str(), Some("complete"));
    assert_eq!(
        last["result"]["_meta"]["io.modelcontextprotocol/subscriptionId"].as_i64(),
        Some(9)
    );

    // A new server starts warm from the persisted cache.
    let core2 = core_with(&mock, &[("STEAM_MCP_CACHE_FILE", path.to_str().unwrap())]);
    let r = call_tool(&core2, "steam_get_app_details", obj! { "appid" => 570 });
    assert_eq!(
        r["result"]["_meta"]["io.github.christianvozar/cache"]["source"].as_str(),
        Some("hit")
    );
    assert_eq!(mock.hits("/api/appdetails"), 1);
    let _ = std::fs::remove_file(path);
}

#[test]
fn upstream_failures_serve_stale_data() {
    let mock = start_mock();
    let core = core_with(&mock, &[("STEAM_MCP_TTL_REALTIME", "0,0")]);
    let r = call_tool(&core, "steam_get_current_players", obj! { "appid" => 570 });
    assert_eq!(r["result"]["isError"].as_bool(), Some(false));
    // Point the API at a dead port: the expired value is served instead of an error.
    let mut cfg = config(&mock, &[("STEAM_MCP_TTL_REALTIME", "0,0")]);
    cfg.api_base = "http://127.0.0.1:9".into();
    core.config.replace(cfg);
    let r = call_tool(&core, "steam_get_current_players", obj! { "appid" => 570 });
    assert_eq!(r["result"]["isError"].as_bool(), Some(false), "{r}");
    assert_eq!(
        r["result"]["_meta"]["io.github.christianvozar/cache"]["source"].as_str(),
        Some("stale-on-error")
    );
    // Unknown data with upstream down is an actionable tool error.
    let r = call_tool(&core, "steam_get_current_players", obj! { "appid" => 1 });
    assert_eq!(r["result"]["isError"].as_bool(), Some(true));
}

#[test]
fn generic_passthrough_is_read_only() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    let r = call_tool(
        &core,
        "steam_api_call",
        obj! { "interface" => "ISteamNews", "method" => "GetNewsForApp", "version" => 2, "params" => obj! { "appid" => 570 } },
    );
    assert_eq!(r["result"]["isError"].as_bool(), Some(false), "{r}");
    let r = call_tool(
        &core,
        "steam_api_call",
        obj! { "interface" => "ISteamApps", "method" => "SetAppBuildLive", "version" => 2 },
    );
    assert_eq!(r["result"]["isError"].as_bool(), Some(true));
    let r = call_tool(
        &core,
        "steam_api_call",
        obj! { "interface" => "../etc", "method" => "x" },
    );
    assert_eq!(r["result"]["isError"].as_bool(), Some(true));
}

#[test]
fn cache_admin_tools() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    call_tool(&core, "steam_get_app_details", obj! { "appid" => 570 });
    call_tool(&core, "steam_get_current_players", obj! { "appid" => 570 });
    let r = call_tool(&core, "cache_stats", obj! {});
    assert_eq!(
        r["result"]["structuredContent"]["cache"]["entries"].as_u64(),
        Some(2)
    );
    let r = call_tool(
        &core,
        "cache_invalidate",
        obj! { "scope" => "app", "appid" => 570 },
    );
    assert_eq!(
        r["result"]["structuredContent"]["removed"].as_u64(),
        Some(2)
    );
    let r = call_tool(&core, "cache_stats", obj! {});
    assert_eq!(
        r["result"]["structuredContent"]["cache"]["entries"].as_u64(),
        Some(0)
    );
}

#[test]
fn url_elicitation_hands_off_to_steamworks() {
    let mock = start_mock();
    let core = core_with(&mock, &[("STEAM_MCP_ENABLE_WRITES", "true")]);
    let caps = obj! { "elicitation" => obj! { "url" => obj! {} } };
    let args = obj! { "appid" => 480, "buildid" => 101, "betakey" => "public" };
    let (r, _) = send(
        &core,
        &request(
            1,
            "tools/call",
            obj! { "name" => "steam_partner_set_app_build_live", "arguments" => args.clone() },
            caps.clone(),
        ),
    );
    let req = &r["result"]["inputRequests"]["steamworks"];
    assert_eq!(req["params"]["mode"].as_str(), Some("url"), "{r}");
    assert_eq!(
        req["params"]["url"].as_str(),
        Some("https://partner.steamgames.com/apps/builds/480")
    );
    assert!(
        req["params"].get("elicitationId").is_none(),
        "removed in 2026-07-28"
    );
    let state = r["result"]["requestState"].as_str().unwrap();
    let accept = obj! { "steamworks" => obj! { "action" => "accept" } };
    let (r, _) = send(
        &core,
        &request(
            2,
            "tools/call",
            obj! { "name" => "steam_partner_set_app_build_live", "arguments" => args, "inputResponses" => accept, "requestState" => state },
            caps,
        ),
    );
    assert_eq!(
        r["result"]["structuredContent"]["apiChangeMade"].as_bool(),
        Some(false),
        "{r}"
    );
    assert!(
        mock.posts.lock().unwrap().is_empty(),
        "no write is made by the server"
    );
}

#[test]
fn listings_carry_icons() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    for (method, key) in [
        ("tools/list", "tools"),
        ("resources/list", "resources"),
        ("resources/templates/list", "resourceTemplates"),
        ("prompts/list", "prompts"),
    ] {
        let (r, _) = send(&core, &request(1, method, obj! {}, obj! {}));
        let items = r["result"][key].as_array().unwrap();
        assert!(!items.is_empty());
        for it in items {
            let src = it["icons"][0]["src"]
                .as_str()
                .unwrap_or_else(|| panic!("{method} item without icon: {it}"));
            assert!(src.starts_with("data:image/svg+xml;base64,"));
        }
    }
}

#[test]
fn config_reload_notifies_list_changes() {
    let mock = start_mock();
    let dir = std::env::temp_dir().join(format!(
        "steam-mcp-reload-{}",
        steam_mcp::util::crypto::random_id(4)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("steam-mcp.conf");
    let base = format!(
        "STEAM_API_KEY={KEY}\nSTEAM_PUBLISHER_KEY={PUB_KEY}\nSTEAM_MCP_API_BASE={b}\nSTEAM_MCP_PARTNER_BASE={b}\nSTEAM_MCP_STORE_BASE={b}\nSTEAM_MCP_USE_PROXY_ENV=false\nSTEAM_MCP_LOG=error\n",
        b = mock.base
    );
    std::fs::write(&file, &base).unwrap();
    let sources = steam_mcp::config::Sources {
        file: Some(file.clone()),
        ..Default::default()
    };
    let cfg = steam_mcp::config::Config::load(&sources).unwrap();
    let core =
        steam_mcp::mcp::server::Core::new(steam_mcp::config::ConfigHandle::new(cfg, sources))
            .unwrap();
    core.watch_tick(); // record config mtime and list hashes

    let cap = std::sync::Arc::new(Capture::default());
    let msg = request(
        5,
        "subscriptions/listen",
        obj! { "notifications" => obj! { "toolsListChanged" => true, "promptsListChanged" => true } },
        obj! {},
    );
    core.handle_message(
        &msg,
        Principal::local(),
        steam_mcp::mcp::context::Transport::Internal,
        cap.clone(),
        steam_mcp::util::CancelToken::new(),
    );

    // Disabling the overview tool hides it and the prompt that embeds it.
    std::thread::sleep(Duration::from_millis(1100)); // ensure a new mtime
    std::fs::write(
        &file,
        format!("{base}STEAM_MCP_DISABLED_TOOLS=steam_get_app_overview\n"),
    )
    .unwrap();
    core.watch_tick();
    let methods: Vec<String> = cap
        .messages()
        .iter()
        .filter_map(|m| m["method"].as_str().map(String::from))
        .collect();
    assert!(
        methods.contains(&"notifications/tools/list_changed".to_string()),
        "{methods:?}"
    );
    assert!(
        methods.contains(&"notifications/prompts/list_changed".to_string()),
        "{methods:?}"
    );
    let (r, _) = send(&core, &request(6, "prompts/list", obj! {}, obj! {}));
    assert!(
        !r["result"]["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"].as_str() == Some("game_health_report"))
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn storefront_redirects_are_reported_not_parsed() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    // Full review text has no alternative source, so the redirect is reported.
    let r = call_tool(&core, "steam_get_app_reviews", obj! { "appid" => 9 });
    assert_eq!(r["result"]["isError"].as_bool(), Some(true));
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("redirected"), "{text}");
}

#[test]
fn keyless_search_uses_store_suggestions() {
    let mock = start_mock();
    let core = core_with(&mock, &[("STEAM_API_KEY", ""), ("STEAM_PUBLISHER_KEY", "")]);
    let r = call_tool(&core, "steam_search_apps", obj! { "query" => "dota" });
    let res = &r["result"]["structuredContent"];
    assert_eq!(
        res["source"].as_str(),
        Some("IStoreQueryService/SearchSuggestions"),
        "{r}"
    );
    assert_eq!(res["results"][0]["appid"].as_u64(), Some(570));
    assert_eq!(
        mock.hits("/IStoreService/GetAppList/v1/"),
        0,
        "no keyed catalog call without a key"
    );
}

#[test]
fn store_tools_shape_data_and_resolve_names() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    let r = call_tool(
        &core,
        "steam_get_store_items",
        obj! { "appids" => steam_mcp::arr![620, 999] },
    );
    let items = &r["result"]["structuredContent"]["items"];
    assert_eq!(items[0]["tags"], steam_mcp::arr!["Indie", "Action"], "{r}");
    assert_eq!(
        items[0]["platforms"]["steamDeck"].as_str(),
        Some("playable")
    );
    assert_eq!(items[0]["reviews"]["label"].as_str(), Some("Very Positive"));
    assert!(items[1]["error"].is_string());

    let r = call_tool(
        &core,
        "steam_get_top_concurrent_games",
        obj! { "limit" => 1 },
    );
    assert_eq!(
        r["result"]["structuredContent"]["ranks"][0]["name"].as_str(),
        Some("Name 730"),
        "{r}"
    );
    let r = call_tool(&core, "steam_get_most_played_games", obj! {});
    assert_eq!(
        r["result"]["structuredContent"]["ranks"][0]["name"].as_str(),
        Some("Name 730")
    );
    // Names are cached: a second chart call does not refetch them.
    call_tool(
        &core,
        "steam_get_most_played_games",
        obj! { "force_refresh" => false },
    );
    assert_eq!(
        mock.hits("/ICommunityService/GetApps/v1/"),
        2,
        "one lookup per distinct id set"
    );

    let r = call_tool(&core, "steam_get_gc_version", obj! { "appid" => 440 });
    assert_eq!(
        r["result"]["structuredContent"]["active_version"].as_u64(),
        Some(11191711)
    );
    let r = call_tool(&core, "steam_get_gc_version", obj! { "appid" => 620 });
    assert_eq!(r["result"]["isError"].as_bool(), Some(true));
}

#[test]
fn review_summary_falls_back_when_storefront_redirects() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    // /appreviews/9 redirects (like the live storefront for some networks).
    let r = call_tool(&core, "steam_get_review_summary", obj! { "appid" => 9 });
    let s = &r["result"]["structuredContent"];
    assert_eq!(
        s["source"].as_str(),
        Some("IStoreBrowseService/GetItems"),
        "{r}"
    );
    assert_eq!(s["reviewScoreDescription"].as_str(), Some("Very Positive"));
    assert_eq!(s["totalPositive"].as_u64(), Some(180));
}

#[test]
fn extended_read_tools() {
    let mock = start_mock();
    let core = core_with(&mock, &[]);
    let r = call_tool(
        &core,
        "steam_get_profile_items",
        obj! { "steamid" => "76561197960287930" },
    );
    let text = r["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(text.contains("profile_background: Night Sky"), "{r}");
    assert!(
        !text.contains("avatar_frame"),
        "empty items are not reported as equipped"
    );

    let r = call_tool(&core, "steam_get_store_categories", obj! {});
    assert_eq!(
        r["result"]["structuredContent"]["categories"][0]["name"].as_str(),
        Some("Single-player"),
        "{r}"
    );

    let r = call_tool(
        &core,
        "steam_query_store",
        obj! { "tags" => steam_mcp::arr!["indie"], "count" => 5 },
    );
    let s = &r["result"]["structuredContent"];
    assert_eq!(s["totalMatches"].as_u64(), Some(1234), "{r}");
    assert_eq!(s["items"][0]["tags"], steam_mcp::arr!["Indie"]);
    let r = call_tool(
        &core,
        "steam_query_store",
        obj! { "tags" => steam_mcp::arr!["no such tag"] },
    );
    assert_eq!(r["result"]["isError"].as_bool(), Some(true));

    let r = call_tool(&core, "steam_get_game_server_accounts", obj! {});
    assert_eq!(
        r["result"]["structuredContent"]["servers"][0]["memo"].as_str(),
        Some("eu-1"),
        "{r}"
    );

    let r = call_tool(
        &core,
        "steam_partner_get_microtxn_agreements",
        obj! { "steamid" => "76561197960287930", "appid" => 570 },
    );
    assert_eq!(
        r["result"]["structuredContent"]["agreements"][0]["status"].as_str(),
        Some("Active"),
        "{r}"
    );
    let r = call_tool(
        &core,
        "steam_partner_get_microtxn_agreements",
        obj! { "steamid" => "nope", "appid" => 570 },
    );
    assert_eq!(r["result"]["isError"].as_bool(), Some(true));

    // VAC status is a POST and never cached.
    for _ in 0..2 {
        let r = call_tool(
            &core,
            "steam_partner_get_vac_status",
            obj! { "steamid" => "76561197960287930", "appid" => 570 },
        );
        assert_eq!(
            r["result"]["structuredContent"]["session_verified"].as_bool(),
            Some(true),
            "{r}"
        );
    }
    assert_eq!(
        mock.hits("/ICheatReportingService/RequestVacStatusForUser/v1/"),
        2
    );
    assert!(
        mock.posts
            .lock()
            .unwrap()
            .iter()
            .any(|(p, _)| p.contains("RequestVacStatusForUser"))
    );
}

fn confirm_write(
    core: &std::sync::Arc<steam_mcp::mcp::server::Core>,
    id: i64,
    tool: &str,
    args: Value,
    typed: &str,
) -> Value {
    let caps = obj! { "elicitation" => obj! { "form" => obj! {} } };
    let (r, _) = send(
        core,
        &request(
            id,
            "tools/call",
            obj! { "name" => tool, "arguments" => args.clone() },
            caps.clone(),
        ),
    );
    assert_eq!(
        r["result"]["resultType"].as_str(),
        Some("input_required"),
        "{r}"
    );
    let state = r["result"]["requestState"].as_str().unwrap().to_string();
    let accept = obj! { "confirm" => obj! { "action" => "accept", "content" => obj! { "confirm" => true, "confirmation" => typed } } };
    let (r, _) = send(
        core,
        &request(
            id + 1,
            "tools/call",
            obj! { "name" => tool, "arguments" => args, "inputResponses" => accept, "requestState" => state.as_str() },
            caps,
        ),
    );
    r
}

#[test]
fn gated_leaderboard_and_game_server_writes() {
    let mock = start_mock();
    // Hidden unless writes are enabled.
    let core = core_with(&mock, &[]);
    let (r, _) = send(&core, &request(1, "tools/list", obj! {}, obj! {}));
    assert!(!r.to_string().contains("steam_partner_reset_leaderboard"));

    let core = core_with(&mock, &[("STEAM_MCP_ENABLE_WRITES", "true")]);
    let (r, _) = send(&core, &request(1, "tools/list", obj! {}, obj! {}));
    assert!(
        r.to_string().contains("steam_partner_reset_leaderboard"),
        "{r}"
    );

    // Prime the leaderboard cache.
    call_tool(
        &core,
        "steam_partner_get_leaderboards",
        obj! { "appid" => 480 },
    );
    call_tool(
        &core,
        "steam_partner_get_leaderboards",
        obj! { "appid" => 480 },
    );
    assert_eq!(
        mock.hits("/ISteamLeaderboards/GetLeaderboardsForGame/v2/"),
        1
    );

    let args = obj! { "appid" => 480, "leaderboardid" => 77 };
    // The wrong confirmation asks again and changes nothing.
    let r = confirm_write(
        &core,
        10,
        "steam_partner_reset_leaderboard",
        args.clone(),
        "480",
    );
    assert_eq!(
        r["result"]["resultType"].as_str(),
        Some("input_required"),
        "{r}"
    );
    assert!(
        r["result"]["inputRequests"]["confirm"]["params"]["message"]
            .as_str()
            .unwrap()
            .contains("EVERY entry")
    );
    assert_eq!(mock.hits("/ISteamLeaderboards/ResetLeaderboard/v1/"), 0);

    let r = confirm_write(&core, 20, "steam_partner_reset_leaderboard", args, "77");
    assert_eq!(r["result"]["isError"].as_bool(), Some(false), "{r}");
    assert_eq!(mock.hits("/ISteamLeaderboards/ResetLeaderboard/v1/"), 1);
    // The cached leaderboard list was purged.
    call_tool(
        &core,
        "steam_partner_get_leaderboards",
        obj! { "appid" => 480 },
    );
    assert_eq!(
        mock.hits("/ISteamLeaderboards/GetLeaderboardsForGame/v2/"),
        2
    );

    // A failure reported inside a 200 response is an error.
    let r = confirm_write(
        &core,
        30,
        "steam_partner_delete_leaderboard",
        obj! { "appid" => 480, "name" => "Gone" },
        "Gone",
    );
    assert_eq!(r["result"]["isError"].as_bool(), Some(true), "{r}");

    // Login tokens are returned once, flagged as secret.
    let r = confirm_write(
        &core,
        40,
        "steam_create_game_server_account",
        obj! { "appid" => 730, "memo" => "eu-2" },
        "730",
    );
    let res = &r["result"];
    assert_eq!(
        res["structuredContent"]["response"]["login_token"].as_str(),
        Some("SECRETGSLT0123456789"),
        "{r}"
    );
    assert!(
        res["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("secret (login_token)")
    );
    let posts = mock.posts.lock().unwrap();
    assert!(
        posts
            .iter()
            .any(|(p, b)| p.ends_with("/CreateAccount/v1/") && b.contains("memo=eu-2")),
        "{posts:?}"
    );
}

#[test]
fn gated_partner_commerce_stats_and_workshop_writes() {
    let mock = start_mock();
    let core = core_with(&mock, &[("STEAM_MCP_ENABLE_WRITES", "true")]);
    let sid = "76561197960287930";

    let r = confirm_write(
        &core,
        10,
        "steam_partner_add_inventory_items",
        obj! { "appid" => 480, "steamid" => sid, "itemdefids" => steam_mcp::arr![100, 101] },
        sid,
    );
    assert_eq!(r["result"]["isError"].as_bool(), Some(false), "{r}");

    // Microtransactions must name sandbox or live explicitly.
    let line = obj! { "itemid" => 5, "amount" => 199, "description" => "Hat" };
    let r = call_tool(
        &core,
        "steam_partner_init_microtxn",
        obj! { "appid" => 480, "steamid" => sid, "orderid" => 42, "currency" => "usd", "items" => steam_mcp::arr![line.clone()] },
    );
    assert!(r.to_string().contains("sandbox"), "{r}");
    let args = obj! { "sandbox" => true, "appid" => 480, "steamid" => sid, "orderid" => 42, "currency" => "usd", "items" => steam_mcp::arr![line] };
    let caps = obj! { "elicitation" => obj! { "form" => obj! {} } };
    let (r, _) = send(
        &core,
        &request(
            20,
            "tools/call",
            obj! { "name" => "steam_partner_init_microtxn", "arguments" => args.clone() },
            caps,
        ),
    );
    let msg = r["result"]["inputRequests"]["confirm"]["params"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        msg.contains("SANDBOX") && msg.contains("1× Hat (199)") && msg.contains("total 199 USD"),
        "{msg}"
    );
    let r = confirm_write(&core, 30, "steam_partner_init_microtxn", args, "42");
    assert_eq!(r["result"]["isError"].as_bool(), Some(false), "{r}");
    {
        let posts = mock.posts.lock().unwrap();
        let (_, body) = posts
            .iter()
            .find(|(p, _)| p.ends_with("/InitTxn/v3/"))
            .expect("InitTxn posted");
        for part in [
            "itemcount=1",
            "currency=USD",
            "amount%5B0%5D=199",
            "description%5B0%5D=Hat",
            "usersession=client",
        ] {
            assert!(body.contains(part), "{part} missing from {body}");
        }
    }
    let r = call_tool(
        &core,
        "steam_partner_query_microtxn",
        obj! { "appid" => 480, "orderid" => 42, "sandbox" => true },
    );
    assert_eq!(
        r["result"]["structuredContent"]["params"]["status"].as_str(),
        Some("Approved"),
        "{r}"
    );

    // A failure inside a 200 response with a string result is an error.
    let r = confirm_write(
        &core,
        40,
        "steam_partner_refund_microtxn",
        obj! { "sandbox" => false, "appid" => 480, "orderid" => 43 },
        "43",
    );
    assert_eq!(r["result"]["isError"].as_bool(), Some(true), "{r}");
    assert!(r.to_string().contains("Order not found"));

    let r = confirm_write(
        &core,
        50,
        "steam_partner_set_user_stats",
        obj! { "appid" => 480, "steamid" => sid, "stats" => obj! { "kills" => 12 }, "achievements" => obj! { "ACH_WIN" => true } },
        sid,
    );
    assert_eq!(r["result"]["isError"].as_bool(), Some(false), "{r}");
    let r = confirm_write(
        &core,
        60,
        "steam_partner_update_workshop_tags",
        obj! { "appid" => 480, "publishedfileid" => 123, "add_tags" => steam_mcp::arr!["Maps"] },
        "123",
    );
    assert_eq!(r["result"]["isError"].as_bool(), Some(false), "{r}");
    let posts = mock.posts.lock().unwrap();
    let stats = &posts
        .iter()
        .find(|(p, _)| p.contains("SetUserStatsForGame"))
        .unwrap()
        .1;
    assert!(
        stats.contains("count=2")
            && stats.contains("name%5B1%5D=ACH_WIN")
            && stats.contains("value%5B1%5D=1"),
        "{stats}"
    );
    assert!(
        posts
            .iter()
            .any(|(p, b)| p.contains("UpdateTags") && b.contains("add_tags%5B0%5D=Maps"))
    );
}
