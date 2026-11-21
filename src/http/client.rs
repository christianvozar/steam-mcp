//! Outbound HTTP/1.1 client with TLS (rustls), keep-alive connection pooling,
//! HTTP CONNECT proxy support (`HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY`) and
//! optional extra trust anchors.

use std::collections::HashMap;
use std::io::{self, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{Framing, Headers, Stream, Url, framing, read_body, read_head};
use crate::util::encoding::b64_encode;

/// Errors surfaced by the client.
#[derive(Debug)]
pub enum HttpError {
    InvalidUrl(String),
    Connect(String),
    Tls(String),
    Io(io::Error),
    Protocol(String),
    Proxy(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::InvalidUrl(u) => write!(f, "invalid URL: {u}"),
            HttpError::Connect(e) => write!(f, "connect failed: {e}"),
            HttpError::Tls(e) => write!(f, "TLS error: {e}"),
            HttpError::Io(e) => write!(f, "I/O error: {e}"),
            HttpError::Protocol(e) => write!(f, "protocol error: {e}"),
            HttpError::Proxy(e) => write!(f, "proxy error: {e}"),
        }
    }
}

impl std::error::Error for HttpError {}

impl From<io::Error> for HttpError {
    fn from(e: io::Error) -> Self {
        HttpError::Io(e)
    }
}

/// An outbound request.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// Whether the request may be transparently retried on a stale pooled
    /// connection.
    pub idempotent: bool,
    /// Per-request timeout override.
    pub timeout: Option<Duration>,
}

impl Request {
    pub fn get(url: impl Into<String>) -> Request {
        Request {
            method: "GET".into(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            idempotent: true,
            timeout: None,
        }
    }

    pub fn post_form(url: impl Into<String>, form: String) -> Request {
        Request {
            method: "POST".into(),
            url: url.into(),
            headers: vec![(
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            )],
            body: Some(form.into_bytes()),
            idempotent: false,
            timeout: None,
        }
    }

    pub fn header(mut self, k: &str, v: &str) -> Request {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
}

/// A received response with fully-buffered body.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Response {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[derive(Clone, Debug)]
struct Proxy {
    url: Url,
}

#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub connect_timeout: Duration,
    pub io_timeout: Duration,
    pub max_body: usize,
    pub user_agent: String,
    /// PEM file of additional trust anchors (e.g. a corporate proxy CA).
    pub extra_ca_file: Option<String>,
    /// Honour proxy environment variables.
    pub use_env_proxy: bool,
    pub max_idle_per_host: usize,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            connect_timeout: Duration::from_secs(10),
            io_timeout: Duration::from_secs(30),
            max_body: 128 * 1024 * 1024,
            user_agent: format!("steam-mcp/{}", env!("CARGO_PKG_VERSION")),
            extra_ca_file: std::env::var("SSL_CERT_FILE").ok(),
            use_env_proxy: true,
            max_idle_per_host: 8,
        }
    }
}

struct Conn {
    reader: BufReader<Stream>,
    idle_since: Instant,
}

/// Thread-safe pooled HTTP client.
pub struct HttpClient {
    tls: Arc<rustls::ClientConfig>,
    pool: Mutex<HashMap<String, Vec<Conn>>>,
    opts: ClientOptions,
    https_proxy: Option<Proxy>,
    http_proxy: Option<Proxy>,
    no_proxy: Vec<String>,
}

fn env_any(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()))
}

