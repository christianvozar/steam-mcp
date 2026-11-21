//! Protocol constants, error codes and per-request metadata (MCP 2026-07-28).

use crate::json::{Map, Value};
use crate::obj;

pub const PROTOCOL_VERSION: &str = "2026-07-28";
pub const SUPPORTED_VERSIONS: &[&str] = &[PROTOCOL_VERSION];
pub const JSONRPC_VERSION: &str = "2.0";

pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
pub const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
pub const META_LOG_LEVEL: &str = "io.modelcontextprotocol/logLevel";
pub const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
pub const META_SUBSCRIPTION_ID: &str = "io.modelcontextprotocol/subscriptionId";
pub const EXT_TASKS: &str = "io.modelcontextprotocol/tasks";

// JSON-RPC 2.0
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
// MCP-reserved range (-32020..-32099)
pub const HEADER_MISMATCH: i64 = -32020;
pub const MISSING_REQUIRED_CLIENT_CAPABILITY: i64 = -32021;
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;
// Application-defined (outside the JSON-RPC reserved range).
pub const RATE_LIMITED: i64 = -31029;
pub const UNAUTHORIZED: i64 = -31001;
pub const FORBIDDEN: i64 = -31003;

/// A JSON-RPC error with the HTTP status the Streamable HTTP binding should use.
#[derive(Clone, Debug, PartialEq)]
pub struct McpError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
    pub http_status: u16,
}

impl McpError {
    pub fn new(code: i64, message: impl Into<String>) -> McpError {
        McpError {
            code,
            message: message.into(),
            data: None,
            http_status: 200,
        }
    }
    pub fn with_data(mut self, data: Value) -> McpError {
        self.data = Some(data);
        self
    }
    pub fn http(mut self, status: u16) -> McpError {
        self.http_status = status;
        self
    }

    pub fn parse_error(msg: impl Into<String>) -> McpError {
        McpError::new(PARSE_ERROR, msg).http(400)
    }
    pub fn invalid_request(msg: impl Into<String>) -> McpError {
        McpError::new(INVALID_REQUEST, msg).http(400)
    }
    pub fn method_not_found(method: &str) -> McpError {
        McpError::new(METHOD_NOT_FOUND, format!("Method not found: {method}"))
            .with_data(obj! { "method" => method })
            .http(404)
    }
    pub fn invalid_params(msg: impl Into<String>) -> McpError {
        McpError::new(INVALID_PARAMS, msg)
    }
    pub fn internal(msg: impl Into<String>) -> McpError {
        McpError::new(INTERNAL_ERROR, msg).http(500)
    }
    pub fn unsupported_version(requested: &str) -> McpError {
        McpError::new(UNSUPPORTED_PROTOCOL_VERSION, "Unsupported protocol version")
            .with_data(
                obj! { "supported" => SUPPORTED_VERSIONS.to_vec(), "requested" => requested },
            )
            .http(400)
    }
    pub fn missing_capability(required: Value, why: &str) -> McpError {
        McpError::new(
            MISSING_REQUIRED_CLIENT_CAPABILITY,
            format!("Missing required client capability: {why}"),
        )
        .with_data(obj! { "requiredCapabilities" => required })
        .http(400)
    }
    pub fn header_mismatch(msg: impl Into<String>) -> McpError {
        McpError::new(HEADER_MISMATCH, format!("Header mismatch: {}", msg.into())).http(400)
    }
    pub fn rate_limited(retry_after_ms: u64) -> McpError {
        McpError::new(RATE_LIMITED, "Rate limit exceeded")
            .with_data(obj! { "retryAfterMs" => retry_after_ms })
            .http(429)
    }

    pub fn to_json(&self) -> Value {
        let mut e = obj! { "code" => self.code, "message" => self.message.clone() };
        if let Some(d) = &self.data {
            e.set("data", d.clone());
        }
        e
    }
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// Syslog severities used by `notifications/message`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug,
    Info,
    Notice,
    Warning,
    Error,
    Critical,
    Alert,
    Emergency,
}

