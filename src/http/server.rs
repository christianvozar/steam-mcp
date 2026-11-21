//! Inbound HTTP/1.1 request parsing and response writing, shared by the MCP
//! Streamable HTTP transport and the test harness.

use std::io::{self, BufReader, Read, Write};
use std::net::SocketAddr;

use super::{Framing, Headers, framing, read_body, read_head, status_text};

/// A fully-read inbound request.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub target: String,
    pub path: String,
    pub query: Option<String>,
    pub version: String,
    pub headers: Headers,
    pub body: Vec<u8>,
    pub remote: Option<SocketAddr>,
}

impl HttpRequest {
    pub fn keep_alive(&self) -> bool {
        if self.headers.has_token("connection", "close") {
            return false;
        }
        self.version == "HTTP/1.1" || self.headers.has_token("connection", "keep-alive")
    }
}

/// Request-level failures mapped to an HTTP status.
#[derive(Debug)]
pub struct RequestError {
    pub status: u16,
    pub message: String,
}

impl RequestError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        RequestError { status, message: message.into() }
    }
}

/// Reads one request. `Ok(None)` means the peer closed the connection.
/// `Expect: 100-continue` is answered through the reader's inner stream.
pub fn read_request<S: Read + Write>(
    reader: &mut BufReader<S>,
    max_head: usize,
    max_body: usize,
) -> Result<Option<HttpRequest>, RequestError> {
    let (start, headers) = match read_head(reader, max_head) {
        Ok(Some(h)) => h,
        Ok(None) => return Ok(None),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            return Ok(None);
        }
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof || e.kind() == io::ErrorKind::ConnectionReset => {
            return Ok(None);
        }
        Err(e) => {
            let status = if e.to_string().contains("too large") || e.to_string().contains("too long") { 431 } else { 400 };
            return Err(RequestError::new(status, e.to_string()));
        }
    };
    let mut parts = start.split(' ');
    let (method, target, version) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(v), None) if !m.is_empty() && !t.is_empty() => (m, t, v),
        _ => return Err(RequestError::new(400, "malformed request line")),
    };
    if !method.bytes().all(super::is_tchar) {
        return Err(RequestError::new(400, "invalid method"));
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(RequestError::new(505, "unsupported HTTP version"));
    }
    if version == "HTTP/1.1" && !headers.contains("host") {
        return Err(RequestError::new(400, "missing Host header"));
    }
    let fr = framing(&headers, false).map_err(|e| RequestError::new(400, e.to_string()))?;
    if let Framing::Length(n) = fr {
        if n > max_body {
            return Err(RequestError::new(413, "request body too large"));
        }
    }
    if headers.has_token("expect", "100-continue") && fr != Framing::None {
        let w = reader.get_mut();
        let _ = w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").and_then(|_| w.flush());
    }
    let body = read_body(reader, &fr, max_body).map_err(|e| {
        if e.to_string().contains("limit") { RequestError::new(413, e.to_string()) } else { RequestError::new(400, e.to_string()) }
    })?;
    // Accept absolute-form targets by stripping the origin.
    let mut target = target.to_string();
    if let Some(rest) = target.strip_prefix("http://").or_else(|| target.strip_prefix("https://")) {
        target = match rest.find('/') {
            Some(i) => rest[i..].to_string(),
            None => "/".to_string(),
        };
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), Some(q.to_string())),
        None => (target.clone(), None),
    };
    Ok(Some(HttpRequest {
        method: method.to_string(),
        target,
        path,
        query,
        version: version.to_string(),
        headers,
        body,
        remote: None,
    }))
}

/// Writes a complete response with Content-Length.
pub fn write_response(
    w: &mut impl Write,
    status: u16,
    headers: &[(&str, String)],
    body: &[u8],
    keep_alive: bool,
) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", status_text(status));
    for (k, v) in headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(&v.replace(['\r', '\n'], " "));
        head.push_str("\r\n");
    }
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    head.push_str(if keep_alive { "Connection: keep-alive\r\n" } else { "Connection: close\r\n" });
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.write_all(body)?;
    w.flush()
}

/// Starts a `text/event-stream` response. The body is written as raw SSE
/// frames until the connection is closed (no chunked encoding).
pub fn start_sse(w: &mut impl Write, headers: &[(&str, String)]) -> io::Result<()> {
    let mut head = String::from("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache, no-transform\r\nX-Accel-Buffering: no\r\nConnection: close\r\n");
    for (k, v) in headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(&v.replace(['\r', '\n'], " "));
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.flush()
}

/// Writes one SSE `message` event carrying `data` (newlines are split into
/// multiple `data:` lines per the SSE specification).
pub fn write_sse_event(w: &mut impl Write, data: &str) -> io::Result<()> {
    let mut frame = String::from("event: message\n");
    for line in data.split('\n') {
        frame.push_str("data: ");
        frame.push_str(line);
        frame.push('\n');
    }
    frame.push('\n');
    w.write_all(frame.as_bytes())?;
    w.flush()
}

/// Writes an SSE comment, used as a keep-alive.
pub fn write_sse_comment(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b":\n\n")?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory duplex stream for tests.
    struct Duplex(std::io::Cursor<Vec<u8>>, Vec<u8>);
    impl Read for Duplex {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.0.read(b)
        }
    }
    impl Write for Duplex {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.1.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn duplex(raw: &[u8]) -> BufReader<Duplex> {
        BufReader::new(Duplex(std::io::Cursor::new(raw.to_vec()), Vec::new()))
    }

    #[test]
    fn parse_request() {
        let raw = b"POST http://localhost:8080/mcp?x=1 HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n";
        let mut r = duplex(raw);
        let req = read_request(&mut r, 8192, 1024).unwrap().unwrap();
        assert!(r.get_ref().1.starts_with(b"HTTP/1.1 100 Continue"));
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/mcp");
        assert_eq!(req.query.as_deref(), Some("x=1"));
        assert_eq!(req.body, b"{}");
        assert!(req.keep_alive());
        let raw = b"GET / HTTP/1.1\r\n\r\n";
        assert_eq!(read_request(&mut duplex(raw), 8192, 10).unwrap_err().status, 400);
        let raw = b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\n";
        assert_eq!(read_request(&mut duplex(raw), 8192, 10).unwrap_err().status, 413);
    }

    #[test]
    fn sse_framing() {
        let mut out = Vec::new();
        write_sse_event(&mut out, "a\nb").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "event: message\ndata: a\ndata: b\n\n");
    }
}
