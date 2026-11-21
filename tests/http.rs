//! Streamable HTTP transport tests over real sockets.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use steam_mcp::http::{Headers, framing, read_body, read_head};
use steam_mcp::json::Value;
use steam_mcp::obj;
use steam_mcp::transport::http::HttpServer;

const ADMIN: &str = "admin-token-0123456789";
const VIEWER: &str = "viewer-token-0123456789";

struct Srv {
    addr: SocketAddr,
    core: Arc<steam_mcp::mcp::server::Core>,
}

fn start(extra: &[(&str, &str)]) -> (Srv, Mock) {
    let mock = start_mock();
    let tokens = format!("{ADMIN}=ops:steam:admin;{VIEWER}=viewer:steam:public");
    let mut opts: Vec<(&str, &str)> = vec![
        ("STEAM_MCP_AUTH_MODE", "static"),
        ("STEAM_MCP_AUTH_TOKENS", tokens.as_str()),
        ("STEAM_MCP_SSE_KEEPALIVE", "1"),
    ];
    opts.extend_from_slice(extra);
    let core = core_with(&mock, &opts);
    let server = Arc::new(HttpServer::new(core.clone()).unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        server
            .serve("127.0.0.1:0", |a| tx.send(a).unwrap())
            .unwrap();
    });
    let addr = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    (Srv { addr, core }, mock)
}

struct Resp {
    status: u16,
    headers: Headers,
    body: Value,
    raw: String,
}

fn raw_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Resp {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut r = BufReader::new(s);
    let (status_line, headers) = read_head(&mut r, 65536).unwrap().unwrap();
    let status: u16 = status_line.split(' ').nth(1).unwrap().parse().unwrap();
    let fr = framing(&headers, true).unwrap();
    let body = read_body(&mut r, &fr, 1 << 24).unwrap();
    let raw = String::from_utf8_lossy(&body).to_string();
    Resp {
        status,
        headers,
        body: steam_mcp::json::parse(&raw).unwrap_or(Value::Null),
        raw,
    }
}

fn mcp_headers<'a>(token: &'a str, method: &'a str) -> Vec<(&'a str, String)> {
    vec![
        ("Authorization", format!("Bearer {token}")),
        ("Content-Type", "application/json".into()),
        ("Accept", "application/json, text/event-stream".into()),
        ("MCP-Protocol-Version", "2026-07-28".into()),
        ("Mcp-Method", method.into()),
    ]
}

fn post(addr: SocketAddr, headers: &[(&str, String)], body: &Value) -> Resp {
    let h: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    raw_request(addr, "POST", "/mcp", &h, &body.to_string())
}

#[test]
fn endpoint_methods_and_operational_routes() {
    let (srv, _m) = start(&[]);
    assert_eq!(raw_request(srv.addr, "GET", "/mcp", &[], "").status, 405);
    assert_eq!(raw_request(srv.addr, "DELETE", "/mcp", &[], "").status, 405);
    assert_eq!(
        raw_request(srv.addr, "GET", "/healthz", &[], "").status,
        200
    );
    assert_eq!(raw_request(srv.addr, "GET", "/readyz", &[], "").status, 200);
    assert_eq!(raw_request(srv.addr, "GET", "/nope", &[], "").status, 404);
    assert_eq!(
        raw_request(srv.addr, "GET", "/metrics", &[], "").status,
        401
    );
    let m = raw_request(
        srv.addr,
        "GET",
        "/metrics",
        &[("Authorization", &format!("Bearer {ADMIN}"))],
        "",
    );
    assert_eq!(m.status, 200);
    assert!(m.raw.contains("steam_mcp_cache_entries"));
}