impl LogLevel {
    pub fn parse(s: &str) -> Option<LogLevel> {
        Some(match s {
            "debug" => LogLevel::Debug,
            "info" => LogLevel::Info,
            "notice" => LogLevel::Notice,
            "warning" => LogLevel::Warning,
            "error" => LogLevel::Error,
            "critical" => LogLevel::Critical,
            "alert" => LogLevel::Alert,
            "emergency" => LogLevel::Emergency,
            _ => return None,
        })
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Notice => "notice",
            LogLevel::Warning => "warning",
            LogLevel::Error => "error",
            LogLevel::Critical => "critical",
            LogLevel::Alert => "alert",
            LogLevel::Emergency => "emergency",
        }
    }
}

/// Validated per-request metadata.
#[derive(Clone, Debug, Default)]
pub struct RequestMeta {
    pub protocol_version: String,
    pub client_info: Option<Value>,
    pub client_capabilities: Value,
    pub log_level: Option<LogLevel>,
    pub progress_token: Option<Value>,
    pub raw: Map,
}

impl RequestMeta {
    /// Parses and validates `params._meta` for a request.
    pub fn parse(params: Option<&Value>) -> Result<RequestMeta, McpError> {
        let meta = params.and_then(|p| p.get("_meta"));
        let Some(meta) = meta.and_then(|m| m.as_object()) else {
            return Err(McpError::invalid_params(format!(
                "Missing params._meta with required {META_PROTOCOL_VERSION} and {META_CLIENT_CAPABILITIES}"
            ))
            .http(400));
        };
        let version = match meta.get(META_PROTOCOL_VERSION) {
            Some(Value::String(v)) => v.clone(),
            Some(_) => {
                return Err(McpError::invalid_params(format!(
                    "{META_PROTOCOL_VERSION} must be a string"
                ))
                .http(400));
            }
            None => {
                return Err(McpError::invalid_params(format!(
                    "Missing required _meta field {META_PROTOCOL_VERSION}"
                ))
                .http(400));
            }
        };
        if !SUPPORTED_VERSIONS.contains(&version.as_str()) {
            return Err(McpError::unsupported_version(&version));
        }
        let caps = match meta.get(META_CLIENT_CAPABILITIES) {
            Some(v @ Value::Object(_)) => v.clone(),
            Some(_) => {
                return Err(McpError::invalid_params(format!(
                    "{META_CLIENT_CAPABILITIES} must be an object"
                ))
                .http(400));
            }
            None => {
                return Err(McpError::invalid_params(format!(
                    "Missing required _meta field {META_CLIENT_CAPABILITIES}"
                ))
                .http(400));
            }
        };
        let client_info = match meta.get(META_CLIENT_INFO) {
            None | Some(Value::Null) => None,
            Some(v @ Value::Object(_)) => Some(v.clone()),
            Some(_) => {
                return Err(McpError::invalid_params(format!(
                    "{META_CLIENT_INFO} must be an Implementation object"
                )));
            }
        };
        let log_level = match meta.get(META_LOG_LEVEL) {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(
                LogLevel::parse(s)
                    .ok_or_else(|| McpError::invalid_params(format!("Invalid log level: {s}")))?,
            ),
            Some(_) => {
                return Err(McpError::invalid_params(
                    "Invalid log level: must be a string",
                ));
            }
        };
        let progress_token = match meta.get("progressToken") {
            None | Some(Value::Null) => None,
            Some(v @ Value::String(_)) => Some(v.clone()),
            Some(v @ Value::Number(n)) if n.is_integer() => Some(v.clone()),
            Some(_) => {
                return Err(McpError::invalid_params(
                    "progressToken must be a string or integer",
                ));
            }
        };
        Ok(RequestMeta {
            protocol_version: version,
            client_info,
            client_capabilities: caps,
            log_level,
            progress_token,
            raw: meta.clone(),
        })
    }

    pub fn client_name(&self) -> String {
        self.client_info
            .as_ref()
            .and_then(|c| c["name"].as_str())
            .unwrap_or("unknown")
            .to_string()
    }
}

