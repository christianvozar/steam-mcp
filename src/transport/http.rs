//! Streamable HTTP transport (MCP 2026-07-28).
//!
//! * Single MCP endpoint accepting POST; GET/DELETE answer 405 (no sessions,
//!   no standalone SSE stream, no resumability).
//! * Each request is answered with `application/json`, or a request-scoped
//!   `text/event-stream` when it carries a progress token or log level, or
//!   is `subscriptions/listen` (long-lived, with SSE keep-alives).
//! * Header/body validation: `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`
//!   and `Mcp-Param-*` (from `x-mcp-header`), with Base64 sentinel decoding.
//! * Origin validation (DNS-rebinding protection), CORS, OAuth 2.1
//!   resource-server authorization, optional native TLS.
//! * Closing a response stream cancels the request.
//! * Operational endpoints: `/healthz`, `/readyz`, `/metrics`.

use std::io::{self, BufReader};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::auth::{AuthFailure, Authenticator};
use crate::http::server::{HttpRequest, read_request, start_sse, write_response, write_sse_comment, write_sse_event};
use crate::http::{Headers, Stream};
use crate::json::{self, Value};
use crate::mcp::context::{NullSink, Principal, Sink, Transport};
use crate::mcp::protocol::{FORBIDDEN, McpError, META_LOG_LEVEL, error_response};
use crate::mcp::server::{Core, Reply};
use crate::util::CancelToken;
use crate::util::encoding::b64_decode;

/// SSE response stream shared between the request thread and notifiers.
struct SseSink {
    stream: Arc<Mutex<Stream>>,
    closed: AtomicBool,
}

impl Sink for SseSink {
    fn send(&self, msg: &Value) -> bool {
        if self.closed.load(Ordering::SeqCst) {
            return false;
        }
        let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        let ok = write_sse_event(&mut *s, &msg.to_string()).is_ok();
        if !ok {
            self.closed.store(true, Ordering::SeqCst);
        }
        ok
    }

    fn keepalive(&self) -> bool {
        if self.closed.load(Ordering::SeqCst) {
            return false;
        }
        let mut s = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        let ok = write_sse_comment(&mut *s).is_ok();
        if !ok {
            self.closed.store(true, Ordering::SeqCst);
        }
        ok
    }
}

pub struct HttpServer {
    core: Arc<Core>,
    auth: Authenticator,
    tls: Option<Arc<rustls::ServerConfig>>,
    active: AtomicUsize,
}

fn load_tls(cert: &str, key: &str) -> Result<Arc<rustls::ServerConfig>, String> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert).map_err(|e| format!("cannot read {cert}: {e}"))?.collect::<Result<_, _>>().map_err(|e| format!("invalid certificate PEM: {e}"))?;
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("cannot read private key {key}: {e}"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("invalid certificate/key: {e}"))?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

fn is_loopback_origin(origin: &str) -> bool {
    match crate::http::Url::parse(origin) {
        Some(u) => matches!(u.host.as_str(), "localhost" | "127.0.0.1" | "::1"),
        None => false,
    }
}

/// Decodes a header value that may use the `=?base64?…?=` sentinel.
pub fn decode_header_value(v: &str) -> Option<String> {
    if let Some(inner) = v.strip_prefix("=?base64?").and_then(|s| s.strip_suffix("?=")) {
        return b64_decode(inner).and_then(|b| String::from_utf8(b).ok());
    }
    if v.bytes().all(|b| b == b'\t' || (0x20..=0x7e).contains(&b)) { Some(v.to_string()) } else { None }
}

impl HttpServer {
    pub fn new(core: Arc<Core>) -> Result<HttpServer, String> {
        let cfg = core.config.get();
        let tls = match (&cfg.tls_cert, &cfg.tls_key) {
            (Some(c), Some(k)) => Some(load_tls(c, k)?),
            _ => None,
        };
        let auth = Authenticator::new(&cfg)?;
        Ok(HttpServer { core, auth, tls, active: AtomicUsize::new(0) })
    }

