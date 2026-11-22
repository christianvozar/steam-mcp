//! Dependency-free building blocks shared across the server.

pub mod crypto;
pub mod encoding;
pub mod log;
pub mod ratelimit;
pub mod regex;
pub mod signal;
pub mod time;
pub mod uritemplate;

/// A cooperative cancellation flag shared between a request and its workers.
#[derive(Clone, Default, Debug)]
pub struct CancelToken(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Truncates a string to at most `max` bytes on a char boundary, appending an
/// ellipsis when truncated.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Strips simple BBCode/HTML-ish markup that Steam embeds in news and reviews.
pub fn strip_markup(s: &str) -> String {
    // Block-level tags become word breaks; inline tags (b, i, url, …) do not.
    fn is_block(tag: &str) -> bool {
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '*')
            .collect::<String>()
            .to_ascii_lowercase();
        matches!(
            name.as_str(),
            "p" | "br"
                | "div"
                | "li"
                | "ul"
                | "ol"
                | "tr"
                | "td"
                | "table"
                | "title"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "hr"
                | "list"
                | "*"
                | "img"
                | "body"
                | "head"
                | "html"
        )
    }
    let mut out = String::with_capacity(s.len());
    let mut tag: Option<(char, String)> = None;
    for c in s.chars() {
        match (&mut tag, c) {
            (None, '<') | (None, '[') => {
                tag = Some((if c == '<' { '>' } else { ']' }, String::new()))
            }
            (Some((close, name)), c) if c == *close => {
                if is_block(name) {
                    out.push(' ');
                }
                tag = None;
            }
            (Some((_, name)), c) => {
                if name.len() < 32 {
                    name.push(c);
                }
            }
            (None, c) => out.push(c),
        }
    }
    let collapsed: Vec<&str> = out.split_whitespace().collect();
    collapsed.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_and_strip() {
        assert_eq!(truncate("héllo", 2), "h…");
        assert_eq!(
            strip_markup("<h1>Not Found</h1><p>Method x</p>"),
            "Not Found Method x"
        );
        assert_eq!(
            strip_markup("pre<i>fix</i> [list][*]one[*]two[/list]"),
            "prefix one two"
        );
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(
            strip_markup("[b]Hi[/b] <i>there</i>\n\n friend"),
            "Hi there friend"
        );
    }

    #[test]
    fn cancel_token() {
        let t = CancelToken::new();
        let t2 = t.clone();
        assert!(!t.is_cancelled());
        t2.cancel();
        assert!(t.is_cancelled());
    }
}