/// Our server's `Implementation` descriptor.
pub fn server_info() -> Value {
    obj! {
        "name" => "steam-mcp",
        "title" => "Steam MCP Server",
        "version" => env!("CARGO_PKG_VERSION"),
        "description" => "Cached access to the Steam Web API, storefront and Steamworks partner APIs: game, player, review, news, achievement, workshop, build, ownership, leaderboard, microtransaction and financial data.",
        "websiteUrl" => "https://github.com/christianvozar/steam-mcp",
        "icons" => crate::arr![server_icon()],
    }
}

/// A small, self-contained SVG gamepad glyph (data URI; no external fetch).
pub fn server_icon() -> Value {
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" rx="14" fill="#1b2838"/><path d="M18 24h28a10 10 0 0 1 9.6 12.8l-2.4 8.4A6 6 0 0 1 43 46.6L38 40H26l-5 6.6a6 6 0 0 1-10.2-1.4l-2.4-8.4A10 10 0 0 1 18 24z" fill="#66c0f4"/><circle cx="22" cy="32" r="3" fill="#1b2838"/><circle cx="42" cy="30" r="2.2" fill="#1b2838"/><circle cx="46" cy="34" r="2.2" fill="#1b2838"/></svg>"##;
    obj! {
        "src" => format!("data:image/svg+xml;base64,{}", crate::util::encoding::b64_encode(svg.as_bytes())),
        "mimeType" => "image/svg+xml",
        "sizes" => crate::arr!["any"],
    }
}

fn svg_icon(body: &str) -> Value {
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect width="24" height="24" rx="5" fill="#1b2838"/>{body}</svg>"##
    );
    obj! {
        "src" => format!("data:image/svg+xml;base64,{}", crate::util::encoding::b64_encode(svg.as_bytes())),
        "mimeType" => "image/svg+xml",
        "sizes" => crate::arr!["any"],
    }
}

/// Inline icon for a scope category (tools, resources and prompts use these).
/// Icons are self-contained data URIs, so clients never fetch third-party URLs.
pub fn scope_icon(scope: &str) -> Value {
    let stroke = r##"fill="none" stroke="#66c0f4" stroke-width="2" stroke-linecap="round" stroke-linejoin="round""##;
    match scope {
        // Bar chart: financial reporting.
        "steam:financial" => svg_icon(&format!(
            r##"<path {stroke} d="M6 18V11M12 18V6M18 18v-4"/>"##
        )),
        // Wrench: partner tooling.
        "steam:partner" => svg_icon(&format!(
            r##"<path {stroke} d="M14.5 6.5a3.5 3.5 0 0 0-4.6 4.6L5 16l3 3 4.9-4.9a3.5 3.5 0 0 0 4.6-4.6l-2 2-2-2z"/>"##
        )),
        // Pencil: state-changing operations.
        "steam:write" => svg_icon(&format!(
            r##"<path {stroke} d="M5 19l1-4L15 6l3 3-9 9zM13 8l3 3"/>"##
        )),
        // Gear-like dial: administration.
        "steam:admin" => svg_icon(&format!(
            r##"<circle {stroke} cx="12" cy="12" r="3"/><path {stroke} d="M12 4v2M12 18v2M4 12h2M18 12h2M6.3 6.3l1.4 1.4M16.3 16.3l1.4 1.4M6.3 17.7l1.4-1.4M16.3 7.7l1.4-1.4"/>"##
        )),
        // Globe: public data.
        _ => svg_icon(&format!(
            r##"<circle {stroke} cx="12" cy="12" r="7"/><path {stroke} d="M5 12h14M12 5c2 2 3 4.5 3 7s-1 5-3 7c-2-2-3-4.5-3-7s1-5 3-7z"/>"##
        )),
    }
}

