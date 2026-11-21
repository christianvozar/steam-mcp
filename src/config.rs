//! Configuration: defaults, then a `KEY=VALUE` config file, then environment
//! variables, then command-line `--set KEY=VALUE` overrides (highest wins).
//!
//! The config file is watched; reloadable settings (TTLs, tool enablement,
//! log level, write enablement, page size) take effect without a restart and
//! trigger `list_changed` notifications where relevant.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::util::crypto::{random_bytes, sha256};

/// A secret string that never prints its value.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Secret {
        Secret(s.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    /// Short non-reversible fingerprint (for cache scoping and logs).
    pub fn fingerprint(&self) -> String {
        crate::util::crypto::hex(&sha256(self.0.as_bytes())[..6])
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret(***{})", self.fingerprint())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    /// No authentication (stdio, or HTTP bound to loopback for development).
    None,
    /// Pre-shared bearer tokens with per-token scopes.
    Static,
    /// OAuth 2.1 access tokens as JWTs (HS256 shared secret and/or RS256/ES256 via JWKS).
    Jwt,
    /// OAuth 2.0 Token Introspection (RFC 7662).
    Introspection,
}

#[derive(Clone, Debug)]
pub struct StaticToken {
    pub token_hash: [u8; 32],
    pub principal: String,
    pub scopes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct AuthConfig {
    pub mode: AuthMode,
    pub static_tokens: Vec<StaticToken>,
    pub jwt_hs256_secret: Option<Secret>,
    pub jwks_url: Option<String>,
    pub issuer: Option<String>,
    /// Accepted audiences; defaults to the canonical resource URL.
    pub audiences: Vec<String>,
    pub introspection_url: Option<String>,
    pub introspection_client_id: Option<String>,
    pub introspection_client_secret: Option<Secret>,
    pub authorization_servers: Vec<String>,
    pub clock_skew_secs: u64,
}

/// Cache TTL classes (seconds): (ttl, stale-while-revalidate window).
pub const TTL_CLASSES: &[(&str, u64, u64, &str)] = &[
    ("realtime", 60, 300, "live counters such as current player counts"),
    ("short", 900, 3600, "news, reviews, server lists, recently played"),
    ("medium", 3600, 6 * 3600, "store pages, profiles, owned games, partner app lists"),
    ("long", 6 * 3600, 24 * 3600, "achievement percentages, global stats"),
    ("static", 24 * 3600, 7 * 24 * 3600, "game schemas, the app catalog, API lists"),
    ("partner_live", 300, 900, "partner build, beta and depot data"),
    ("financial", 3600, 6 * 3600, "partner financial reports"),
    ("volatile", 30, 60, "ownership checks, transactions, inventories"),
];

#[derive(Clone, Debug)]
pub struct Config {
    // Steam credentials and endpoints.
    pub steam_api_key: Option<Secret>,
    pub publisher_key: Option<Secret>,
    pub financial_key: Option<Secret>,
    pub api_base: String,
    pub partner_base: String,
    pub store_base: String,
    pub default_cc: String,
    pub default_language: String,
    pub upstream_rps: f64,
    pub upstream_burst: f64,
    pub daily_budget: u64,
    pub upstream_timeout_secs: u64,
    pub upstream_retries: u32,
    pub ca_file: Option<String>,
    pub use_proxy_env: bool,

    // Cache.
    pub cache_max_bytes: usize,
    pub cache_max_entries: usize,
    pub cache_file: Option<PathBuf>,
    pub cache_persist_interval_secs: u64,
    pub max_stale_on_error_secs: u64,
    pub ttl: BTreeMap<String, (u64, u64)>,

    // MCP behaviour.
    pub page_size: usize,
    pub enable_writes: bool,
    pub disabled_tools: Vec<String>,
    pub task_sync_window_ms: u64,
    pub task_ttl_secs: u64,
    pub client_rps: f64,
    pub client_burst: f64,
    pub state_secret: Vec<u8>,
    pub watch_interval_secs: u64,
    pub log_level: String,
    pub log_json: bool,
    pub export_dir: Option<PathBuf>,

    // HTTP transport.
    pub http_bind: Option<String>,
    pub http_path: String,
    pub public_url: Option<String>,
    pub allowed_origins: Vec<String>,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    pub max_connections: usize,
    pub max_request_bytes: usize,
    pub sse_keepalive_secs: u64,

    pub auth: AuthConfig,

    pub config_file: Option<PathBuf>,
    pub stdio: bool,
}

/// Every recognised key with a description (used for `--help` and docs).
pub const KEYS: &[(&str, &str)] = &[
    ("STEAM_API_KEY", "Steam Web API user key (https://steamcommunity.com/dev/apikey)"),
    ("STEAM_PUBLISHER_KEY", "Steamworks publisher Web API key; enables partner tools via partner.steam-api.com"),
    ("STEAM_FINANCIAL_KEY", "Steamworks Financial API group key; enables IPartnerFinancialsService tools"),
    ("STEAM_MCP_API_BASE", "Base URL for the public Web API (default https://api.steampowered.com)"),
    ("STEAM_MCP_PARTNER_BASE", "Base URL for the partner Web API (default https://partner.steam-api.com)"),
    ("STEAM_MCP_STORE_BASE", "Base URL for the storefront API (default https://store.steampowered.com)"),
    ("STEAM_MCP_DEFAULT_CC", "Default store country code (default us)"),
    ("STEAM_MCP_DEFAULT_LANGUAGE", "Default language (default english)"),
    ("STEAM_MCP_UPSTREAM_RPS", "Upstream requests per second (default 10)"),
    ("STEAM_MCP_UPSTREAM_BURST", "Upstream burst size (default 20)"),
    ("STEAM_MCP_DAILY_BUDGET", "Upstream daily call budget; warnings past 80% (default 100000)"),
    ("STEAM_MCP_UPSTREAM_TIMEOUT", "Upstream request timeout seconds (default 30)"),
    ("STEAM_MCP_UPSTREAM_RETRIES", "Retries for transient upstream failures (default 2)"),
    ("STEAM_MCP_CA_FILE", "Extra PEM trust anchors for outbound TLS (also SSL_CERT_FILE)"),
    ("STEAM_MCP_USE_PROXY_ENV", "Honour HTTPS_PROXY/HTTP_PROXY/NO_PROXY (default true)"),
    ("STEAM_MCP_CACHE_MAX_BYTES", "Approximate cache memory budget (default 268435456)"),
    ("STEAM_MCP_CACHE_MAX_ENTRIES", "Maximum cache entries (default 100000)"),
    ("STEAM_MCP_CACHE_FILE", "Persist the cache to this JSONL file so restarts start warm"),
    ("STEAM_MCP_CACHE_PERSIST_INTERVAL", "Seconds between cache snapshots (default 60)"),
    ("STEAM_MCP_MAX_STALE_ON_ERROR", "Seconds an expired value may be served when Steam is failing (default 86400)"),
    ("STEAM_MCP_TTL_<CLASS>", "Override a TTL class as `ttl` or `ttl,stale` seconds; classes: realtime, short, medium, long, static, partner_live, financial, volatile"),
    ("STEAM_MCP_PAGE_SIZE", "Page size for MCP list results (default 100)"),
    ("STEAM_MCP_ENABLE_WRITES", "Enable state-changing partner tools such as SetAppBuildLive (default false)"),
    ("STEAM_MCP_DISABLED_TOOLS", "Comma-separated tool names or prefixes (ending in *) to hide"),
    ("STEAM_MCP_TASK_SYNC_WINDOW_MS", "How long a task-capable tool runs synchronously before returning a task (default 1500)"),
    ("STEAM_MCP_TASK_TTL", "Seconds tasks are retained (default 3600)"),
    ("STEAM_MCP_CLIENT_RPS", "Per-principal request rate limit (default 20)"),
    ("STEAM_MCP_CLIENT_BURST", "Per-principal burst (default 60)"),
    ("STEAM_MCP_STATE_SECRET", "Secret for integrity-protecting MRTR requestState; set the same value on every replica"),
    ("STEAM_MCP_WATCH_INTERVAL", "Seconds between subscription/config change checks (default 15)"),
    ("STEAM_MCP_LOG", "Log level: error, warn, info, debug, trace (default info)"),
    ("STEAM_MCP_LOG_FORMAT", "Log format: text or json (default text)"),
    ("STEAM_MCP_EXPORT_DIR", "Default directory for export_report when the client offers no roots"),
    ("STEAM_MCP_HTTP_BIND", "Serve Streamable HTTP on this address, e.g. 127.0.0.1:8080"),
    ("STEAM_MCP_HTTP_PATH", "MCP endpoint path (default /mcp)"),
    ("STEAM_MCP_PUBLIC_URL", "Canonical public URL of the MCP endpoint (OAuth resource identifier)"),
    ("STEAM_MCP_ALLOWED_ORIGINS", "Comma-separated allowed Origin values; `*` allows any (default: loopback origins)"),
    ("STEAM_MCP_TLS_CERT", "PEM certificate chain to serve HTTPS directly"),
    ("STEAM_MCP_TLS_KEY", "PEM private key to serve HTTPS directly"),
    ("STEAM_MCP_MAX_CONNECTIONS", "Maximum concurrent HTTP connections (default 256)"),
    ("STEAM_MCP_MAX_REQUEST_BYTES", "Maximum HTTP request body (default 4194304)"),
    ("STEAM_MCP_SSE_KEEPALIVE", "Seconds between SSE keep-alive comments (default 15)"),
    ("STEAM_MCP_AUTH_MODE", "none | static | jwt | introspection (default none)"),
    ("STEAM_MCP_AUTH_TOKENS", "Static tokens: `token=principal:scope1 scope2;token2=principal2:scope`"),
    ("STEAM_MCP_JWT_HS256_SECRET", "Shared secret for HS256 access tokens"),
    ("STEAM_MCP_JWKS_URL", "JWKS URL for RS256/ES256 access tokens"),
    ("STEAM_MCP_AUTH_ISSUER", "Required `iss` claim"),
    ("STEAM_MCP_AUTH_AUDIENCE", "Comma-separated accepted audiences (default: STEAM_MCP_PUBLIC_URL)"),
    ("STEAM_MCP_INTROSPECTION_URL", "RFC 7662 introspection endpoint"),
    ("STEAM_MCP_INTROSPECTION_CLIENT_ID", "Client id for introspection"),
    ("STEAM_MCP_INTROSPECTION_CLIENT_SECRET", "Client secret for introspection"),
    ("STEAM_MCP_AUTHORIZATION_SERVERS", "Comma-separated issuer URLs advertised in protected resource metadata"),
    ("STEAM_MCP_CLOCK_SKEW", "Allowed token clock skew seconds (default 60)"),
];

/// Raw key/value sources merged in precedence order.
#[derive(Clone, Debug, Default)]
pub struct Sources {
    pub file: Option<PathBuf>,
    pub cli: Vec<(String, String)>,
    pub stdio: bool,
    pub http_bind: Option<String>,
}

fn parse_kv_file(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(v);
            out.insert(k.trim().to_string(), v.to_string());
        }
    }
    out
}

