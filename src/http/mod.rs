//! HTTP/1.1 implemented from scratch: header model, message framing
//! (Content-Length and chunked), URL parsing and a TLS-capable stream type.

pub mod client;
pub mod server;

use std::io::{self, BufRead, Read, Write};
use std::net::TcpStream;

/// Case-insensitive, order-preserving header list.
#[derive(Clone, Debug, Default)]
pub struct Headers(pub Vec<(String, String)>);

impl Headers {
    pub fn new() -> Self {
        Headers(Vec::new())
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.0
            .iter()
            .filter(move |(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn push(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.0.push((name.into(), value.into()));
    }

    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        self.0.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        self.0.push((name.to_string(), value.into()));
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// True if a comma-separated header contains `token` (case-insensitive).
    pub fn has_token(&self, name: &str, token: &str) -> bool {
        self.get_all(name).flat_map(|v| v.split(',')).any(|t| {
            t.trim()
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case(token)
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// A byte stream that is either plain TCP or TLS (client or server side).
pub enum Stream {
    Plain(TcpStream),
    ClientTls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
    ServerTls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl Stream {
    pub fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::ClientTls(s) => &s.sock,
            Stream::ServerTls(s) => &s.sock,
        }
    }

    pub fn shutdown(&self) {
        let _ = self.tcp().shutdown(std::net::Shutdown::Both);
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::ClientTls(s) => match s.read(buf) {
                // Servers that close without close_notify are common; treat as EOF.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
                r => r,
            },
            Stream::ServerTls(s) => match s.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
                r => r,
            },
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::ClientTls(s) => s.write(buf),
            Stream::ServerTls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::ClientTls(s) => s.flush(),
            Stream::ServerTls(s) => s.flush(),
        }
    }
}

/// A parsed absolute URL (http/https only).
#[derive(Clone, Debug, PartialEq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    /// Path plus query, always starting with `/`.
    pub path_and_query: String,
    pub userinfo: Option<String>,
}

impl Url {
    pub fn parse(s: &str) -> Option<Url> {
        let (scheme, rest) = s.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        let default_port = match scheme.as_str() {
            "http" => 80,
            "https" => 443,
            _ => return None,
        };
        let (authority, path) = match rest.find(['/', '?', '#']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u.to_string()), h),
            None => (None, authority),
        };
        let (host, port) = if let Some(stripped) = hostport.strip_prefix('[') {
            let end = stripped.find(']')?;
            let host = &stripped[..end];
            let after = &stripped[end + 1..];
            let port = match after.strip_prefix(':') {
                Some(p) => p.parse().ok()?,
                None if after.is_empty() => default_port,
                None => return None,
            };
            (host.to_string(), port)
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse().ok()?),
                None => (hostport.to_string(), default_port),
            }
        };
        if host.is_empty() {
            return None;
        }
        let mut path = path.split('#').next().unwrap_or("/").to_string();
        if !path.starts_with('/') {
            path.insert(0, '/');
        }
        Some(Url {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
            path_and_query: path,
            userinfo,
        })
    }

    pub fn is_https(&self) -> bool {
        self.scheme == "https"
    }

    /// `host[:port]` suitable for the Host header (port omitted when default).
    pub fn host_header(&self) -> String {
        let default = if self.is_https() { 443 } else { 80 };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == default {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }

    pub fn origin(&self) -> String {
        format!("{}://{}", self.scheme, self.host_header())
    }

    pub fn path(&self) -> &str {
        self.path_and_query.split('?').next().unwrap_or("/")
    }

    pub fn query(&self) -> Option<&str> {
        self.path_and_query.split_once('?').map(|(_, q)| q)
    }
}

/// Reads a CRLF (or LF) terminated line, enforcing a maximum length.
pub fn read_line(r: &mut impl BufRead, max: usize) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    loop {
        let available = r.fill_buf()?;
        if available.is_empty() {
            return if buf.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof in line"))
            };
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(i) => {
                buf.extend_from_slice(&available[..i]);
                r.consume(i + 1);
                break;
            }
            None => {
                let n = available.len();
                buf.extend_from_slice(available);
                r.consume(n);
            }
        }
        if buf.len() > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    if buf.len() > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 header"))
}

/// Reads header lines until the blank line. Returns `None` on clean EOF
/// before any byte was read.
pub fn read_head(r: &mut impl BufRead, max_bytes: usize) -> io::Result<Option<(String, Headers)>> {
    let mut start = match read_line(r, max_bytes)? {
        Some(l) => l,
        None => return Ok(None),
    };
    // Robustness (RFC 9112 §2.2): ignore leading empty lines.
    let mut guard = 0;
    while start.is_empty() {
        guard += 1;
        if guard > 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many empty lines",
            ));
        }
        start = match read_line(r, max_bytes)? {
            Some(l) => l,
            None => return Ok(None),
        };
    }
    let mut headers = Headers::new();
    let mut total = start.len();
    loop {
        let line = read_line(r, max_bytes)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "eof in headers"))?;
        if line.is_empty() {
            break;
        }
        total += line.len();
        if total > max_bytes || headers.0.len() > 200 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "headers too large",
            ));
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            // Obsolete line folding is rejected (RFC 9112 §5.2).
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "obsolete header folding",
            ));
        }
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed header"))?;
        if k.is_empty() || k.bytes().any(|b| !is_tchar(b)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid header name",
            ));
        }
        headers.push(k, v.trim());
    }
    Ok(Some((start, headers)))
}

