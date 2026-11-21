//! A small backtracking regular-expression engine for JSON Schema `pattern`.
//!
//! Supported (ECMA-262 subset): literals, `.`, anchors `^` `$`, classes
//! `[...]` / `[^...]` with ranges, escapes `\d \D \w \W \s \S` and escaped
//! metacharacters, groups `(...)` / `(?:...)`, alternation `|`, and greedy or
//! lazy quantifiers `* + ? {n} {n,} {n,m}`. Lookaround, backreferences and
//! word boundaries are rejected at compile time so callers can fall back.
//! Matching is bounded by a step budget to prevent catastrophic backtracking.

use std::cell::Cell;

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Char(char),
    Any,
    Class {
        ranges: Vec<(char, char)>,
        negated: bool,
    },
    Start,
    End,
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
        greedy: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    Syntax(String),
    Unsupported(String),
    /// The step budget was exhausted while matching.
    TooComplex,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Syntax(s) => write!(f, "invalid pattern: {s}"),
            Error::Unsupported(s) => write!(f, "unsupported pattern feature: {s}"),
            Error::TooComplex => f.write_str("pattern too complex to evaluate"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Regex {
    root: Node,
}

const MAX_STEPS: u64 = 200_000;
const MAX_REPEAT: u32 = 1000;

struct Parser<'a> {
    chars: &'a [char],
    i: usize,
}

fn digit() -> Vec<(char, char)> {
    vec![('0', '9')]
}
fn word() -> Vec<(char, char)> {
    vec![('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')]
}
fn space() -> Vec<(char, char)> {
    vec![
        (' ', ' '),
        ('\t', '\r'),
        ('\u{a0}', '\u{a0}'),
        ('\u{2028}', '\u{2029}'),
        ('\u{feff}', '\u{feff}'),
    ]
}

/// A parsed escape sequence.
enum Esc {
    Char(char),
    Set(Vec<(char, char)>),
    NegSet(Vec<(char, char)>),
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn alt(&mut self, depth: usize) -> Result<Node, Error> {
        if depth > 64 {
            return Err(Error::Syntax("nesting too deep".into()));
        }
        let mut branches = vec![self.concat(depth)?];
        while self.eat('|') {
            branches.push(self.concat(depth)?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().unwrap_or(Node::Concat(vec![]))
        } else {
            Node::Alt(branches)
        })
    }