#[test]
fn authorization_challenges_and_metadata() {
    let (srv, _m) = start(&[]);
    let req = request(1, "tools/list", obj! {}, obj! {});
    let r = post(srv.addr, &mcp_headers("", "tools/list")[1..], &req);
    assert_eq!(r.status, 401);
    let challenge = r.headers.get("www-authenticate").unwrap();
    assert!(challenge.starts_with("Bearer "), "{challenge}");
    assert!(
        challenge.contains("resource_metadata=\"http://"),
        "{challenge}"
    );
    let r = post(
        srv.addr,
        &mcp_headers("wrong-token-wrong-token", "tools/list"),
        &req,
    );
    assert_eq!(r.status, 401);
    assert!(
        r.headers
            .get("www-authenticate")
            .unwrap()
            .contains("error=\"invalid_token\"")
    );

    let md = raw_request(
        srv.addr,
        "GET",
        "/.well-known/oauth-protected-resource/mcp",
        &[],
        "",
    );
    assert_eq!(md.status, 200);
    assert!(md.body["resource"].as_str().unwrap().ends_with("/mcp"));
    assert!(
        md.body["scopes_supported"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s.as_str() == Some("steam:partner"))
    );
    assert_eq!(
        raw_request(
            srv.addr,
            "GET",
            "/.well-known/oauth-protected-resource",
            &[],
            ""
        )
        .status,
        200
    );

    // Insufficient scope → 403 with an insufficient_scope challenge.
    let call = request(
        2,
        "tools/call",
        obj! { "name" => "steam_partner_list_apps" },
        obj! {},
    );
    let mut h = mcp_headers(VIEWER, "tools/call");
    h.push(("Mcp-Name", "steam_partner_list_apps".into()));
    let r = post(srv.addr, &h, &call);
    assert_eq!(r.status, 403, "{}", r.raw);
    let c = r.headers.get("www-authenticate").unwrap();
    assert!(
        c.contains("error=\"insufficient_scope\"") && c.contains("scope=\"steam:partner\""),
        "{c}"
    );
}