/// Builds a JSON-RPC success response, adding `resultType` and serverInfo.
pub fn result_response(id: &Value, mut result: Value) -> Value {
    if let Value::Object(m) = &mut result {
        if !m.contains_key("resultType") {
            m.insert("resultType", "complete");
        }
        let meta = m.get_mut("_meta").and_then(|v| v.as_object_mut());
        match meta {
            Some(meta) => {
                if !meta.contains_key(META_SERVER_INFO) {
                    meta.insert(META_SERVER_INFO, server_info_brief());
                }
            }
            None => {
                m.insert("_meta", obj! { META_SERVER_INFO => server_info_brief() });
            }
        }
    }
    obj! { "jsonrpc" => JSONRPC_VERSION, "id" => id.clone(), "result" => result }
}

/// Name+version only, to keep per-result overhead small.
pub fn server_info_brief() -> Value {
    obj! { "name" => "steam-mcp", "version" => env!("CARGO_PKG_VERSION") }
}

/// Builds a JSON-RPC error response; `id` is omitted when unknown.
pub fn error_response(id: Option<&Value>, err: &McpError) -> Value {
    let mut m = Map::new();
    m.insert("jsonrpc", JSONRPC_VERSION);
    if let Some(id) = id {
        if !id.is_null() {
            m.insert("id", id.clone());
        }
    }
    m.insert("error", err.to_json());
    Value::Object(m)
}

/// Builds a notification message.
pub fn notification(method: &str, params: Value) -> Value {
    obj! { "jsonrpc" => JSONRPC_VERSION, "method" => method, "params" => params }
}

/// Validates a JSON-RPC request id (string or integer, never null).
pub fn valid_request_id(id: &Value) -> bool {
    match id {
        Value::String(_) => true,
        Value::Number(n) => n.is_integer(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    #[test]
    fn meta_validation() {
        let ok = parse(r#"{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/logLevel":"info","progressToken":7}}"#).unwrap();
        let m = RequestMeta::parse(Some(&ok)).unwrap();
        assert_eq!(m.log_level, Some(LogLevel::Info));
        assert_eq!(m.progress_token, Some(Value::from(7)));

        let missing =
            parse(r#"{"_meta":{"io.modelcontextprotocol/clientCapabilities":{}}}"#).unwrap();
        let e = RequestMeta::parse(Some(&missing)).unwrap_err();
        assert_eq!((e.code, e.http_status), (INVALID_PARAMS, 400));

        let bad_version = parse(r#"{"_meta":{"io.modelcontextprotocol/protocolVersion":"1900-01-01","io.modelcontextprotocol/clientCapabilities":{}}}"#).unwrap();
        let e = RequestMeta::parse(Some(&bad_version)).unwrap_err();
        assert_eq!(e.code, UNSUPPORTED_PROTOCOL_VERSION);
        assert_eq!(e.data.unwrap()["requested"].as_str(), Some("1900-01-01"));

        let bad_level = parse(r#"{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/logLevel":"loud"}}"#).unwrap();
        assert_eq!(
            RequestMeta::parse(Some(&bad_level)).unwrap_err().code,
            INVALID_PARAMS
        );
        assert!(RequestMeta::parse(None).is_err());
    }

    #[test]
    fn icons_are_data_uris() {
        for s in [
            "steam:public",
            "steam:partner",
            "steam:financial",
            "steam:write",
            "steam:admin",
        ] {
            let icon = scope_icon(s);
            let src = icon["src"].as_str().unwrap();
            assert!(src.starts_with("data:image/svg+xml;base64,"));
            let svg = crate::util::encoding::b64_decode(&src[26..]).unwrap();
            assert!(String::from_utf8(svg).unwrap().starts_with("<svg"));
        }
    }

    #[test]
    fn responses() {
        let r = result_response(&Value::from(1), obj! { "x" => 1 });
        assert_eq!(r["result"]["resultType"].as_str(), Some("complete"));
        assert_eq!(
            r["result"]["_meta"][META_SERVER_INFO]["name"].as_str(),
            Some("steam-mcp")
        );
        let e = error_response(None, &McpError::parse_error("bad"));
        assert!(e.get("id").is_none());
        assert!(valid_request_id(&Value::from("a")));
        assert!(!valid_request_id(&Value::Null));
        assert!(!valid_request_id(&Value::from(1.5)));
    }
}
