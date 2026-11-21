//! Tool framework and registry.
//!
//! Each tool declares its schema, annotations, required scope and credential,
//! and a handler. Handlers return a [`ToolOutcome`]: a final `CallToolResult`,
//! an MRTR `InputRequiredResult`, or a protocol error.

pub mod admin;
pub mod partner;
pub mod public;

use std::sync::Arc;

use super::context::RequestContext;
use super::protocol::McpError;
use super::server::Core;
use super::tasks::TaskHandle;
use crate::cache::Lookup;
use crate::json::{Map, Value};
use crate::obj;
use crate::steam::{ErrorKind, KeyKind, SteamError};

/// Vendor `_meta` key carrying cache provenance on results.
pub const META_CACHE: &str = "io.github.christianvozar/cache";

/// MRTR inputs supplied on a retry.
#[derive(Default)]
pub struct Mrtr {
    /// `inputResponses` from the retry, if any.
    pub responses: Option<Map>,
    /// Verified `requestState` payload, if any.
    pub state: Option<Value>,
}

impl Mrtr {
    pub fn response(&self, key: &str) -> Option<&Value> {
        self.responses.as_ref().and_then(|r| r.get(key))
    }
}

/// Handler environment.
pub struct ToolEnv<'a> {
    pub core: &'a Arc<Core>,
    pub ctx: &'a RequestContext,
    pub mrtr: Mrtr,
    /// Present when the call is executing as a task (tasks extension).
    pub task: Option<Arc<TaskHandle>>,
}

impl ToolEnv<'_> {
    pub fn steam(&self) -> &Arc<crate::steam::SteamClient> {
        &self.core.steam
    }

    pub fn cfg(&self) -> Arc<crate::config::Config> {
        self.core.config.get()
    }

    /// Reports progress via `notifications/progress` (synchronous calls) or
    /// the task status message (task execution).
    pub fn progress(&self, done: f64, total: Option<f64>, message: &str) {
        self.ctx.progress(done, total, Some(message));
        if let Some(t) = &self.task {
            t.set_status_message(message);
        }
    }

    pub fn cancelled(&self) -> bool {
        self.ctx.is_cancelled() || self.task.as_ref().is_some_and(|t| t.is_cancelled())
    }
}

/// What a handler produced.
pub enum ToolOutcome {
    /// A complete `CallToolResult` object.
    Done(Value),
    /// An MRTR interim result: input requests and a payload to seal into
    /// `requestState`.
    InputRequired { requests: Map, state: Option<Value> },
    /// A JSON-RPC protocol error.
    Error(McpError),
}

pub type Handler = fn(&ToolEnv, &Map) -> ToolOutcome;

/// Tool behaviour hints (MCP `ToolAnnotations`).
#[derive(Clone, Copy, Debug)]
pub struct Hints {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub open_world: bool,
}

pub const READ: Hints = Hints {
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: true,
};
pub const LOCAL: Hints = Hints {
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
};

/// A tool definition.
pub struct ToolDef {
    pub name: &'static str,
    pub title: &'static str,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub hints: Hints,
    /// Scope required to list and call the tool.
    pub scope: &'static str,
    /// Credential that must be configured for the tool to be offered.
    pub key: KeyKind,
    /// Eligible to run as a task when the client supports the extension.
    pub task: bool,
    /// State-changing tool, offered only with `STEAM_MCP_ENABLE_WRITES`.
    pub write: bool,
    pub handler: Handler,
}

impl ToolDef {
    /// The MCP `Tool` JSON.
    pub fn to_json(&self) -> Value {
        let mut t = obj! {
            "name" => self.name,
            "title" => self.title,
            "description" => self.description.as_str(),
            "inputSchema" => self.input_schema.clone(),
            "icons" => crate::arr![super::protocol::scope_icon(self.scope)],
            "annotations" => obj! {
                "title" => self.title,
                "readOnlyHint" => self.hints.read_only,
                "destructiveHint" => self.hints.destructive,
                "idempotentHint" => self.hints.idempotent,
                "openWorldHint" => self.hints.open_world,
            },
        };
        if let Some(o) = &self.output_schema {
            t.set("outputSchema", o.clone());
        }
        let mut meta = obj! { "io.github.christianvozar/scope" => self.scope };
        if self.task {
            meta.set("io.github.christianvozar/taskCapable", true);
        }
        t.set("_meta", meta);
        t
    }

