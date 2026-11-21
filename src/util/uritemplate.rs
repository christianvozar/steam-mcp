//! RFC 6570 URI templates: expansion (levels 1–4 operators for simple
//! variables) and reverse matching for the level-1 `{var}` form that resource
//! templates use.

use std::collections::BTreeMap;

use super::encoding::pct_encode;

/// A parsed template segment.
#[derive(Debug, Clone, PartialEq)]
enum Part {
    Lit(String),
    Expr { op: Option<char>, vars: Vec<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct UriTemplate {
    raw: String,
    parts: Vec<Part>,
}

impl UriTemplate {
    pub fn parse(raw: &str) -> Option<UriTemplate> {
        let mut parts = Vec::new();
        let mut rest = raw;
        while let Some(start) = rest.find('{') {
            if start > 0 {
                parts.push(Part::Lit(rest[..start].to_string()));
            }
            let end = rest[start..].find('}')? + start;
            let inner = &rest[start + 1..end];
            let (op, names) = match inner.chars().next() {
                Some(c @ ('+' | '#' | '.' | '/' | ';' | '?' | '&')) => (Some(c), &inner[1..]),
                _ => (None, inner),
            };
            let vars: Vec<String> = names
                .split(',')
                .map(|v| {
                    v.trim_end_matches('*')
                        .split(':')
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
                .collect();
            if vars.iter().any(|v| {
                v.is_empty()
                    || !v
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
            }) {
                return None;
            }
            parts.push(Part::Expr { op, vars });
            rest = &rest[end + 1..];
        }
        if rest.contains('}') {
            return None;
        }
        if !rest.is_empty() {
            parts.push(Part::Lit(rest.to_string()));
        }
        Some(UriTemplate {
            raw: raw.to_string(),
            parts,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Variable names in order of appearance.
    pub fn variables(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for p in &self.parts {
            if let Part::Expr { vars, .. } = p {
                out.extend(vars.iter().map(|s| s.as_str()));
            }
        }
        out
    }

    /// Expands with the given variables (undefined variables are skipped).
    pub fn expand(&self, vars: &BTreeMap<String, String>) -> String {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                Part::Lit(s) => out.push_str(s),
                Part::Expr { op, vars: names } => {
                    let defined: Vec<(&String, &String)> = names
                        .iter()
                        .filter_map(|n| vars.get(n).map(|v| (n, v)))
                        .collect();
                    if defined.is_empty() {
                        continue;
                    }
                    let (first, sep, named, reserved) = match op {
                        None => ("", ",", false, false),
                        Some('+') => ("", ",", false, true),
                        Some('#') => ("#", ",", false, true),
                        Some('.') => (".", ".", false, false),
                        Some('/') => ("/", "/", false, false),
                        Some(';') => (";", ";", true, false),
                        Some('?') => ("?", "&", true, false),
                        Some('&') => ("&", "&", true, false),
                        Some(_) => ("", ",", false, false),
                    };
                    out.push_str(first);
                    for (i, (n, v)) in defined.iter().enumerate() {
                        if i > 0 {
                            out.push_str(sep);
                        }
                        let enc = if reserved {
                            encode_reserved(v)
                        } else {
                            pct_encode(v)
                        };
                        if named {
                            out.push_str(n);
                            if !(v.is_empty() && *op == Some(';')) {
                                out.push('=');
                                out.push_str(&enc);
                            }
                        } else {
                            out.push_str(&enc);
                        }
                    }
                }
            }
        }
        out
    }

    /// Matches a concrete URI against a template composed of literals and
    /// simple `{var}` expressions. Variables match one or more characters up
    /// to the next literal (and never across `/`).
    pub fn matches(&self, uri: &str) -> Option<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        let mut rest = uri;
        let mut i = 0;
        while i < self.parts.len() {
            match &self.parts[i] {
                Part::Lit(l) => {
                    rest = rest.strip_prefix(l.as_str())?;
                }
                Part::Expr { op: None, vars } if vars.len() == 1 => {
                    let stop = match self.parts.get(i + 1) {
                        Some(Part::Lit(next)) => rest.find(next.as_str())?,
                        None => rest.len(),
                        _ => return None,
                    };
                    let value = &rest[..stop];
                    if value.is_empty() || value.contains('/') {
                        return None;
                    }
                    out.insert(vars[0].clone(), super::encoding::pct_decode(value, false));
                    rest = &rest[stop..];
                }
                _ => return None,
            }
            i += 1;
        }
        if rest.is_empty() { Some(out) } else { None }
    }
}

fn encode_reserved(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~:/?#[]@!$&'()*+,;=".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn expand_rfc_examples() {
        let v = vars(&[
            ("var", "value"),
            ("hello", "Hello World!"),
            ("path", "/foo/bar"),
            ("x", "1024"),
            ("y", "768"),
        ]);
        let cases = [
            ("{var}", "value"),
            ("{hello}", "Hello%20World%21"),
            ("{+path}/here", "/foo/bar/here"),
            ("{#path}", "#/foo/bar"),
            ("map?{x,y}", "map?1024,768"),
            ("{?x,y}", "?x=1024&y=768"),
            ("{/var}", "/value"),
            ("{.var}", ".value"),
            ("{;x,y}", ";x=1024;y=768"),
            ("?a=1{&x}", "?a=1&x=1024"),
            ("{undefined}", ""),
        ];
        for (t, want) in cases {
            assert_eq!(
                UriTemplate::parse(t).unwrap().expand(&v),
                want,
                "template {t}"
            );
        }
    }

    #[test]
    fn match_simple() {
        let t = UriTemplate::parse("steam://app/{appid}/details").unwrap();
        assert_eq!(t.variables(), vec!["appid"]);
        let m = t.matches("steam://app/570/details").unwrap();
        assert_eq!(m["appid"], "570");
        assert!(t.matches("steam://app//details").is_none());
        assert!(t.matches("steam://app/5/7/details").is_none());
        assert!(t.matches("steam://app/570/news").is_none());
        let t2 = UriTemplate::parse("steam://partner/sales/{date}").unwrap();
        assert_eq!(
            t2.matches("steam://partner/sales/2026-07-01").unwrap()["date"],
            "2026-07-01"
        );
        assert!(UriTemplate::parse("bad{").is_none());
    }
}
