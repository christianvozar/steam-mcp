//! stdio transport: newline-delimited JSON-RPC on stdin/stdout.
//!
//! Requests are processed concurrently on worker threads; all output is
//! serialized through one writer. `notifications/cancelled` cancels in-flight
//! requests (whose responses are then suppressed) and ends subscriptions.
//! EOF on stdin triggers graceful shutdown.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::json::{self, Value};
use crate::mcp::context::{Principal, Sink, Transport};
use crate::mcp::protocol::{McpError, error_response};
use crate::mcp::server::{Core, Reply};
use crate::util::CancelToken;

/// Maximum accepted line length (one JSON-RPC message).
const MAX_LINE: usize = 16 * 1024 * 1024;
/// Maximum concurrently executing requests.
const MAX_WORKERS: usize = 64;

/// Serialized line writer shared by all workers.
pub struct LineSink<W: Write + Send> {
    out: Mutex<W>,
    broken: AtomicBool,
}

impl<W: Write + Send> LineSink<W> {
    pub fn new(out: W) -> Self {
        LineSink {
            out: Mutex::new(out),
            broken: AtomicBool::new(false),
        }
    }
}

impl<W: Write + Send> Sink for LineSink<W> {
    fn send(&self, msg: &Value) -> bool {
        if self.broken.load(Ordering::Relaxed) {
            return false;
        }
        // Serialization never emits raw newlines (they are escaped).
        let mut line = msg.to_string();
        line.push('\n');
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let ok = out
            .write_all(line.as_bytes())
            .and_then(|_| out.flush())
            .is_ok();
        if !ok {
            self.broken.store(true, Ordering::Relaxed);
        }
        ok
    }
}

struct Inflight {
    map: Mutex<HashMap<String, CancelToken>>,
    count: AtomicUsize,
    idle: (Mutex<()>, Condvar),
}

impl Inflight {
    fn wait_for_slot(&self) {
        let (m, cv) = &self.idle;
        let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
        while self.count.load(Ordering::SeqCst) >= MAX_WORKERS {
            g = cv
                .wait_timeout(g, Duration::from_millis(100))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
    fn done(&self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
        self.idle.1.notify_all();
    }
}

/// Reads one line with a length cap. `Ok(None)` at EOF.
fn read_capped_line(r: &mut impl BufRead, buf: &mut Vec<u8>) -> io::Result<Option<()>> {
    buf.clear();
    let n = r
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() > MAX_LINE && !buf.ends_with(b"\n") {
        // Discard the remainder of an oversized line.
        let mut skip = Vec::new();
        loop {
            skip.clear();
            let m = r.by_ref().take(1 << 20).read_until(b'\n', &mut skip)?;
            if m == 0 || skip.ends_with(b"\n") {
                break;
            }
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message exceeds maximum size",
        ));
    }
    Ok(Some(()))
}

/// Serves MCP over arbitrary reader/writer streams (stdin/stdout in
/// production; pipes or sockets in tests).
pub fn serve<R: Read, W: Write + Send + 'static>(
    core: Arc<Core>,
    input: R,
    output: W,
) -> io::Result<()> {
    let sink: Arc<LineSink<W>> = Arc::new(LineSink::new(output));
    let inflight = Arc::new(Inflight {
        map: Mutex::new(HashMap::new()),
        count: AtomicUsize::new(0),
        idle: (Mutex::new(()), Condvar::new()),
    });
    let mut reader = BufReader::with_capacity(256 * 1024, input);
    let mut buf = Vec::new();
    crate::log_info!("stdio", "serving MCP over stdio");
    loop {
        match read_capped_line(&mut reader, &mut buf) {
            Ok(None) => break,
            Ok(Some(())) => {}
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                sink.send(&error_response(
                    None,
                    &McpError::invalid_request(e.to_string()),
                ));
                continue;
            }
            Err(e) => {
                crate::log_error!("stdio", "stdin read failed", error = e);
                break;
            }
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg = match json::parse(line) {
            Ok(m) => m,
            Err(e) => {
                sink.send(&error_response(
                    None,
                    &McpError::parse_error(format!("Invalid JSON: {e}")),
                ));
                continue;
            }
        };
        let id = msg.get("id").filter(|v| !v.is_null()).cloned();
        let is_request = id.is_some() && msg.get("method").is_some();

        if !is_request {
            // Notifications run inline; cancellation needs the in-flight map.
            if msg["method"].as_str() == Some("notifications/cancelled") {
                let rid = msg.at(&["params", "requestId"]).to_string();
                let token = inflight
                    .map
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&rid)
                    .cloned();
                if let Some(t) = token {
                    t.cancel();
                    crate::log_debug!(
                        "stdio",
                        "request cancelled",
                        id = rid,
                        reason = msg.at(&["params", "reason"]).as_str().unwrap_or("")
                    );
                }
            }
            let s: Arc<dyn Sink> = sink.clone();
            if let Reply::Response(r, _) = core.handle_message(
                &msg,
                Principal::local(),
                Transport::Stdio,
                s,
                CancelToken::new(),
            ) {
                sink.send(&r);
            }
            continue;
        }

        let key = id.as_ref().map(|i| i.to_string()).unwrap_or_default();
        let token = CancelToken::new();
        {
            let mut map = inflight.map.lock().unwrap_or_else(|e| e.into_inner());
            if map.contains_key(&key) {
                drop(map);
                sink.send(&error_response(
                    id.as_ref(),
                    &McpError::invalid_request("A request with this id is already in flight"),
                ));
                continue;
            }
            map.insert(key.clone(), token.clone());
        }
        inflight.wait_for_slot();
        inflight.count.fetch_add(1, Ordering::SeqCst);
        let (core2, sink2, inflight2) = (core.clone(), sink.clone(), inflight.clone());
        let spawned = std::thread::Builder::new()
            .name("stdio-req".into())
            .spawn(move || {
                let s: Arc<dyn Sink> = sink2.clone();
                let reply = core2.handle_message(
                    &msg,
                    Principal::local(),
                    Transport::Stdio,
                    s,
                    token.clone(),
                );
                match reply {
                    Reply::Response(r, _) if !token.is_cancelled() => {
                        sink2.send(&r);
                    }
                    // Subscriptions stay registered in the hub; their notifications
                    // and final response go through the shared sink.
                    _ => {}
                }
                inflight2
                    .map
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&key);
                inflight2.done();
            });
        if spawned.is_err() {
            inflight.done();
            sink.send(&error_response(
                id.as_ref(),
                &McpError::internal("could not schedule request"),
            ));
        }
    }
    crate::log_info!("stdio", "stdin closed; shutting down");
    // Stop accepting work, end subscriptions gracefully, let workers finish,
    // then persist the cache.
    core.begin_shutdown();
    let grace = Duration::from_secs(core.config.get().shutdown_grace_secs);
    let deadline = Instant::now() + grace;
    while inflight.count.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    core.drain(deadline.saturating_duration_since(Instant::now()));
    core.finish_shutdown();
    Ok(())
}
