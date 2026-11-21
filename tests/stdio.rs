//! stdio transport tests against the real binary.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use common::*;
use steam_mcp::json::Value;
use steam_mcp::obj;

struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Value>,
}

fn spawn(mock: &Mock) -> Proc {
    let mut child = Command::new(env!("CARGO_BIN_EXE_steam-mcp"))
        .arg("--stdio")
        .env_clear()
        .env("STEAM_API_KEY", KEY)
        .env("STEAM_PUBLISHER_KEY", PUB_KEY)
        .env("STEAM_FINANCIAL_KEY", FIN_KEY)
        .env("STEAM_MCP_API_BASE", &mock.base)
        .env("STEAM_MCP_PARTNER_BASE", &mock.base)
        .env("STEAM_MCP_STORE_BASE", &mock.base)
        .env("STEAM_MCP_USE_PROXY_ENV", "false")
        .env("STEAM_MCP_UPSTREAM_RETRIES", "0")
        .env("STEAM_MCP_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            tx.send(steam_mcp::json::parse(&line).expect("stdout must only carry JSON-RPC")).ok();
        }
    });
    let stdin = child.stdin.take();
    Proc { child, stdin, rx }
}

impl Proc {
    fn send(&mut self, v: &Value) {
        let s = self.stdin.as_mut().unwrap();
        writeln!(s, "{v}").unwrap();
        s.flush().unwrap();
    }
    fn recv(&self) -> Value {
        self.rx.recv_timeout(Duration::from_secs(10)).expect("timed out waiting for output")
    }
    fn recv_id(&self, id: i64) -> Value {
        loop {
            let v = self.recv();
            if v["id"].as_i64() == Some(id) {
                return v;
            }
        }
    }
}

#[test]
fn concurrency_cancellation_and_shutdown() {
    let mock = start_mock();
    mock.sales_delay_ms.store(400, Ordering::SeqCst);
    let mut p = spawn(&mock);

    // A slow request, then a fast one: the fast one must not wait.
    p.send(&request(1, "tools/call", obj! { "name" => "steam_partner_sales_summary", "arguments" => obj! { "date" => "2026/07/01" } }, obj! {}));
    p.send(&request(2, "server/discover", obj! {}, obj! {}));
    let first = p.recv();
    assert_eq!(first["id"].as_i64(), Some(2), "fast request answered first");

    // Duplicate in-flight id is rejected.
    p.send(&request(1, "tools/list", obj! {}, obj! {}));
    let dup = p.recv();
    assert_eq!(dup["error"]["code"].as_i64(), Some(-32600), "{dup}");

    // Cancel the slow request: no response may be sent for it.
    p.send(&parse(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1,"reason":"user abort"}}"#));
    p.send(&request(3, "tools/list", obj! {}, obj! {}));
    let r = p.recv();
    assert_eq!(r["id"].as_i64(), Some(3));
    std::thread::sleep(Duration::from_millis(1500));

    // Open a subscription, then close stdin: the server completes it and exits.
    p.send(&request(4, "subscriptions/listen", obj! { "notifications" => obj! { "toolsListChanged" => true } }, obj! {}));
    let mut seen = Vec::new();
    let ack = loop {
        let v = p.recv();
        if v["method"].as_str() == Some("notifications/subscriptions/acknowledged") {
            break v;
        }
        seen.push(v);
    };
    assert_eq!(ack["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"].as_i64(), Some(4));
    assert!(seen.iter().all(|v| v["id"].as_i64() != Some(1)), "cancelled request produced a response: {seen:?}");
    drop(p.stdin.take());
    let done = p.recv_id(4);
    assert_eq!(done["result"]["resultType"].as_str(), Some("complete"));
    let status = p.child.wait().unwrap();
    assert!(status.success());
}

#[test]
fn invalid_input_is_reported_without_crashing() {
    let mock = start_mock();
    let mut p = spawn(&mock);
    p.stdin.as_mut().unwrap().write_all(b"{not json}\n\n   \n").unwrap();
    let e = p.recv();
    assert_eq!(e["error"]["code"].as_i64(), Some(-32700));
    assert!(e.get("id").is_none());
    p.send(&request(1, "initialize", obj! { "protocolVersion" => "2025-11-25" }, obj! {}));
    let e = p.recv();
    assert_eq!(e["error"]["code"].as_i64(), Some(-32022));
    assert!(e["error"]["message"].as_str().unwrap().contains("2026-07-28"));
    p.send(&request(2, "tools/call", obj! { "name" => "steam_get_app_details", "arguments" => obj! { "appid" => 570 } }, obj! {}));
    assert_eq!(p.recv()["result"]["structuredContent"]["summary"]["name"].as_str(), Some("Dota 2"));
    drop(p.stdin.take());
    assert!(p.child.wait().unwrap().success());
}
