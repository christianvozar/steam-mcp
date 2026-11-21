//! Process lifecycle: graceful shutdown on SIGTERM in HTTP mode.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use steam_mcp::obj;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn post(port: u16, method: &str, name: Option<&str>, body: &str) -> TcpStream {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: {method}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(n) = name {
        req.push_str(&format!("Mcp-Name: {n}\r\nMcp-Param-AppId: 570\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    s
}

#[cfg(unix)]
#[test]
fn sigterm_drains_completes_subscriptions_and_persists_cache() {
    let mock = start_mock();
    let port = free_port();
    let cache = std::env::temp_dir().join(format!(
        "steam-mcp-life-{}.jsonl",
        steam_mcp::util::crypto::random_id(4)
    ));
    let mut child = Command::new(env!("CARGO_BIN_EXE_steam-mcp"))
        .args(["--http", &format!("127.0.0.1:{port}")])
        .env_clear()
        .env("STEAM_API_KEY", KEY)
        .env("STEAM_MCP_API_BASE", &mock.base)
        .env("STEAM_MCP_STORE_BASE", &mock.base)
        .env("STEAM_MCP_PARTNER_BASE", &mock.base)
        .env("STEAM_MCP_USE_PROXY_ENV", "false")
        .env("STEAM_MCP_CACHE_FILE", &cache)
        .env("STEAM_MCP_CACHE_PERSIST_INTERVAL", "3600")
        .env("STEAM_MCP_LOG", "error")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Wait for the listener.
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Warm the cache.
    let call = request(
        1,
        "tools/call",
        obj! { "name" => "steam_get_app_details", "arguments" => obj! { "appid" => 570 } },
        obj! {},
    );
    let mut s = post(
        port,
        "tools/call",
        Some("steam_get_app_details"),
        &call.to_string(),
    );
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.contains("Dota 2"));

    // Open a subscription stream and read the acknowledgment.
    let listen = request(
        7,
        "subscriptions/listen",
        obj! { "notifications" => obj! { "toolsListChanged" => true } },
        obj! {},
    );
    let mut stream = BufReader::new(post(
        port,
        "subscriptions/listen",
        None,
        &listen.to_string(),
    ));
    let mut line = String::new();
    loop {
        line.clear();
        stream.read_line(&mut line).unwrap();
        if line.contains("subscriptions/acknowledged") {
            break;
        }
    }

    // SIGTERM → graceful completion of the stream, cache persisted, exit 0.
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let mut rest = String::new();
    let _ = stream.read_to_string(&mut rest);
    assert!(
        rest.contains("\"id\":7") && rest.contains("\"resultType\":\"complete\""),
        "stream did not complete gracefully: {rest:?}"
    );
    let exit = child.wait().unwrap();
    assert!(exit.success(), "exit status {exit:?}");
    let persisted = std::fs::read_to_string(&cache).unwrap();
    assert!(
        persisted.contains("Dota 2"),
        "cache not persisted on SIGTERM"
    );
    let _ = std::fs::remove_file(cache);
}