    /// Top-level properties annotated with `x-mcp-header`: (property, header name).
    pub fn header_params(&self) -> Vec<(String, String)> {
        self.input_schema["properties"]
            .as_object()
            .map(|p| {
                p.iter()
                    .filter_map(|(k, v)| {
                        v["x-mcp-header"]
                            .as_str()
                            .map(|h| (k.clone(), h.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// All tools in deterministic order.
pub fn all_tools() -> Vec<ToolDef> {
    let mut v = public::tools();
    v.extend(partner::tools());
    v.extend(admin::tools());
    v
}

// ---------------------------------------------------------------------------
// Schema builders
// ---------------------------------------------------------------------------

pub fn object_schema(props: Vec<(&str, Value)>, required: &[&str]) -> Value {
    let mut p = Map::new();
    for (k, v) in props {
        p.insert(k, v);
    }
    let mut s = obj! { "type" => "object", "properties" => Value::Object(p), "additionalProperties" => false };
    if !required.is_empty() {
        s.set("required", required.to_vec());
    }
    s
}

pub fn p_appid() -> Value {
    obj! {
        "type" => "integer",
        "minimum" => 1,
        "maximum" => 4_294_967_295u64,
        "description" => "Steam application ID, e.g. 570 (Dota 2) or 730 (Counter-Strike 2). Use steam_search_apps to find IDs by name.",
        "x-mcp-header" => "AppId",
    }
}

pub fn p_appid_plain(desc: &str) -> Value {
    obj! { "type" => "integer", "minimum" => 1, "maximum" => 4_294_967_295u64, "description" => desc }
}

pub fn p_steamid() -> Value {
    obj! {
        "type" => "string",
        "pattern" => "^[0-9]{17}$",
        "minLength" => 17,
        "maxLength" => 17,
        "description" => "64-bit SteamID (17 digits, e.g. 76561197960287930). Use steam_resolve_vanity_url to convert a profile name.",
    }
}

pub fn p_steamids(max: u64) -> Value {
    obj! {
        "type" => "array",
        "items" => p_steamid(),
        "minItems" => 1,
        "maxItems" => max,
        "uniqueItems" => true,
        "description" => format!("Up to {max} 64-bit SteamIDs."),
    }
}

pub fn p_int(desc: &str, min: i64, max: i64, default: Option<i64>) -> Value {
    let mut v =
        obj! { "type" => "integer", "minimum" => min, "maximum" => max, "description" => desc };
    if let Some(d) = default {
        v.set("default", d);
    }
    v
}

pub fn p_u64(desc: &str) -> Value {
    obj! { "type" => "integer", "minimum" => 0, "description" => desc }
}

pub fn p_str(desc: &str) -> Value {
    obj! { "type" => "string", "description" => desc }
}

pub fn p_bool(desc: &str, default: bool) -> Value {
    obj! { "type" => "boolean", "default" => default, "description" => desc }
}

pub fn p_enum(desc: &str, values: &[&str], default: Option<&str>) -> Value {
    let mut v = obj! { "type" => "string", "enum" => values.to_vec(), "description" => desc };
    if let Some(d) = default {
        v.set("default", d);
    }
    v
}

pub fn p_cc() -> Value {
    obj! { "type" => "string", "minLength" => 2, "maxLength" => 2, "description" => "ISO 3166-1 alpha-2 store country code for pricing/availability (default from server config, usually 'us')." }
}

pub fn p_lang() -> Value {
    obj! { "type" => "string", "description" => "Steam language API name, e.g. english, german, schinese, japanese, koreana, brazilian (default from server config)." }
}

pub fn p_force() -> (&'static str, Value) {
    (
        "force_refresh",
        p_bool(
            "Bypass the cache and fetch fresh data from Steam. Use sparingly; cached data is usually sufficient.",
            false,
        ),
    )
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

pub fn arg_u64(args: &Map, k: &str) -> Option<u64> {
    args.get(k).and_then(|v| v.as_u64_lenient())
}

pub fn arg_i64(args: &Map, k: &str) -> Option<i64> {
    args.get(k).and_then(|v| v.as_i64())
}

pub fn arg_str<'a>(args: &'a Map, k: &str) -> Option<&'a str> {
    args.get(k)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub fn arg_bool(args: &Map, k: &str, default: bool) -> bool {
    args.get(k).and_then(|v| v.as_bool()).unwrap_or(default)
}

pub fn arg_list(args: &Map, k: &str) -> Vec<String> {
    match args.get(k) {
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| v.to_param_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        Some(v @ Value::Number(_)) => vec![v.to_string()],
        _ => Vec::new(),
    }
}

pub fn force(args: &Map) -> bool {
    arg_bool(args, "force_refresh", false)
}

pub fn cc(env: &ToolEnv, args: &Map) -> String {
    arg_str(args, "cc")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| env.cfg().default_cc.clone())
}

pub fn lang(env: &ToolEnv, args: &Map) -> String {
    arg_str(args, "language")
        .map(String::from)
        .unwrap_or_else(|| env.cfg().default_language.clone())
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// Cache provenance for one or more lookups.
pub fn cache_meta(lookups: &[&Lookup]) -> Value {
    let items: Vec<Value> = lookups
        .iter()
        .map(|l| {
            obj! {
                "source" => l.source.as_str(),
                "ageMs" => l.age_ms(),
                "ttlMs" => l.ttl_ms,
                "fetchedAt" => crate::util::time::iso8601_ms(l.fetched_at_ms),
            }
        })
        .collect();
    if items.len() == 1 {
        items.into_iter().next().unwrap_or_default()
    } else {
        Value::Array(items)
    }
}

/// A successful result: a human summary, the JSON as text (backwards
/// compatibility), `structuredContent`, and cache provenance in `_meta`.
pub fn ok_result(summary: impl Into<String>, data: Value, lookups: &[&Lookup]) -> ToolOutcome {
    let json_text = data.to_string();
    let mut r = obj! {
        "content" => crate::arr![
            obj! { "type" => "text", "text" => summary.into() },
            obj! { "type" => "text", "text" => json_text },
        ],
        "structuredContent" => data,
        "isError" => false,
    };
    if !lookups.is_empty() {
        r.set("_meta", obj! { META_CACHE => cache_meta(lookups) });
    }
    ToolOutcome::Done(r)
}

/// A tool execution error the model can act on (`isError: true`).
pub fn tool_error(msg: impl Into<String>) -> ToolOutcome {
    ToolOutcome::Done(obj! {
        "content" => crate::arr![obj! { "type" => "text", "text" => msg.into() }],
        "isError" => true,
    })
}

/// Maps an upstream error to an actionable tool error.
pub fn steam_error(e: SteamError) -> ToolOutcome {
    let advice = match e.kind {
        ErrorKind::NotConfigured => "",
        ErrorKind::Unauthorized => " Check that the configured key is valid.",
        ErrorKind::Forbidden => {
            " The key may not have permission for this app; partner methods require a publisher key from a Steamworks group with access to the app."
        }
        ErrorKind::NotFound => " Verify the ID exists and is visible to this key.",
        ErrorKind::BadRequest => " Adjust the arguments and retry.",
        ErrorKind::RateLimited => " Retry later; cached data will be served where available.",
        ErrorKind::Upstream => " Steam may be having issues; retry shortly.",
        ErrorKind::Network => " Steam could not be reached; retry shortly.",
    };
    tool_error(format!("{}{advice}", e.message))
}

/// Convenience: unwrap a steam result or return a tool error.
#[macro_export]
macro_rules! try_steam {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(err) => return $crate::mcp::tools::steam_error(err),
        }
    };
}

/// Returns a "not found" tool error for negative lookups.
pub fn not_found(what: &str) -> ToolOutcome {
    tool_error(format!(
        "{what} was not found or is not visible to the configured key."
    ))
}

/// Number formatting with thousands separators.
pub fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_well_formed() {
        let tools = all_tools();
        let mut names = std::collections::HashSet::new();
        for t in &tools {
            assert!(names.insert(t.name), "duplicate tool {}", t.name);
            assert!(
                t.name.len() <= 128
                    && t.name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c))
            );
            assert_eq!(
                t.input_schema["type"].as_str(),
                Some("object"),
                "{}",
                t.name
            );
            assert!(!t.description.is_empty());
            // x-mcp-header values must be valid tokens, unique, on primitive non-number types.
            let mut seen = std::collections::HashSet::new();
            for (prop, h) in t.header_params() {
                assert!(h.bytes().all(crate::http::is_tchar));
                assert!(seen.insert(h.to_ascii_lowercase()));
                let ty = t.input_schema["properties"][prop.as_str()]["type"]
                    .as_str()
                    .unwrap();
                assert!(matches!(ty, "integer" | "string" | "boolean"));
            }
            // Every schema must itself be self-consistent under our validator.
            assert!(
                super::super::schema::validate(&t.input_schema, &Value::object()).is_ok()
                    || t.input_schema.get("required").is_some(),
                "{}",
                t.name
            );
        }
        assert!(
            tools.len() >= 50,
            "expected a comprehensive tool surface, got {}",
            tools.len()
        );
    }

    #[test]
    fn number_formatting() {
        assert_eq!(fmt_num(0), "0");
        assert_eq!(fmt_num(1234567), "1,234,567");
        assert_eq!(fmt_num(100), "100");
    }
}
