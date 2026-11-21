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
    let mut out = String::with_capacity(s.len());
    let mut depth_angle = false;
    let mut depth_square = false;
    for c in s.chars() {
        match c {
            '<' => depth_angle = true,
            '>' if depth_angle => depth_angle = false,
            '[' => depth_square = true,
            ']' if depth_square => depth_square = false,
            _ if depth_angle || depth_square => {}
            _ => out.push(c),
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
