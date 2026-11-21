//! Base64 (RFC 4648, standard and URL-safe) and percent-encoding (RFC 3986).

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn encode_with(data: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(alphabet[(n >> 18) as usize & 63] as char);
        out.push(alphabet[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(alphabet[(n >> 6) as usize & 63] as char);
        } else if pad {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(alphabet[n as usize & 63] as char);
        } else if pad {
            out.push('=');
        }
    }
    out
}

/// Standard base64 with padding.
pub fn b64_encode(data: &[u8]) -> String {
    encode_with(data, STD, true)
}

/// URL-safe base64 without padding (as used by JWT).
pub fn b64url_encode(data: &[u8]) -> String {
    encode_with(data, URL, false)
}

/// Decodes standard or URL-safe base64, with or without padding.
/// Whitespace is ignored. Returns `None` for malformed input.
pub fn b64_decode(input: &str) -> Option<Vec<u8>> {
    let mut vals = Vec::with_capacity(input.len());
    let mut seen_pad = false;
    for c in input.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => {
                seen_pad = true;
                continue;
            }
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return None,
        };
        if seen_pad {
            return None; // data after padding
        }
        vals.push(v);
    }
    if vals.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for chunk in vals.chunks(4) {
        let mut n = 0u32;
        for (i, v) in chunk.iter().enumerate() {
            n |= (*v as u32) << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// Strict, canonical unpadded base64url (RFC 4648 §5) as used by JWTs and
/// sealed state: rejects padding, whitespace, the standard alphabet and
/// non-zero trailing bits, so each byte string has exactly one encoding.
pub fn b64url_decode_strict(input: &str) -> Option<Vec<u8>> {
    if !input
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        || input.len() % 4 == 1
    {
        return None;
    }
    let out = b64_decode(input)?;
    (b64url_encode(&out) == input).then_some(out)
}

fn is_unreserved(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~')
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Decodes percent-encoding; `plus_as_space` handles form encoding.
pub fn pct_decode(s: &str, plus_as_space: bool) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let h = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                if let Some(v) = h.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Builds `a=1&b=2` from pairs, percent-encoding keys and values.
pub fn build_query<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let mut out = String::new();
    for (k, v) in pairs {
        if !out.is_empty() {
            out.push('&');
        }
        out.push_str(&pct_encode(k));
        out.push('=');
        out.push_str(&pct_encode(v));
    }
    out
}

/// Parses `a=1&b=2` into decoded pairs.
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (pct_decode(k, true), pct_decode(v, true)),
            None => (pct_decode(p, true), String::new()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_vectors() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (raw, enc) in cases {
            assert_eq!(b64_encode(raw.as_bytes()), enc);
            assert_eq!(b64_decode(enc).unwrap(), raw.as_bytes());
            assert_eq!(
                b64_decode(&b64url_encode(raw.as_bytes())).unwrap(),
                raw.as_bytes()
            );
        }
        assert_eq!(b64_encode("Hello, 世界".as_bytes()), "SGVsbG8sIOS4lueVjA==");
        assert!(b64_decode("A").is_none());
        assert!(b64_decode("Zg==Zg").is_none());
        assert!(b64_decode("Z!").is_none());
        assert_eq!(b64url_decode_strict("Zm9vYg").unwrap(), b"foob");
        for bad in ["Zm9vYg==", "Zm9v Yg", "Zm9vYh", "Zm+v", "Z"] {
            assert!(b64url_decode_strict(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn percent() {
        assert_eq!(pct_encode("a b&c/é"), "a%20b%26c%2F%C3%A9");
        assert_eq!(pct_decode("a%20b%26c%2F%C3%A9", false), "a b&c/é");
        assert_eq!(pct_decode("a+b", true), "a b");
        assert_eq!(pct_decode("100%", false), "100%");
        assert_eq!(pct_decode("%zz", false), "%zz");
        let q = build_query([("key", "x y"), ("appid", "570")]);
        assert_eq!(q, "key=x%20y&appid=570");
        assert_eq!(
            parse_query(&q),
            vec![("key".into(), "x y".into()), ("appid".into(), "570".into())]
        );
    }
}