#[test]
fn json_responses_and_status_codes() {
    let (srv, _m) = start(&[]);
    let r = post(
        srv.addr,
        &mcp_headers(ADMIN, "tools/list"),
        &request(1, "tools/list", obj! {}, obj! {}),
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.headers.get("content-type"), Some("application/json"));
    assert!(r.body["result"]["tools"].as_array().unwrap().len() > 50);

    // Unknown method → 404 with -32601.
    let r = post(
        srv.addr,
        &mcp_headers(ADMIN, "bogus/method"),
        &request(2, "bogus/method", obj! {}, obj! {}),
    );
    assert_eq!(
        (r.status, r.body["error"]["code"].as_i64()),
        (404, Some(-32601))
    );

    // Unsupported version → 400 with -32022.
    let mut body = request(3, "tools/list", obj! {}, obj! {});
    body.get_mut("params")
        .unwrap()
        .get_mut("_meta")
        .unwrap()
        .set("io.modelcontextprotocol/protocolVersion", "2099-01-01");
    let mut h = mcp_headers(ADMIN, "tools/list");
    h[3].1 = "2099-01-01".into();
    let r = post(srv.addr, &h, &body);
    assert_eq!(
        (r.status, r.body["error"]["code"].as_i64()),
        (400, Some(-32022))
    );

    // Missing _meta → 400 with -32602.
    let r = post(
        srv.addr,
        &mcp_headers(ADMIN, "tools/list"),
        &parse(r#"{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}"#),
    );
    assert_eq!(
        (r.status, r.body["error"]["code"].as_i64()),
        (400, Some(-32602))
    );

    // Malformed JSON → 400 parse error without id.
    let owned = mcp_headers(ADMIN, "tools/list");
    let h: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let r = raw_request(srv.addr, "POST", "/mcp", &h, "{nope");
    assert_eq!(
        (r.status, r.body["error"]["code"].as_i64()),
        (400, Some(-32700))
    );
    assert!(r.body.get("id").is_none());

    // Notifications are accepted with 202 and no body.
    let r = post(
        srv.addr,
        &mcp_headers(ADMIN, "notifications/cancelled"),
        &parse(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":99}}"#),
    );
    assert_eq!(r.status, 202);
    assert!(r.raw.is_empty());

    // Wrong content type.
    let mut h = mcp_headers(ADMIN, "tools/list");
    h[1].1 = "text/plain".into();
    assert_eq!(
        post(srv.addr, &h, &request(5, "tools/list", obj! {}, obj! {})).status,
        415
    );
}

#[test]
fn header_body_validation() {
    let (srv, _m) = start(&[]);
    let call = request(
        1,
        "tools/call",
        obj! { "name" => "steam_get_current_players", "arguments" => obj! { "appid" => 570 } },
        obj! {},
    );
    let base = mcp_headers(ADMIN, "tools/call");

    let r = post(srv.addr, &base, &call);
    assert_eq!(
        (r.status, r.body["error"]["code"].as_i64()),
        (400, Some(-32020)),
        "missing Mcp-Name"
    );

    let mut h = base.clone();
    h.push(("Mcp-Name", "steam_get_app_news".into()));
    h.push(("Mcp-Param-AppId", "570".into()));
    assert_eq!(
        post(srv.addr, &h, &call).body["error"]["code"].as_i64(),
        Some(-32020),
        "name mismatch"
    );

    let mut h = base.clone();
    h.push(("Mcp-Name", "steam_get_current_players".into()));
    let r = post(srv.addr, &h, &call);
    assert_eq!(
        r.body["error"]["code"].as_i64(),
        Some(-32020),
        "missing Mcp-Param-AppId: {}",
        r.raw
    );

    let mut h = base.clone();
    h.push(("Mcp-Name", "steam_get_current_players".into()));
    h.push(("Mcp-Param-AppId", "730".into()));
    assert_eq!(
        post(srv.addr, &h, &call).body["error"]["code"].as_i64(),
        Some(-32020),
        "param mismatch"
    );

    // Base64 sentinel decoding and numeric comparison.
    let mut h = base.clone();
    h.push((
        "Mcp-Name",
        format!(
            "=?base64?{}?=",
            steam_mcp::util::encoding::b64_encode(b"steam_get_current_players")
        ),
    ));
    h.push(("mcp-param-appid", "570.0".into()));
    let r = post(srv.addr, &h, &call);
    assert_eq!(r.status, 200, "{}", r.raw);
    assert_eq!(r.body["result"]["isError"].as_bool(), Some(false));

    // Mismatched protocol version header vs body.
    let mut h = base.clone();
    h[3].1 = "2025-11-25".into();
    h.push(("Mcp-Name", "steam_get_current_players".into()));
    h.push(("Mcp-Param-AppId", "570".into()));
    assert_eq!(
        post(srv.addr, &h, &call).body["error"]["code"].as_i64(),
        Some(-32020)
    );

    // Mcp-Method mismatch.
    let mut h = mcp_headers(ADMIN, "tools/list");
    h.push(("Mcp-Name", "steam_get_current_players".into()));
    assert_eq!(
        post(srv.addr, &h, &call).body["error"]["code"].as_i64(),
        Some(-32020)
    );

    // Session headers from older clients are ignored.
    let mut h = mcp_headers(ADMIN, "tools/list");
    h.push(("Mcp-Session-Id", "abc".into()));
    h.push(("Last-Event-ID", "5".into()));
    let r = post(srv.addr, &h, &request(2, "tools/list", obj! {}, obj! {}));
    assert_eq!(r.status, 200);
    assert!(r.headers.get("mcp-session-id").is_none());
}

#[test]
fn origin_validation() {
    let (srv, _m) = start(&[]);
    let mut h = mcp_headers(ADMIN, "tools/list");
    h.push(("Origin", "https://evil.example".into()));
    let r = post(srv.addr, &h, &request(1, "tools/list", obj! {}, obj! {}));
    assert_eq!(r.status, 403);
    assert!(r.body.get("id").is_none());
    let mut h = mcp_headers(ADMIN, "tools/list");
    h.push(("Origin", "http://localhost:6274".into()));
    let r = post(srv.addr, &h, &request(1, "tools/list", obj! {}, obj! {}));
    assert_eq!(r.status, 200);
    assert_eq!(
        r.headers.get("access-control-allow-origin"),
        Some("http://localhost:6274")
    );
}

fn read_sse_events(
    reader: &mut BufReader<TcpStream>,
    until: impl Fn(&Value) -> bool,
) -> Vec<Value> {
    let mut events = Vec::new();
    let mut data = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return events;
        }
        let l = line.trim_end_matches(['\r', '\n']);
        if let Some(d) = l.strip_prefix("data: ") {
            data.push_str(d);
        } else if l.is_empty() && !data.is_empty() {
            let v = steam_mcp::json::parse(&data).unwrap();
            data.clear();
            let stop = until(&v);
            events.push(v);
            if stop {
                return events;
            }
        }
    }
}