/// RFC 9110 `tchar`.
pub fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Body framing determined from headers.
#[derive(Debug, PartialEq)]
pub enum Framing {
    None,
    Length(usize),
    Chunked,
    UntilClose,
}

pub fn framing(headers: &Headers, is_response: bool) -> io::Result<Framing> {
    if let Some(te) = headers.get("transfer-encoding") {
        if te
            .split(',')
            .next_back()
            .map(|t| t.trim().eq_ignore_ascii_case("chunked"))
            == Some(true)
        {
            return Ok(Framing::Chunked);
        }
        return if is_response {
            Ok(Framing::UntilClose)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported transfer-encoding",
            ))
        };
    }
    let lengths: Vec<&str> = headers.get_all("content-length").collect();
    if let Some(first) = lengths.first() {
        if lengths.iter().any(|l| l != first) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "conflicting content-length",
            ));
        }
        let n: usize = first
            .trim()
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid content-length"))?;
        return Ok(if n == 0 {
            Framing::None
        } else {
            Framing::Length(n)
        });
    }
    Ok(if is_response {
        Framing::UntilClose
    } else {
        Framing::None
    })
}

/// Reads a message body according to `framing`, enforcing `limit` bytes.
pub fn read_body(r: &mut impl BufRead, framing: &Framing, limit: usize) -> io::Result<Vec<u8>> {
    let too_large = || io::Error::new(io::ErrorKind::InvalidData, "body exceeds limit");
    match framing {
        Framing::None => Ok(Vec::new()),
        Framing::Length(n) => {
            if *n > limit {
                return Err(too_large());
            }
            let mut buf = vec![0u8; *n];
            r.read_exact(&mut buf)?;
            Ok(buf)
        }
        Framing::UntilClose => {
            let mut buf = Vec::new();
            r.take(limit as u64 + 1).read_to_end(&mut buf)?;
            if buf.len() > limit {
                return Err(too_large());
            }
            Ok(buf)
        }
        Framing::Chunked => {
            let mut buf = Vec::new();
            loop {
                let line = read_line(r, 4096)?.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "eof in chunk size")
                })?;
                let size_str = line.split(';').next().unwrap_or("").trim();
                let size = usize::from_str_radix(size_str, 16)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
                if size == 0 {
                    // Trailers until blank line.
                    loop {
                        match read_line(r, 8192)? {
                            Some(l) if !l.is_empty() => continue,
                            _ => break,
                        }
                    }
                    return Ok(buf);
                }
                if buf.len() + size > limit {
                    return Err(too_large());
                }
                let start = buf.len();
                buf.resize(start + size, 0);
                r.read_exact(&mut buf[start..])?;
                let mut crlf = [0u8; 2];
                r.read_exact(&mut crlf)?;
                if &crlf != b"\r\n" {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "missing chunk terminator",
                    ));
                }
            }
        }
    }
}

/// Writes a single chunk in chunked transfer encoding.
pub fn write_chunk(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    write!(w, "{:X}\r\n", data.len())?;
    w.write_all(data)?;
    w.write_all(b"\r\n")
}

pub fn status_text(code: u16) -> &'static str {
    match code {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        417 => "Expectation Failed",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn url_parsing() {
        let u = Url::parse("https://api.steampowered.com/ISteamNews/GetNewsForApp/v2/?appid=570")
            .unwrap();
        assert_eq!(u.host, "api.steampowered.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.path(), "/ISteamNews/GetNewsForApp/v2/");
        assert_eq!(u.query(), Some("appid=570"));
        assert_eq!(u.host_header(), "api.steampowered.com");
        let u = Url::parse("http://user:pw@[::1]:8080").unwrap();
        assert_eq!(u.host, "::1");
        assert_eq!(u.port, 8080);
        assert_eq!(u.path_and_query, "/");
        assert_eq!(u.userinfo.as_deref(), Some("user:pw"));
        assert_eq!(u.host_header(), "[::1]:8080");
        assert!(Url::parse("ftp://x").is_none());
        assert!(Url::parse("http://:80/").is_none());
    }

    #[test]
    fn chunked_body() {
        let raw = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\nTrailer: x\r\n\r\n";
        let mut r = BufReader::new(&raw[..]);
        let body = read_body(&mut r, &Framing::Chunked, 1024).unwrap();
        assert_eq!(body, b"Wikipedia");
        let mut r = BufReader::new(&raw[..]);
        assert!(read_body(&mut r, &Framing::Chunked, 5).is_err());
    }

    #[test]
    fn head_parsing() {
        let raw =
            b"\r\nPOST /mcp HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\nAccept: a, b\r\n\r\nhi";
        let mut r = BufReader::new(&raw[..]);
        let (start, h) = read_head(&mut r, 8192).unwrap().unwrap();
        assert_eq!(start, "POST /mcp HTTP/1.1");
        assert_eq!(h.get("content-length"), Some("2"));
        assert!(h.has_token("accept", "B"));
        assert_eq!(framing(&h, false).unwrap(), Framing::Length(2));
        let bad = b"GET / HTTP/1.1\r\nBad Header: x\r\n\r\n";
        assert!(read_head(&mut BufReader::new(&bad[..]), 8192).is_err());
        let conflict = b"GET / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n";
        let (_, h) = read_head(&mut BufReader::new(&conflict[..]), 8192)
            .unwrap()
            .unwrap();
        assert!(framing(&h, false).is_err());
    }
}