    fn concat(&mut self, depth: usize) -> Result<Node, Error> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.atom(depth)?;
            let atom = self.quantifier(atom)?;
            items.push(atom);
        }
        Ok(Node::Concat(items))
    }

    fn number(&mut self) -> Option<u32> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        if start == self.i {
            return None;
        }
        self.chars[start..self.i]
            .iter()
            .collect::<String>()
            .parse()
            .ok()
    }

    fn quantifier(&mut self, atom: Node) -> Result<Node, Error> {
        let (min, max) = match self.peek() {
            Some('*') => {
                self.i += 1;
                (0, None)
            }
            Some('+') => {
                self.i += 1;
                (1, None)
            }
            Some('?') => {
                self.i += 1;
                (0, Some(1))
            }
            Some('{') => {
                let save = self.i;
                self.i += 1;
                match self.number() {
                    Some(n) => {
                        let max = if self.eat(',') {
                            self.number()
                        } else {
                            Some(n)
                        };
                        if !self.eat('}') {
                            return Err(Error::Syntax("unterminated {}".into()));
                        }
                        if max.is_some_and(|m| m < n) {
                            return Err(Error::Syntax("{n,m} with m < n".into()));
                        }
                        (n, max)
                    }
                    None => {
                        // A literal '{' (ECMA Annex B).
                        self.i = save;
                        return Ok(atom);
                    }
                }
            }
            _ => return Ok(atom),
        };
        if min > MAX_REPEAT || max.is_some_and(|m| m > MAX_REPEAT) {
            return Err(Error::Unsupported("repetition count too large".into()));
        }
        if matches!(atom, Node::Start | Node::End) {
            return Err(Error::Syntax("nothing to repeat".into()));
        }
        let greedy = !self.eat('?');
        Ok(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            greedy,
        })
    }

    fn escape(&mut self, in_class: bool) -> Result<Esc, Error> {
        let c = self
            .peek()
            .ok_or_else(|| Error::Syntax("trailing backslash".into()))?;
        self.i += 1;
        Ok(match c {
            'd' => Esc::Set(digit()),
            'w' => Esc::Set(word()),
            's' => Esc::Set(space()),
            'D' | 'W' | 'S' if in_class => {
                return Err(Error::Unsupported(
                    "negated shorthand inside a class".into(),
                ));
            }
            'D' => Esc::NegSet(digit()),
            'W' => Esc::NegSet(word()),
            'S' => Esc::NegSet(space()),
            'n' => Esc::Char('\n'),
            'r' => Esc::Char('\r'),
            't' => Esc::Char('\t'),
            'f' => Esc::Char('\u{0c}'),
            'v' => Esc::Char('\u{0b}'),
            '0' => Esc::Char('\0'),
            'b' if in_class => Esc::Char('\u{08}'),
            'b' | 'B' => return Err(Error::Unsupported("word boundaries".into())),
            'u' => {
                let hex: String = self
                    .chars
                    .get(self.i..self.i + 4)
                    .map(|s| s.iter().collect())
                    .unwrap_or_default();
                let v = u32::from_str_radix(&hex, 16)
                    .map_err(|_| Error::Syntax("bad \\u escape".into()))?;
                self.i += 4;
                Esc::Char(char::from_u32(v).ok_or_else(|| Error::Syntax("bad code point".into()))?)
            }
            c if c.is_ascii_digit() => return Err(Error::Unsupported("backreferences".into())),
            c if c.is_ascii_alphanumeric() => {
                return Err(Error::Unsupported(format!("escape \\{c}")));
            }
            c => Esc::Char(c),
        })
    }

    fn class(&mut self) -> Result<Node, Error> {
        let negated = self.eat('^');
        let mut ranges = Vec::new();
        let mut first = true;
        loop {
            let c = self
                .peek()
                .ok_or_else(|| Error::Syntax("unterminated class".into()))?;
            if c == ']' && !first {
                self.i += 1;
                break;
            }
            first = false;
            self.i += 1;
            let lo = if c == '\\' {
                match self.escape(true)? {
                    Esc::Char(ch) => ch,
                    Esc::Set(set) | Esc::NegSet(set) => {
                        ranges.extend(set);
                        continue;
                    }
                }
            } else {
                c
            };
            if self.peek() == Some('-') && self.chars.get(self.i + 1).is_some_and(|n| *n != ']') {
                self.i += 1;
                let hc = self
                    .peek()
                    .ok_or_else(|| Error::Syntax("unterminated range".into()))?;
                self.i += 1;
                let hi = if hc == '\\' {
                    match self.escape(true)? {
                        Esc::Char(ch) => ch,
                        _ => return Err(Error::Syntax("class escape in range".into())),
                    }
                } else {
                    hc
                };
                if hi < lo {
                    return Err(Error::Syntax("range out of order".into()));
                }
                ranges.push((lo, hi));
            } else {
                ranges.push((lo, lo));
            }
        }
        Ok(Node::Class { ranges, negated })
    }

    fn atom(&mut self, depth: usize) -> Result<Node, Error> {
        let c = self
            .peek()
            .ok_or_else(|| Error::Syntax("unexpected end".into()))?;
        self.i += 1;
        Ok(match c {
            '^' => Node::Start,
            '$' => Node::End,
            '.' => Node::Any,
            '[' => self.class()?,
            '(' => {
                if self.eat('?') && !self.eat(':') {
                    return Err(Error::Unsupported("lookaround or named groups".into()));
                }
                let inner = self.alt(depth + 1)?;
                if !self.eat(')') {
                    return Err(Error::Syntax("unbalanced parenthesis".into()));
                }
                inner
            }
            '\\' => match self.escape(false)? {
                Esc::Char(ch) => Node::Char(ch),
                Esc::Set(ranges) => Node::Class {
                    ranges,
                    negated: false,
                },
                Esc::NegSet(ranges) => Node::Class {
                    ranges,
                    negated: true,
                },
            },
            '*' | '+' | '?' => return Err(Error::Syntax("nothing to repeat".into())),
            c => Node::Char(c),
        })
    }
}

impl Regex {
    pub fn new(pattern: &str) -> Result<Regex, Error> {
        let chars: Vec<char> = pattern.chars().collect();
        let mut p = Parser {
            chars: &chars,
            i: 0,
        };
        let root = p.alt(0)?;
        if p.i != chars.len() {
            return Err(Error::Syntax("unbalanced parenthesis".into()));
        }
        Ok(Regex { root })
    }