fn open_stream(addr: SocketAddr, headers: &[(&str, String)], body: &Value) -> BufReader<TcpStream> {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let body = body.to_string();
    let mut req = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(&body);
    s.write_all(req.as_bytes()).unwrap();
    let mut r = BufReader::new(s);
    let (status, headers) = read_head(&mut r, 65536).unwrap().unwrap();
    assert!(status.contains(" 200 "), "{status}");
    assert_eq!(headers.get("content-type"), Some("text/event-stream"));
    assert_eq!(headers.get("x-accel-buffering"), Some("no"));
    r
}

#[test]
fn sse_progress_stream() {
    let (srv, _m) = start(&[]);
    let call = request(
        7,
        "tools/call",
        obj! { "name" => "steam_get_app_reviews", "arguments" => obj! { "appid" => 570, "max_pages" => 2 }, "_meta" => obj! { "progressToken" => 1 } },
        obj! {},
    );
    let mut h = mcp_headers(ADMIN, "tools/call");
    h.push(("Mcp-Name", "steam_get_app_reviews".into()));
    h.push(("Mcp-Param-AppId", "570".into()));
    let mut r = open_stream(srv.addr, &h, &call);
    let events = read_sse_events(&mut r, |v| v.get("id").is_some());
    let progress = events
        .iter()
        .filter(|e| e["method"].as_str() == Some("notifications/progress"))
        .count();
    assert_eq!(progress, 2);
    let last = events.last().unwrap();
    assert_eq!(last["id"].as_i64(), Some(7));
    assert_eq!(
        last["result"]["structuredContent"]["reviews"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    // The stream ends after the response.
    let mut rest = String::new();
    let _ = r.read_to_string(&mut rest);
}

#[test]
fn subscription_stream_lifecycle() {
    let (srv, _m) = start(&[]);
    let listen = request(
        11,
        "subscriptions/listen",
        obj! { "notifications" => obj! { "toolsListChanged" => true } },
        obj! {},
    );
    let mut r = open_stream(
        srv.addr,
        &mcp_headers(ADMIN, "subscriptions/listen"),
        &listen,
    );
    let ack = read_sse_events(&mut r, |_| true);
    assert_eq!(
        ack[0]["method"].as_str(),
        Some("notifications/subscriptions/acknowledged")
    );
    assert_eq!(
        ack[0]["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"].as_i64(),
        Some(11)
    );
    assert_eq!(srv.core.hub.len(), 1);

    assert_eq!(srv.core.hub.notify_tools_changed(), 1);
    let ev = read_sse_events(&mut r, |_| true);
    assert_eq!(
        ev[0]["method"].as_str(),
        Some("notifications/tools/list_changed")
    );
    assert_eq!(
        ev[0]["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"].as_i64(),
        Some(11)
    );

    // Keep-alive comments arrive during quiet periods.
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    assert!(
        line.starts_with(':'),
        "expected keep-alive comment, got {line:?}"
    );

    // Closing the stream cancels the subscription.
    drop(r);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !srv.core.hub.is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "subscription was not cleaned up"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Graceful shutdown sends a completion response on open streams.
    let mut r = open_stream(
        srv.addr,
        &mcp_headers(ADMIN, "subscriptions/listen"),
        &request(
            12,
            "subscriptions/listen",
            obj! { "notifications" => obj! { "promptsListChanged" => true } },
            obj! {},
        ),
    );
    read_sse_events(&mut r, |_| true);
    srv.core.shutdown();
    let done = read_sse_events(&mut r, |v| v.get("id").is_some());
    let last = done.last().unwrap();
    assert_eq!(last["id"].as_i64(), Some(12));
    assert_eq!(
        last["result"]["_meta"]["io.modelcontextprotocol/subscriptionId"].as_i64(),
        Some(12)
    );
}
