//! Minimal structured logger writing to stderr.
//!
//! On the stdio transport, stdout is reserved for MCP messages, so all
//! diagnostics go to stderr. Lines are `ts level target: message key=value…`,
//! or JSON when `STEAM_MCP_LOG_FORMAT=json`.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            "trace" => Some(Level::Trace),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        }
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static JSON: AtomicBool = AtomicBool::new(false);

pub fn set_level(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

pub fn set_json(json: bool) {
    JSON.store(json, Ordering::Relaxed);
}

pub fn enabled(level: Level) -> bool {
    (level as u8) <= LEVEL.load(Ordering::Relaxed)
}

/// Emits one log line. Prefer the `log_*!` macros.
pub fn emit(level: Level, target: &str, msg: &str, fields: &[(&str, String)]) {
    if !enabled(level) {
        return;
    }
    let ts = super::time::iso8601_ms(super::time::now_ms());
    let line = if JSON.load(Ordering::Relaxed) {
        let mut m = crate::json::Map::new();
        m.insert("ts", ts);
        m.insert("level", level.as_str());
        m.insert("target", target);
        m.insert("msg", msg);
        for (k, v) in fields {
            m.insert(*k, v.as_str());
        }
        crate::json::Value::Object(m).to_string()
    } else {
        let mut s = format!("{ts} {:<5} {target}: {msg}", level.as_str());
        for (k, v) in fields {
            if v.contains(' ') || v.is_empty() {
                s.push_str(&format!(" {k}={v:?}"));
            } else {
                s.push_str(&format!(" {k}={v}"));
            }
        }
        s
    };
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    let _ = writeln!(lock, "{line}");
}

#[macro_export]
macro_rules! log_at {
    ($lvl:expr, $target:expr, $msg:expr $(, $k:ident = $v:expr)* $(,)?) => {
        if $crate::util::log::enabled($lvl) {
            $crate::util::log::emit($lvl, $target, &$msg, &[$((stringify!($k), format!("{}", $v))),*]);
        }
    };
}
#[macro_export]
macro_rules! log_error { ($($t:tt)*) => { $crate::log_at!($crate::util::log::Level::Error, $($t)*) }; }
#[macro_export]
macro_rules! log_warn { ($($t:tt)*) => { $crate::log_at!($crate::util::log::Level::Warn, $($t)*) }; }
#[macro_export]
macro_rules! log_info { ($($t:tt)*) => { $crate::log_at!($crate::util::log::Level::Info, $($t)*) }; }
#[macro_export]
macro_rules! log_debug { ($($t:tt)*) => { $crate::log_at!($crate::util::log::Level::Debug, $($t)*) }; }