    /// Unanchored search, as JSON Schema `pattern` requires.
    pub fn is_match(&self, text: &str) -> Result<bool, Error> {
        let input: Vec<char> = text.chars().collect();
        let steps = Cell::new(0u64);
        let m = Matcher {
            input: &input,
            steps: &steps,
        };
        for start in 0..=input.len() {
            if m.node(&self.root, start, &mut |_| true)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

struct Matcher<'a> {
    input: &'a [char],
    steps: &'a Cell<u64>,
}

type Cont<'c> = &'c mut dyn FnMut(usize) -> bool;

impl Matcher<'_> {
    fn tick(&self) -> Result<(), Error> {
        let s = self.steps.get() + 1;
        self.steps.set(s);
        if s > MAX_STEPS {
            Err(Error::TooComplex)
        } else {
            Ok(())
        }
    }

    fn node(&self, n: &Node, pos: usize, k: Cont) -> Result<bool, Error> {
        self.tick()?;
        match n {
            Node::Char(c) => Ok(self.input.get(pos) == Some(c) && k(pos + 1)),
            Node::Any => Ok(self
                .input
                .get(pos)
                .is_some_and(|c| *c != '\n' && *c != '\r')
                && k(pos + 1)),
            Node::Class { ranges, negated } => Ok(match self.input.get(pos) {
                Some(c) => {
                    (ranges.iter().any(|(lo, hi)| c >= lo && c <= hi) != *negated) && k(pos + 1)
                }
                None => false,
            }),
            Node::Start => Ok(pos == 0 && k(pos)),
            Node::End => Ok(pos == self.input.len() && k(pos)),
            Node::Concat(items) => self.seq(items, pos, k),
            Node::Alt(branches) => {
                for b in branches {
                    if self.node(b, pos, k)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Node::Repeat {
                node,
                min,
                max,
                greedy,
            } => self.repeat(node, *min, *max, *greedy, 0, pos, k),
        }
    }

    fn seq(&self, items: &[Node], pos: usize, k: Cont) -> Result<bool, Error> {
        match items.split_first() {
            None => Ok(k(pos)),
            Some((first, rest)) => {
                let mut err = None;
                let r = self.node(first, pos, &mut |p| match self.seq(rest, p, k) {
                    Ok(b) => b,
                    Err(e) => {
                        err = Some(e);
                        true
                    }
                })?;
                match err {
                    Some(e) => Err(e),
                    None => Ok(r),
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn repeat(
        &self,
        n: &Node,
        min: u32,
        max: Option<u32>,
        greedy: bool,
        count: u32,
        pos: usize,
        k: Cont,
    ) -> Result<bool, Error> {
        self.tick()?;
        let can_more = max.is_none_or(|m| count < m);
        let mut err = None;
        let mut more = |this: &Self, k: Cont| -> Result<bool, Error> {
            if !can_more {
                return Ok(false);
            }
            this.node(n, pos, &mut |p| {
                // Zero-width iterations would loop forever.
                if p == pos && count >= min {
                    return false;
                }
                match this.repeat(n, min, max, greedy, count + 1, p, k) {
                    Ok(b) => b,
                    Err(e) => {
                        err = Some(e);
                        true
                    }
                }
            })
        };
        // Short-circuit order is the point: greedy tries another repetition
        // before the continuation, lazy the reverse.
        #[allow(clippy::if_same_then_else)]
        let result = if count < min {
            more(self, k)?
        } else if greedy {
            more(self, k)? || k(pos)
        } else {
            k(pos) || more(self, k)?
        };
        match err {
            Some(e) => Err(e),
            None => Ok(result),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, s: &str) -> bool {
        Regex::new(p).unwrap().is_match(s).unwrap()
    }

    #[test]
    fn basics() {
        assert!(m("^[0-9]{17}$", "76561197960287930"));
        assert!(!m("^[0-9]{17}$", "7656119796028793"));
        assert!(!m("^[0-9]{17}$", "76561197960287930x"));
        assert!(m("abc", "xxabcxx"), "unanchored search");
        assert!(m("^a(b|cd)+e$", "abcdbe"));
        assert!(!m("^a(b|cd)+e$", "ae"));
        assert!(m("^\\d{4}-\\d{2}-\\d{2}$", "2026-07-28"));
        assert!(m("^[a-zA-Z_][\\w.-]*$", "steam_get.x-1"));
        assert!(!m("^[^0-9]+$", "ab3"));
        assert!(m("^colou?r$", "color") && m("^colou?r$", "colour"));
        assert!(m("^a{2,}$", "aaaa") && !m("^a{2,}$", "a"));
        assert!(m("^a{1,2}b$", "aab") && !m("^a{1,2}b$", "aaab"));
        assert!(m("^.+?x$", "abcx"));
        assert!(m("^\\s*$", " \t"));
        assert!(m("^(?:ab)*$", "ababab"));
        assert!(m("^\\$\\.$", "$."));
        assert!(m("^[\\-+]?\\d+$", "-42"));
        assert!(m("^[a-]$", "-"));
        assert!(m("^\\u00e9$", "é"));
        assert!(m("^(a*)*b$", "aaab"));
        assert!(m("^\\D+$", "abc") && !m("^\\D+$", "a1"));
        assert!(m("^\\S\\W\\S$", "a-b") && m("^\\S\\W\\S$", "a b"));
        assert!(!m("^\\S+$", "a b") && !m("^\\W$", "x"));
    }

    #[test]
    fn rejects_unsupported_and_invalid() {
        for p in [
            "(?=a)", "\\1", "\\bword", "[z-a]", "(", "a)", "*a", "a{3,1}", "[abc",
        ] {
            assert!(Regex::new(p).is_err(), "{p}");
        }
    }

    #[test]
    fn bounded_backtracking() {
        let r = Regex::new("^(a+)+$").unwrap();
        let evil = "a".repeat(40) + "!";
        assert_eq!(r.is_match(&evil), Err(Error::TooComplex));
    }
}