fn bool_of(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

fn list_of(s: &str) -> Vec<String> {
    s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

impl Config {
    /// Builds a configuration from the given sources plus the process
    /// environment.
    pub fn load(sources: &Sources) -> Result<Config, String> {
        let mut kv: HashMap<String, String> = HashMap::new();
        if let Some(path) = &sources.file {
            let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read config file {}: {e}", path.display()))?;
            kv.extend(parse_kv_file(&text));
        }
        for (k, v) in std::env::vars() {
            if k.starts_with("STEAM_") || k == "SSL_CERT_FILE" {
                kv.insert(k, v);
            }
        }
        for (k, v) in &sources.cli {
            kv.insert(k.clone(), v.clone());
        }
        let mut cfg = Config::from_map(&kv)?;
        cfg.config_file = sources.file.clone();
        cfg.stdio = sources.stdio;
        if sources.http_bind.is_some() {
            cfg.http_bind = sources.http_bind.clone();
        }
        Ok(cfg)
    }

    /// Builds a configuration purely from a key/value map (no environment).
    pub fn from_map(kv: &HashMap<String, String>) -> Result<Config, String> {
        let get = |k: &str| kv.get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let secret = |k: &str| get(k).map(Secret::new);
        fn num<T: std::str::FromStr>(v: Option<String>, k: &str, d: T) -> Result<T, String> {
            match v {
                None => Ok(d),
                Some(s) => s.parse().map_err(|_| format!("invalid value for {k}: {s:?}")),
            }
        }
        let mut ttl = BTreeMap::new();
        for (class, t, s, _) in TTL_CLASSES {
            ttl.insert(class.to_string(), (*t, *s));
        }
        for (k, v) in kv {
            if let Some(class) = k.strip_prefix("STEAM_MCP_TTL_") {
                let class = class.to_ascii_lowercase();
                if !ttl.contains_key(&class) {
                    return Err(format!("unknown TTL class in {k}"));
                }
                let (t, s) = match v.split_once(',') {
                    Some((a, b)) => (a.trim().parse::<u64>(), b.trim().parse::<u64>()),
                    None => (v.trim().parse::<u64>(), Ok(ttl[&class].1)),
                };
                match (t, s) {
                    (Ok(t), Ok(s)) => {
                        ttl.insert(class, (t, s));
                    }
                    _ => return Err(format!("invalid value for {k}: {v:?} (expected `ttl` or `ttl,stale` seconds)")),
                }
            }
        }

        let state_secret = match get("STEAM_MCP_STATE_SECRET") {
            Some(s) => sha256(s.as_bytes()).to_vec(),
            None => {
                let mut b = vec![0u8; 32];
                random_bytes(&mut b);
                b
            }
        };

        let static_tokens = match get("STEAM_MCP_AUTH_TOKENS") {
            None => Vec::new(),
            Some(spec) => {
                let mut out = Vec::new();
                for item in spec.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                    let (token, rest) = item.split_once('=').ok_or_else(|| "STEAM_MCP_AUTH_TOKENS entries must be token=principal:scopes".to_string())?;
                    let (principal, scopes) = rest.split_once(':').unwrap_or((rest, ""));
                    if token.trim().len() < 16 {
                        return Err("static bearer tokens must be at least 16 characters".into());
                    }
                    out.push(StaticToken {
                        token_hash: sha256(token.trim().as_bytes()),
                        principal: principal.trim().to_string(),
                        scopes: scopes.split_whitespace().map(String::from).collect(),
                    });
                }
                out
            }
        };

        let public_url = get("STEAM_MCP_PUBLIC_URL").map(|s| s.trim_end_matches('/').to_string());
        let audiences = match get("STEAM_MCP_AUTH_AUDIENCE") {
            Some(a) => list_of(&a),
            None => public_url.iter().cloned().collect(),
        };
        let mode = match get("STEAM_MCP_AUTH_MODE").as_deref().map(str::to_ascii_lowercase).as_deref() {
            None | Some("none") => AuthMode::None,
            Some("static") => AuthMode::Static,
            Some("jwt") => AuthMode::Jwt,
            Some("introspection") => AuthMode::Introspection,
            Some(other) => return Err(format!("unknown STEAM_MCP_AUTH_MODE {other:?}")),
        };

        let cfg = Config {
            steam_api_key: secret("STEAM_API_KEY"),
            publisher_key: secret("STEAM_PUBLISHER_KEY"),
            financial_key: secret("STEAM_FINANCIAL_KEY"),
            api_base: get("STEAM_MCP_API_BASE").unwrap_or_else(|| "https://api.steampowered.com".into()).trim_end_matches('/').to_string(),
            partner_base: get("STEAM_MCP_PARTNER_BASE").unwrap_or_else(|| "https://partner.steam-api.com".into()).trim_end_matches('/').to_string(),
            store_base: get("STEAM_MCP_STORE_BASE").unwrap_or_else(|| "https://store.steampowered.com".into()).trim_end_matches('/').to_string(),
            default_cc: get("STEAM_MCP_DEFAULT_CC").unwrap_or_else(|| "us".into()).to_ascii_lowercase(),
            default_language: get("STEAM_MCP_DEFAULT_LANGUAGE").unwrap_or_else(|| "english".into()),
            upstream_rps: num(get("STEAM_MCP_UPSTREAM_RPS"), "STEAM_MCP_UPSTREAM_RPS", 10.0)?,
            upstream_burst: num(get("STEAM_MCP_UPSTREAM_BURST"), "STEAM_MCP_UPSTREAM_BURST", 20.0)?,
            daily_budget: num(get("STEAM_MCP_DAILY_BUDGET"), "STEAM_MCP_DAILY_BUDGET", 100_000)?,
            upstream_timeout_secs: num(get("STEAM_MCP_UPSTREAM_TIMEOUT"), "STEAM_MCP_UPSTREAM_TIMEOUT", 30)?,
            upstream_retries: num(get("STEAM_MCP_UPSTREAM_RETRIES"), "STEAM_MCP_UPSTREAM_RETRIES", 2)?,
            ca_file: get("STEAM_MCP_CA_FILE").or_else(|| get("SSL_CERT_FILE")),
            use_proxy_env: get("STEAM_MCP_USE_PROXY_ENV").map(|v| bool_of(&v)).unwrap_or(true),
            cache_max_bytes: num(get("STEAM_MCP_CACHE_MAX_BYTES"), "STEAM_MCP_CACHE_MAX_BYTES", 256 * 1024 * 1024)?,
            cache_max_entries: num(get("STEAM_MCP_CACHE_MAX_ENTRIES"), "STEAM_MCP_CACHE_MAX_ENTRIES", 100_000)?,
            cache_file: get("STEAM_MCP_CACHE_FILE").map(PathBuf::from),
            cache_persist_interval_secs: num(get("STEAM_MCP_CACHE_PERSIST_INTERVAL"), "STEAM_MCP_CACHE_PERSIST_INTERVAL", 60)?,
            max_stale_on_error_secs: num(get("STEAM_MCP_MAX_STALE_ON_ERROR"), "STEAM_MCP_MAX_STALE_ON_ERROR", 86_400)?,
            ttl,
            page_size: num::<usize>(get("STEAM_MCP_PAGE_SIZE"), "STEAM_MCP_PAGE_SIZE", 100)?.clamp(1, 1000),
            enable_writes: get("STEAM_MCP_ENABLE_WRITES").map(|v| bool_of(&v)).unwrap_or(false),
            disabled_tools: get("STEAM_MCP_DISABLED_TOOLS").map(|v| list_of(&v)).unwrap_or_default(),
            task_sync_window_ms: num(get("STEAM_MCP_TASK_SYNC_WINDOW_MS"), "STEAM_MCP_TASK_SYNC_WINDOW_MS", 1500)?,
            task_ttl_secs: num(get("STEAM_MCP_TASK_TTL"), "STEAM_MCP_TASK_TTL", 3600)?,
            client_rps: num(get("STEAM_MCP_CLIENT_RPS"), "STEAM_MCP_CLIENT_RPS", 20.0)?,
            client_burst: num(get("STEAM_MCP_CLIENT_BURST"), "STEAM_MCP_CLIENT_BURST", 60.0)?,
            state_secret,
            watch_interval_secs: num::<u64>(get("STEAM_MCP_WATCH_INTERVAL"), "STEAM_MCP_WATCH_INTERVAL", 15)?.max(1),
            log_level: get("STEAM_MCP_LOG").unwrap_or_else(|| "info".into()),
            log_json: get("STEAM_MCP_LOG_FORMAT").map(|v| v.eq_ignore_ascii_case("json")).unwrap_or(false),
            export_dir: get("STEAM_MCP_EXPORT_DIR").map(PathBuf::from),
            http_bind: get("STEAM_MCP_HTTP_BIND"),
            http_path: {
                let p = get("STEAM_MCP_HTTP_PATH").unwrap_or_else(|| "/mcp".into());
                if p.starts_with('/') { p } else { format!("/{p}") }
            },
            public_url,
            allowed_origins: get("STEAM_MCP_ALLOWED_ORIGINS").map(|v| list_of(&v)).unwrap_or_default(),
            tls_cert: get("STEAM_MCP_TLS_CERT"),
            tls_key: get("STEAM_MCP_TLS_KEY"),
            max_connections: num(get("STEAM_MCP_MAX_CONNECTIONS"), "STEAM_MCP_MAX_CONNECTIONS", 256)?,
            max_request_bytes: num(get("STEAM_MCP_MAX_REQUEST_BYTES"), "STEAM_MCP_MAX_REQUEST_BYTES", 4 * 1024 * 1024)?,
            sse_keepalive_secs: num::<u64>(get("STEAM_MCP_SSE_KEEPALIVE"), "STEAM_MCP_SSE_KEEPALIVE", 15)?.max(1),
            auth: AuthConfig {
                mode,
                static_tokens,
                jwt_hs256_secret: secret("STEAM_MCP_JWT_HS256_SECRET"),
                jwks_url: get("STEAM_MCP_JWKS_URL"),
                issuer: get("STEAM_MCP_AUTH_ISSUER"),
                audiences,
                introspection_url: get("STEAM_MCP_INTROSPECTION_URL"),
                introspection_client_id: get("STEAM_MCP_INTROSPECTION_CLIENT_ID"),
                introspection_client_secret: secret("STEAM_MCP_INTROSPECTION_CLIENT_SECRET"),
                authorization_servers: get("STEAM_MCP_AUTHORIZATION_SERVERS").map(|v| list_of(&v)).unwrap_or_default(),
                clock_skew_secs: num(get("STEAM_MCP_CLOCK_SKEW"), "STEAM_MCP_CLOCK_SKEW", 60)?,
            },
            config_file: None,
            stdio: false,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        match self.auth.mode {
            AuthMode::Static if self.auth.static_tokens.is_empty() => {
                return Err("STEAM_MCP_AUTH_MODE=static requires STEAM_MCP_AUTH_TOKENS".into());
            }
            AuthMode::Jwt if self.auth.jwt_hs256_secret.is_none() && self.auth.jwks_url.is_none() => {
                return Err("STEAM_MCP_AUTH_MODE=jwt requires STEAM_MCP_JWT_HS256_SECRET or STEAM_MCP_JWKS_URL".into());
            }
            AuthMode::Jwt if self.auth.audiences.is_empty() => {
                return Err("STEAM_MCP_AUTH_MODE=jwt requires STEAM_MCP_PUBLIC_URL or STEAM_MCP_AUTH_AUDIENCE for audience validation".into());
            }
            AuthMode::Introspection if self.auth.introspection_url.is_none() => {
                return Err("STEAM_MCP_AUTH_MODE=introspection requires STEAM_MCP_INTROSPECTION_URL".into());
            }
            _ => {}
        }
        if self.tls_cert.is_some() != self.tls_key.is_some() {
            return Err("STEAM_MCP_TLS_CERT and STEAM_MCP_TLS_KEY must be set together".into());
        }
        if self.auth.mode != AuthMode::None && self.auth.authorization_servers.is_empty() && self.auth.mode != AuthMode::Static {
            return Err("OAuth modes require STEAM_MCP_AUTHORIZATION_SERVERS for protected resource metadata".into());
        }
        Ok(())
    }

    /// TTL policy for a class (falls back to `medium`).
    pub fn policy(&self, class: &str) -> crate::cache::Policy {
        let (t, s) = self.ttl.get(class).copied().or_else(|| self.ttl.get("medium").copied()).unwrap_or((3600, 3600));
        crate::cache::Policy::new(t, s)
    }

    pub fn has_partner(&self) -> bool {
        self.publisher_key.is_some()
    }

    pub fn has_financial(&self) -> bool {
        self.financial_key.is_some()
    }

    /// A key usable for public keyed endpoints (user key, else publisher key).
    pub fn public_key(&self) -> Option<&Secret> {
        self.steam_api_key.as_ref().or(self.publisher_key.as_ref())
    }

    pub fn tool_disabled(&self, name: &str) -> bool {
        self.disabled_tools.iter().any(|d| match d.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => d == name,
        })
    }
}

/// Shared, hot-swappable configuration.
#[derive(Clone)]
pub struct ConfigHandle {
    inner: Arc<RwLock<Arc<Config>>>,
    sources: Arc<Sources>,
}

impl ConfigHandle {
    pub fn new(cfg: Config, sources: Sources) -> ConfigHandle {
        ConfigHandle { inner: Arc::new(RwLock::new(Arc::new(cfg))), sources: Arc::new(sources) }
    }

    pub fn get(&self) -> Arc<Config> {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Reloads from sources. Startup-only settings (bind address, cache sizes,
    /// credentials, auth) are preserved from the running configuration.
    pub fn reload(&self) -> Result<Arc<Config>, String> {
        let fresh = Config::load(&self.sources)?;
        let current = self.get();
        let merged = Config {
            ttl: fresh.ttl,
            page_size: fresh.page_size,
            enable_writes: fresh.enable_writes,
            disabled_tools: fresh.disabled_tools,
            task_sync_window_ms: fresh.task_sync_window_ms,
            log_level: fresh.log_level,
            default_cc: fresh.default_cc,
            default_language: fresh.default_language,
            ..(*current).clone()
        };
        let merged = Arc::new(merged);
        *self.inner.write().unwrap_or_else(|e| e.into_inner()) = merged.clone();
        Ok(merged)
    }

    pub fn sources(&self) -> &Sources {
        &self.sources
    }

    /// Replaces the configuration (tests).
    pub fn replace(&self, cfg: Config) {
        *self.inner.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(cfg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn defaults_and_overrides() {
        let c = Config::from_map(&map(&[("STEAM_MCP_TTL_REALTIME", "5,10"), ("STEAM_MCP_TTL_STATIC", "99"), ("STEAM_MCP_DISABLED_TOOLS", "steam_partner_*, cache_stats")])).unwrap();
        assert_eq!(c.ttl["realtime"], (5, 10));
        assert_eq!(c.ttl["static"].0, 99);
        assert_eq!(c.api_base, "https://api.steampowered.com");
        assert!(c.tool_disabled("steam_partner_get_app_builds"));
        assert!(c.tool_disabled("cache_stats"));
        assert!(!c.tool_disabled("steam_get_app_details"));
        assert!(Config::from_map(&map(&[("STEAM_MCP_TTL_BOGUS", "5")])).is_err());
        assert!(Config::from_map(&map(&[("STEAM_MCP_PAGE_SIZE", "abc")])).is_err());
    }

    #[test]
    fn auth_validation() {
        assert!(Config::from_map(&map(&[("STEAM_MCP_AUTH_MODE", "static")])).is_err());
        let c = Config::from_map(&map(&[("STEAM_MCP_AUTH_MODE", "static"), ("STEAM_MCP_AUTH_TOKENS", "0123456789abcdef0123=alice:steam:public steam:partner")])).unwrap();
        assert_eq!(c.auth.static_tokens[0].principal, "alice");
        assert_eq!(c.auth.static_tokens[0].scopes, vec!["steam:public", "steam:partner"]);
        assert!(Config::from_map(&map(&[("STEAM_MCP_AUTH_MODE", "jwt"), ("STEAM_MCP_JWT_HS256_SECRET", "s")])).is_err());
    }

    #[test]
    fn kv_file_parsing() {
        let m = parse_kv_file("# c\nexport A=\"1\"\nB = two \n\nC='x=y'\n");
        assert_eq!(m["A"], "1");
        assert_eq!(m["B"], "two");
        assert_eq!(m["C"], "x=y");
    }

    #[test]
    fn secret_debug_hides_value() {
        let s = Secret::new("supersecret");
        assert!(!format!("{s:?}").contains("supersecret"));
    }
}