impl HttpClient {
    pub fn new(opts: ClientOptions) -> Result<HttpClient, HttpError> {
        let mut roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.into(),
        };
        if let Some(path) = &opts.extra_ca_file {
            use rustls::pki_types::{CertificateDer, pem::PemObject};
            match CertificateDer::pem_file_iter(path) {
                Ok(iter) => {
                    let mut added = 0;
                    for cert in iter.flatten() {
                        if roots.add(cert).is_ok() {
                            added += 1;
                        }
                    }
                    crate::log_debug!(
                        "http",
                        "loaded extra trust anchors",
                        path = path,
                        count = added
                    );
                }
                Err(e) => {
                    crate::log_warn!("http", "could not read CA file", path = path, error = e)
                }
            }
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| HttpError::Tls(e.to_string()))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let (https_proxy, http_proxy, no_proxy) = if opts.use_env_proxy {
            let parse = |v: Option<String>| {
                v.and_then(|s| {
                    let s = if s.contains("://") {
                        s
                    } else {
                        format!("http://{s}")
                    };
                    Url::parse(&s).map(|url| Proxy { url })
                })
            };
            (
                parse(env_any(&[
                    "HTTPS_PROXY",
                    "https_proxy",
                    "ALL_PROXY",
                    "all_proxy",
                ])),
                parse(env_any(&[
                    "HTTP_PROXY",
                    "http_proxy",
                    "ALL_PROXY",
                    "all_proxy",
                ])),
                env_any(&["NO_PROXY", "no_proxy"])
                    .map(|v| {
                        v.split(',')
                            .map(|s| s.trim().to_ascii_lowercase())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        } else {
            (None, None, Vec::new())
        };
        Ok(HttpClient {
            tls: Arc::new(tls),
            pool: Mutex::new(HashMap::new()),
            opts,
            https_proxy,
            http_proxy,
            no_proxy,
        })
    }

    fn bypass_proxy(&self, host: &str) -> bool {
        if host == "localhost" || host.starts_with("127.") || host == "::1" {
            return true;
        }
        self.no_proxy.iter().any(|p| {
            if p == "*" {
                return true;
            }
            let p = p.trim_start_matches("*.").trim_start_matches('.');
            let p = p.split(':').next().unwrap_or(p);
            host == p || host.ends_with(&format!(".{p}"))
        })
    }

    fn proxy_for(&self, url: &Url) -> Option<&Proxy> {
        if self.bypass_proxy(&url.host) {
            return None;
        }
        if url.is_https() {
            self.https_proxy.as_ref()
        } else {
            self.http_proxy.as_ref()
        }
    }

    fn tcp_connect(
        &self,
        host: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<TcpStream, HttpError> {
        let addrs = (host, port)
            .to_socket_addrs()
            .map_err(|e| HttpError::Connect(format!("{host}: {e}")))?;
        let mut last = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, self.opts.connect_timeout) {
                Ok(s) => {
                    s.set_nodelay(true).ok();
                    s.set_read_timeout(Some(timeout)).ok();
                    s.set_write_timeout(Some(timeout)).ok();
                    return Ok(s);
                }
                Err(e) => last = Some(e),
            }
        }
        Err(HttpError::Connect(format!(
            "{host}:{port}: {}",
            last.map(|e| e.to_string())
                .unwrap_or_else(|| "no addresses".into())
        )))
    }

    fn connect(&self, url: &Url, timeout: Duration) -> Result<Stream, HttpError> {
        let proxy = self.proxy_for(url);
        let tcp = match proxy {
            Some(p) => self.tcp_connect(&p.url.host, p.url.port, timeout)?,
            None => self.tcp_connect(&url.host, url.port, timeout)?,
        };
        if !url.is_https() {
            return Ok(Stream::Plain(tcp));
        }
        let tcp = match proxy {
            Some(p) => self.connect_tunnel(tcp, p, url)?,
            None => tcp,
        };
        let server_name = rustls::pki_types::ServerName::try_from(url.host.clone())
            .map_err(|e| HttpError::Tls(format!("invalid server name {}: {e}", url.host)))?;
        let conn = rustls::ClientConnection::new(self.tls.clone(), server_name)
            .map_err(|e| HttpError::Tls(e.to_string()))?;
        let mut tls = rustls::StreamOwned::new(conn, tcp);
        // Complete the handshake eagerly so TLS failures surface as such.
        while tls.conn.is_handshaking() {
            tls.conn
                .complete_io(&mut tls.sock)
                .map_err(|e| HttpError::Tls(e.to_string()))?;
        }
        Ok(Stream::ClientTls(Box::new(tls)))
    }

    fn connect_tunnel(
        &self,
        mut tcp: TcpStream,
        proxy: &Proxy,
        url: &Url,
    ) -> Result<TcpStream, HttpError> {
        let target = format!(
            "{}:{}",
            if url.host.contains(':') {
                format!("[{}]", url.host)
            } else {
                url.host.clone()
            },
            url.port
        );
        let mut req = format!(
            "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nUser-Agent: {}\r\n",
            self.opts.user_agent
        );
        if let Some(ui) = &proxy.url.userinfo {
            let decoded = crate::util::encoding::pct_decode(ui, false);
            req.push_str(&format!(
                "Proxy-Authorization: Basic {}\r\n",
                b64_encode(decoded.as_bytes())
            ));
        }
        req.push_str("\r\n");
        tcp.write_all(req.as_bytes())?;
        let mut reader = BufReader::new(tcp);
        let (status_line, headers) = read_head(&mut reader, 64 * 1024)?
            .ok_or_else(|| HttpError::Proxy("proxy closed connection".into()))?;
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if status != 200 {
            return Err(HttpError::Proxy(format!(
                "CONNECT {target} returned {status_line}"
            )));
        }
        if !matches!(
            framing(&headers, true),
            Ok(Framing::None) | Ok(Framing::UntilClose)
        ) || !reader.buffer().is_empty()
        {
            return Err(HttpError::Proxy(
                "unexpected data after CONNECT response".into(),
            ));
        }
        Ok(reader.into_inner())
    }

    fn pool_key(url: &Url) -> String {
        format!("{}://{}:{}", url.scheme, url.host, url.port)
    }

    fn checkout(&self, key: &str) -> Option<Conn> {
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        let conns = pool.get_mut(key)?;
        while let Some(c) = conns.pop() {
            if c.idle_since.elapsed() < Duration::from_secs(30) {
                return Some(c);
            }
        }
        None
    }

    fn checkin(&self, key: String, mut conn: Conn) {
        conn.idle_since = Instant::now();
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        let conns = pool.entry(key).or_default();
        if conns.len() < self.opts.max_idle_per_host {
            conns.push(conn);
        }
    }

    /// Sends a request and buffers the full response.
    pub fn send(&self, req: &Request) -> Result<Response, HttpError> {
        let url = Url::parse(&req.url).ok_or_else(|| HttpError::InvalidUrl(req.url.clone()))?;
        let key = Self::pool_key(&url);
        let timeout = req.timeout.unwrap_or(self.opts.io_timeout);
        if let Some(mut conn) = self.checkout(&key) {
            conn.reader
                .get_ref()
                .tcp()
                .set_read_timeout(Some(timeout))
                .ok();
            conn.reader
                .get_ref()
                .tcp()
                .set_write_timeout(Some(timeout))
                .ok();
            match self.exchange(&mut conn, &url, req) {
                Ok((resp, reusable)) => {
                    if reusable {
                        self.checkin(key, conn);
                    }
                    return Ok(resp);
                }
                Err((err, retriable)) => {
                    if !(retriable || req.idempotent) {
                        return Err(err);
                    }
                    crate::log_debug!("http", "retrying on fresh connection", error = err);
                }
            }
        }
        let stream = self.connect(&url, timeout)?;
        let mut conn = Conn {
            reader: BufReader::with_capacity(64 * 1024, stream),
            idle_since: Instant::now(),
        };
        match self.exchange(&mut conn, &url, req) {
            Ok((resp, reusable)) => {
                if reusable {
                    self.checkin(key, conn);
                }
                Ok(resp)
            }
            Err((e, _)) => Err(e),
        }
    }

    /// Performs one request/response exchange. On error, the boolean says
    /// whether no part of the response was received (safe to retry).
    fn exchange(
        &self,
        conn: &mut Conn,
        url: &Url,
        req: &Request,
    ) -> Result<(Response, bool), (HttpError, bool)> {
        let via_http_proxy = !url.is_https() && self.proxy_for(url).is_some();
        let target = if via_http_proxy {
            format!("{}{}", url.origin(), url.path_and_query)
        } else {
            url.path_and_query.clone()
        };
        let mut head = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\n",
            req.method,
            target,
            url.host_header()
        );
        let has = |n: &str| req.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(n));
        if !has("user-agent") {
            head.push_str(&format!("User-Agent: {}\r\n", self.opts.user_agent));
        }
        if !has("accept") {
            head.push_str("Accept: application/json, */*;q=0.5\r\n");
        }
        if !has("accept-encoding") {
            head.push_str("Accept-Encoding: identity\r\n");
        }
        if via_http_proxy {
            if let Some(ui) = self.proxy_for(url).and_then(|p| p.url.userinfo.clone()) {
                let decoded = crate::util::encoding::pct_decode(&ui, false);
                head.push_str(&format!(
                    "Proxy-Authorization: Basic {}\r\n",
                    b64_encode(decoded.as_bytes())
                ));
            }
        }
        for (k, v) in &req.headers {
            if k.contains(['\r', '\n']) || v.contains(['\r', '\n']) {
                return Err((
                    HttpError::Protocol("header injection attempt".into()),
                    false,
                ));
            }
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        let body = req.body.as_deref().unwrap_or(&[]);
        if req.body.is_some() || matches!(req.method.as_str(), "POST" | "PUT" | "PATCH") {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("Connection: keep-alive\r\n\r\n");
        let w = conn.reader.get_mut();
        if let Err(e) = w
            .write_all(head.as_bytes())
            .and_then(|_| w.write_all(body))
            .and_then(|_| w.flush())
        {
            return Err((HttpError::Io(e), true));
        }
        // Read the response head, skipping interim 1xx responses.
        let (status, headers) = loop {
            let (status_line, headers) = match read_head(&mut conn.reader, 64 * 1024) {
                Ok(Some(h)) => h,
                Ok(None) => {
                    return Err((
                        HttpError::Protocol("connection closed before response".into()),
                        true,
                    ));
                }
                Err(e) => return Err((HttpError::Io(e), false)),
            };
            let mut parts = status_line.splitn(3, ' ');
            let version = parts.next().unwrap_or("");
            if !version.starts_with("HTTP/1.") {
                return Err((
                    HttpError::Protocol(format!("unexpected status line: {status_line}")),
                    false,
                ));
            }
            let status: u16 = match parts.next().and_then(|s| s.parse().ok()) {
                Some(s) => s,
                None => {
                    return Err((
                        HttpError::Protocol(format!("bad status: {status_line}")),
                        false,
                    ));
                }
            };
            if (100..200).contains(&status) && status != 101 {
                continue;
            }
            break (status, headers);
        };
        let fr = if req.method == "HEAD" || status == 204 || status == 304 {
            Framing::None
        } else {
            framing(&headers, true).map_err(|e| (HttpError::Io(e), false))?
        };
        let body = read_body(&mut conn.reader, &fr, self.opts.max_body)
            .map_err(|e| (HttpError::Io(e), false))?;
        let reusable = fr != Framing::UntilClose
            && !headers.has_token("connection", "close")
            && conn.reader.buffer().is_empty();
        Ok((
            Response {
                status,
                headers,
                body,
            },
            reusable,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read};
    use std::net::TcpListener;

    #[test]
    fn pooled_keepalive_and_chunked() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            // A single connection serving two requests proves pooling.
            let (s, _) = listener.accept().unwrap();
            let mut r = std::io::BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            for i in 0..2 {
                let mut line = String::new();
                loop {
                    line.clear();
                    r.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                if i == 0 {
                    w.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n").unwrap();
                } else {
                    w.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\nno")
                        .unwrap();
                }
            }
            let mut rest = Vec::new();
            let _ = r.read_to_end(&mut rest);
        });
        let client = HttpClient::new(ClientOptions {
            use_env_proxy: false,
            ..Default::default()
        })
        .unwrap();
        let r1 = client
            .send(&Request::get(format!("http://{addr}/a")))
            .unwrap();
        assert_eq!(r1.status, 200);
        assert_eq!(r1.body, b"abc");
        let r2 = client
            .send(&Request::get(format!("http://{addr}/b")))
            .unwrap();
        assert_eq!(r2.status, 404);
        assert_eq!(r2.text(), "no");
    }
}