    /// Binds and serves until the core is stopped. Returns the bound address
    /// through `on_bound` (useful with port 0).
    pub fn serve(self: Arc<Self>, bind: &str, on_bound: impl FnOnce(SocketAddr)) -> io::Result<()> {
        let listener = TcpListener::bind(bind)?;
        let addr = listener.local_addr()?;
        on_bound(addr);
        let cfg = self.core.config.get();
        crate::log_info!("http", "listening", addr = addr, path = cfg.http_path, tls = self.tls.is_some(), auth = format!("{:?}", cfg.auth.mode));
        if !addr.ip().is_loopback() && cfg.auth.mode == crate::config::AuthMode::None {
            crate::log_warn!("http", "serving on a non-loopback address without authentication; set STEAM_MCP_AUTH_MODE for production");
        }
        listener.set_nonblocking(true)?;
        while !self.core.stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, peer)) => {
                    stream.set_nonblocking(false).ok();
                    let max = self.core.config.get().max_connections;
                    if self.active.load(Ordering::SeqCst) >= max {
                        let mut s = stream;
                        let _ = write_response(&mut s, 503, &[("Retry-After", "1".into()), ("Content-Type", "text/plain".into())], b"server busy", false);
                        continue;
                    }
                    self.active.fetch_add(1, Ordering::SeqCst);
                    let this = self.clone();
                    let spawned = std::thread::Builder::new().name("http-conn".into()).spawn(move || {
                        this.connection(stream, peer);
                        this.active.fetch_sub(1, Ordering::SeqCst);
                    });
                    if spawned.is_err() {
                        self.active.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => {
                    crate::log_warn!("http", "accept failed", error = e);
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        Ok(())
    }

    fn connection(&self, tcp: TcpStream, peer: SocketAddr) {
        tcp.set_nodelay(true).ok();
        tcp.set_read_timeout(Some(Duration::from_secs(60))).ok();
        tcp.set_write_timeout(Some(Duration::from_secs(30))).ok();
        let stream = match &self.tls {
            Some(cfg) => match rustls::ServerConnection::new(cfg.clone()) {
                Ok(conn) => Stream::ServerTls(Box::new(rustls::StreamOwned::new(conn, tcp))),
                Err(e) => {
                    crate::log_warn!("http", "TLS setup failed", error = e);
                    return;
                }
            },
            None => Stream::Plain(tcp),
        };
        let cfg = self.core.config.get();
        let mut reader = BufReader::with_capacity(32 * 1024, stream);
        loop {
            let req = read_request(&mut reader, 64 * 1024, cfg.max_request_bytes);
            let mut req = match req {
                Ok(Some(r)) => r,
                Ok(None) => return,
                Err(e) => {
                    let _ = write_response(reader.get_mut(), e.status, &[("Content-Type", "text/plain".into())], e.message.as_bytes(), false);
                    return;
                }
            };
            req.remote = Some(peer);
            match self.route(req, reader) {
                Some(r) => reader = r,
                None => return,
            }
        }
    }

    /// Handles one request. Returns the reader for keep-alive, or `None`
    /// when the connection must close.
    fn route(&self, req: HttpRequest, mut reader: BufReader<Stream>) -> Option<BufReader<Stream>> {
        let cfg = self.core.config.get();
        let keep = req.keep_alive();
        let tls = self.tls.is_some();
        let host = req.headers.get("host").map(String::from);
        let origin = req.headers.get("origin").map(String::from);
        let origin_ok = match &origin {
            None => true,
            Some(o) => cfg.allowed_origins.iter().any(|a| a == "*" || a.eq_ignore_ascii_case(o)) || (cfg.allowed_origins.is_empty() && is_loopback_origin(o)),
        };
        let mut cors: Vec<(&str, String)> = Vec::new();
        if let (Some(o), true) = (&origin, origin_ok) {
            cors.push(("Access-Control-Allow-Origin", o.clone()));
            cors.push(("Vary", "Origin".into()));
            cors.push(("Access-Control-Expose-Headers", "WWW-Authenticate, Content-Type".into()));
        }
        let respond = |reader: &mut BufReader<Stream>, status: u16, mut headers: Vec<(&str, String)>, ctype: &str, body: &[u8]| {
            headers.push(("Content-Type", ctype.to_string()));
            headers.push(("Cache-Control", "no-store".into()));
            headers.extend(cors.iter().cloned());
            write_response(reader.get_mut(), status, &headers, body, keep).is_ok()
        };
        let prm_path = {
            let p = Authenticator::metadata_url(&cfg, host.as_deref(), tls);
            crate::http::Url::parse(&p).map(|u| u.path().to_string()).unwrap_or_default()
        };
        let path = req.path.as_str();

        if !origin_ok {
            let body = error_response(None, &McpError::invalid_request("Origin not allowed")).to_string();
            respond(&mut reader, 403, vec![], "application/json", body.as_bytes());
            return None;
        }

        if path == cfg.http_path {
            match req.method.as_str() {
                "POST" => return self.handle_post(req, reader, &cors, keep),
                "OPTIONS" => {
                    let mut h = vec![
                        ("Access-Control-Allow-Methods", "POST, OPTIONS".to_string()),
                        ("Access-Control-Allow-Headers", req.headers.get("access-control-request-headers").unwrap_or("Content-Type, Authorization, MCP-Protocol-Version, Mcp-Method, Mcp-Name").to_string()),
                        ("Access-Control-Max-Age", "600".into()),
                    ];
                    h.extend(cors.iter().cloned());
                    let ok = write_response(reader.get_mut(), 204, &h, b"", keep).is_ok();
                    return (ok && keep).then_some(reader);
                }
                _ => {
                    let ok = respond(&mut reader, 405, vec![("Allow", "POST, OPTIONS".into())], "text/plain", b"This MCP endpoint accepts POST only (MCP 2026-07-28 has no GET stream or sessions).");
                    return (ok && keep).then_some(reader);
                }
            }
        }
        if req.method == "GET" && (path == "/.well-known/oauth-protected-resource" || path == prm_path) {
            if cfg.auth.mode == crate::config::AuthMode::None {
                respond(&mut reader, 404, vec![], "text/plain", b"authorization is not enabled");
            } else {
                let doc = Authenticator::metadata(&cfg, host.as_deref(), tls, &self.core.scopes_supported());
                respond(&mut reader, 200, vec![], "application/json", doc.to_string().as_bytes());
            }
            return keep.then_some(reader);
        }
        if req.method == "GET" && path == "/healthz" {
            respond(&mut reader, 200, vec![], "application/json", br#"{"status":"ok"}"#);
            return keep.then_some(reader);
        }
        if req.method == "GET" && path == "/readyz" {
            let ready = !self.core.stop.load(Ordering::Relaxed);
            respond(&mut reader, if ready { 200 } else { 503 }, vec![], "application/json", if ready { br#"{"status":"ready"}"# } else { br#"{"status":"stopping"}"# });
            return keep.then_some(reader);
        }
        if req.method == "GET" && path == "/metrics" {
            if cfg.auth.mode != crate::config::AuthMode::None {
                match self.auth.authenticate(&cfg, req.headers.get("authorization")) {
                    Ok(p) if p.has_scope(crate::mcp::context::scopes::ADMIN) => {}
                    _ => {
                        respond(&mut reader, 401, vec![("WWW-Authenticate", Authenticator::challenge(&Authenticator::metadata_url(&cfg, host.as_deref(), tls), None, None, Some("steam:admin")))], "text/plain", b"unauthorized");
                        return keep.then_some(reader);
                    }
                }
            }
            respond(&mut reader, 200, vec![], "text/plain; version=0.0.4", self.core.metrics_text().as_bytes());
            return keep.then_some(reader);
        }
        respond(&mut reader, 404, vec![], "text/plain", b"not found");
        keep.then_some(reader)
    }

    fn handle_post(&self, req: HttpRequest, mut reader: BufReader<Stream>, cors: &[(&str, String)], keep: bool) -> Option<BufReader<Stream>> {
        let cfg = self.core.config.get();
        let tls = self.tls.is_some();
        let host = req.headers.get("host").map(String::from);
        let md_url = Authenticator::metadata_url(&cfg, host.as_deref(), tls);
        let send_json = |reader: &mut BufReader<Stream>, status: u16, extra: Vec<(&str, String)>, body: &Value| -> bool {
            let mut h = vec![("Content-Type", "application/json".to_string()), ("Cache-Control", "no-store".to_string())];
            h.extend(extra);
            h.extend(cors.iter().cloned());
            write_response(reader.get_mut(), status, &h, body.to_string().as_bytes(), keep).is_ok()
        };
        // Authorization.
        let principal: Principal = match self.auth.authenticate(&cfg, req.headers.get("authorization")) {
            Ok(p) => p,
            Err(AuthFailure { status, error, description }) => {
                let challenge = Authenticator::challenge(&md_url, error, if error.is_some() { Some(&description) } else { None }, Some("steam:public"));
                let body = error_response(None, &McpError::new(crate::mcp::protocol::UNAUTHORIZED, description));
                let ok = send_json(&mut reader, status, vec![("WWW-Authenticate", challenge)], &body);
                return (ok && keep).then_some(reader);
            }
        };

        // Content negotiation.
        let ctype = req.headers.get("content-type").unwrap_or("");
        if !ctype.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json") {
            let ok = send_json(&mut reader, 415, vec![], &error_response(None, &McpError::invalid_request("Content-Type must be application/json")));
            return (ok && keep).then_some(reader);
        }
        let accept_sse = req.headers.has_token("accept", "text/event-stream") || req.headers.has_token("accept", "*/*");
        let accept_json = req.headers.has_token("accept", "application/json") || req.headers.has_token("accept", "*/*");
        if req.headers.contains("accept") && !accept_sse && !accept_json {
            let ok = send_json(&mut reader, 406, vec![], &error_response(None, &McpError::invalid_request("Accept must include application/json and text/event-stream")));
            return (ok && keep).then_some(reader);
        }

        let msg = match json::parse_bytes(&req.body) {
            Ok(v) => v,
            Err(e) => {
                let ok = send_json(&mut reader, 400, vec![], &error_response(None, &McpError::parse_error(format!("Invalid JSON: {e}"))));
                return (ok && keep).then_some(reader);
            }
        };
        let id = msg.get("id").filter(|v| !v.is_null()).cloned();
        let method = msg["method"].as_str().map(String::from);

        // Notifications: accept with 202.
        if id.is_none() && method.is_some() {
            // On HTTP, closing a stream is the cancellation signal; explicit
            // notifications/cancelled for subscriptions are still honoured.
            let reply = self.core.handle_message(&msg, principal, Transport::Http, Arc::new(NullSink), CancelToken::new());
            let status = match reply {
                Reply::Response(body, status) => {
                    let ok = send_json(&mut reader, status.max(400), vec![], &body);
                    return (ok && keep).then_some(reader);
                }
                _ => 202,
            };
            let mut h: Vec<(&str, String)> = cors.to_vec();
            h.push(("Cache-Control", "no-store".into()));
            let ok = write_response(reader.get_mut(), status, &h, b"", keep).is_ok();
            return (ok && keep).then_some(reader);
        }

        // Standard header validation for requests.
        if let (Some(_), Some(m)) = (&id, &method) {
            if let Err(e) = self.validate_headers(&req.headers, &msg, m) {
                let ok = send_json(&mut reader, 400, vec![], &error_response(id.as_ref(), &e));
                return (ok && keep).then_some(reader);
            }
        }

        let meta = msg.at(&["params", "_meta"]);
        let wants_stream = method.as_deref() == Some("subscriptions/listen") || !meta["progressToken"].is_null() || !meta[META_LOG_LEVEL].is_null();
        if wants_stream && accept_sse {
            return self.respond_sse(msg, principal, reader, cors, &md_url);
        }

        // JSON response; watch for client disconnect to cancel work.
        let cancel = CancelToken::new();
        let done = Arc::new(AtomicBool::new(false));
        let monitor = reader.get_ref().tcp().try_clone().ok().map(|tcp| spawn_disconnect_monitor(tcp, cancel.clone(), done.clone()));
        let reply = self.core.handle_message(&msg, principal, Transport::Http, Arc::new(NullSink), cancel.clone());
        done.store(true, Ordering::SeqCst);
        if let Some(h) = monitor {
            let _ = h.join();
        }
        reader.get_ref().tcp().set_read_timeout(Some(Duration::from_secs(60))).ok();
        if cancel.is_cancelled() {
            return None;
        }
        match reply {
            Reply::Response(body, status) => {
                let mut extra = Vec::new();
                if status == 403 && body.at(&["error", "code"]).as_i64() == Some(FORBIDDEN) {
                    let scope = body.at(&["error", "data", "requiredScope"]).as_str().unwrap_or("steam:public").to_string();
                    extra.push(("WWW-Authenticate", Authenticator::challenge(&md_url, Some("insufficient_scope"), Some("Additional scope required"), Some(&scope))));
                }
                if status == 429 {
                    let ms = body.at(&["error", "data", "retryAfterMs"]).as_u64().unwrap_or(1000);
                    extra.push(("Retry-After", ms.div_ceil(1000).max(1).to_string()));
                }
                let ok = send_json(&mut reader, status, extra, &body);
                (ok && keep).then_some(reader)
            }
            Reply::NoResponse => {
                let ok = write_response(reader.get_mut(), 202, &cors.to_vec(), b"", keep).is_ok();
                (ok && keep).then_some(reader)
            }
            Reply::Subscription(sub) => {
                // Client did not accept SSE: a subscription cannot be served as JSON.
                sub.close();
                self.core.hub.cancel(&sub.id);
                let e = McpError::invalid_request("subscriptions/listen requires Accept: text/event-stream");
                let ok = send_json(&mut reader, 406, vec![], &error_response(id.as_ref(), &e));
                (ok && keep).then_some(reader)
            }
        }
    }

    fn respond_sse(&self, msg: Value, principal: Principal, reader: BufReader<Stream>, cors: &[(&str, String)], md_url: &str) -> Option<BufReader<Stream>> {
        let cfg = self.core.config.get();
        // SSE responses own the connection until they end.
        let stream = reader.into_inner();
        let tcp = stream.tcp().try_clone().ok();
        let shared = Arc::new(Mutex::new(stream));
        {
            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
            let mut headers: Vec<(&str, String)> = cors.to_vec();
            headers.push(("Link", format!("<{md_url}>; rel=\"oauth-protected-resource\"")));
            if start_sse(&mut *s, &headers).is_err() {
                return None;
            }
        }
        let sink = Arc::new(SseSink { stream: shared.clone(), closed: AtomicBool::new(false) });
        let cancel = CancelToken::new();
        let done = Arc::new(AtomicBool::new(false));
        let monitor = tcp.map(|t| spawn_disconnect_monitor(t, cancel.clone(), done.clone()));
        let reply = self.core.handle_message(&msg, principal, Transport::Http, sink.clone(), cancel.clone());
        match reply {
            Reply::Response(body, _) => {
                if !cancel.is_cancelled() {
                    sink.send(&body);
                }
            }
            Reply::NoResponse => {}
            Reply::Subscription(sub) => {
                let keepalive = Duration::from_secs(cfg.sse_keepalive_secs);
                loop {
                    if sub.wait(keepalive) {
                        break;
                    }
                    if cancel.is_cancelled() || !sub.keepalive() {
                        break;
                    }
                }
                // Client disconnect (or keep-alive failure) cancels the subscription.
                sub.close();
                self.core.hub.cancel(&sub.id);
            }
        }
        done.store(true, Ordering::SeqCst);
        if let Some(h) = monitor {
            let _ = h.join();
        }
        let s = shared.lock().unwrap_or_else(|e| e.into_inner());
        let _ = (&*s).tcp().shutdown(std::net::Shutdown::Write);
        None
    }

    /// Validates mirrored headers against the body (HeaderMismatch on failure).
    fn validate_headers(&self, h: &Headers, msg: &Value, method: &str) -> Result<(), McpError> {
        let check_plain = |name: &str, v: &str| -> Result<String, McpError> {
            decode_header_value(v).ok_or_else(|| McpError::header_mismatch(format!("{name} header contains invalid characters")))
        };
        // MCP-Protocol-Version
        let Some(pv) = h.get("mcp-protocol-version") else {
            return Err(McpError::header_mismatch("missing required MCP-Protocol-Version header"));
        };
        if let Some(body_pv) = msg.at(&["params", "_meta", crate::mcp::protocol::META_PROTOCOL_VERSION]).as_str() {
            if pv.trim() != body_pv {
                return Err(McpError::header_mismatch(format!("MCP-Protocol-Version header '{pv}' does not match body value '{body_pv}'")));
            }
        }
        // Mcp-Method
        match h.get("mcp-method") {
            None => return Err(McpError::header_mismatch("missing required Mcp-Method header")),
            Some(m) if m.trim() != method => return Err(McpError::header_mismatch(format!("Mcp-Method header '{m}' does not match body value '{method}'"))),
            _ => {}
        }
        // Mcp-Name
        let name_source = match method {
            "tools/call" | "prompts/get" => Some(msg.at(&["params", "name"])),
            "resources/read" => Some(msg.at(&["params", "uri"])),
            "tasks/get" | "tasks/update" | "tasks/cancel" => Some(msg.at(&["params", "taskId"])),
            _ => None,
        };
        if let Some(src) = name_source {
            let body_name = src.as_str().unwrap_or("");
            match h.get("mcp-name") {
                None => return Err(McpError::header_mismatch("missing required Mcp-Name header")),
                Some(v) => {
                    let decoded = check_plain("Mcp-Name", v.trim())?;
                    if decoded != body_name {
                        return Err(McpError::header_mismatch(format!("Mcp-Name header value '{decoded}' does not match body value '{body_name}'")));
                    }
                }
            }
        }
        // Mcp-Param-* from x-mcp-header annotations.
        if method == "tools/call" {
            if let Some(tool) = msg.at(&["params", "name"]).as_str().and_then(|n| self.core.find_tool(n)) {
                let args = msg.at(&["params", "arguments"]);
                for (prop, header) in tool.header_params() {
                    let hname = format!("mcp-param-{}", header.to_ascii_lowercase());
                    let body_v = &args[prop.as_str()];
                    let header_v = h.get(&hname);
                    match (body_v.is_null(), header_v) {
                        (true, None) => {}
                        (true, Some(_)) => return Err(McpError::header_mismatch(format!("Mcp-Param-{header} is present but '{prop}' is absent from the arguments"))),
                        (false, None) => return Err(McpError::header_mismatch(format!("missing Mcp-Param-{header} header for argument '{prop}'"))),
                        (false, Some(raw)) => {
                            let decoded = check_plain(&format!("Mcp-Param-{header}"), raw)?;
                            let matches = match body_v {
                                Value::Number(n) => decoded.trim().parse::<f64>().ok().is_some_and(|x| x == n.as_f64()),
                                Value::Bool(b) => decoded == if *b { "true" } else { "false" },
                                Value::String(s) => decoded == *s,
                                _ => false,
                            };
                            if !matches {
                                return Err(McpError::header_mismatch(format!("Mcp-Param-{header} header value '{decoded}' does not match argument '{prop}'")));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Watches a connection for EOF while a request is processed; a closed
/// connection cancels the request.
fn spawn_disconnect_monitor(tcp: TcpStream, cancel: CancelToken, done: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        tcp.set_read_timeout(Some(Duration::from_millis(200))).ok();
        let mut buf = [0u8; 1];
        while !done.load(Ordering::SeqCst) {
            match tcp.peek(&mut buf) {
                Ok(0) => {
                    cancel.cancel();
                    return;
                }
                Ok(_) => std::thread::sleep(Duration::from_millis(200)),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => {}
                Err(_) => {
                    cancel.cancel();
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_value_decoding() {
        assert_eq!(decode_header_value("us-west1").as_deref(), Some("us-west1"));
        assert_eq!(decode_header_value("=?base64?SGVsbG8sIOS4lueVjA==?=").as_deref(), Some("Hello, 世界"));
        assert_eq!(decode_header_value("=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?=").as_deref(), Some("=?base64?literal?="));
        assert!(decode_header_value("bad\u{1}").is_none());
        assert!(decode_header_value("=?base64?!!!?=").is_none());
        assert!(is_loopback_origin("http://localhost:3000"));
        assert!(is_loopback_origin("http://127.0.0.1"));
        assert!(!is_loopback_origin("https://evil.example"));
    }
}
