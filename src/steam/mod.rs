//! Steam upstream client.
//!
//! Every upstream call goes through [`SteamClient::call`], which:
//! 1. builds a cache key scoped by host, path, sorted parameters and a
//!    fingerprint of the API key class used (never the key itself);
//! 2. consults the shared [`Cache`] (fresh hit, stale-while-revalidate,
//!    coalescing, stale-if-error);
//! 3. on miss, waits on the upstream token bucket, enforces the daily budget,
//!    performs the HTTPS request with retries and classifies failures.

pub mod catalog;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::cache::{Cache, FetchError, FetchFn, Fetched, Lookup};
use crate::config::{Config, ConfigHandle};
use crate::http::client::{ClientOptions, HttpClient, Request};
use crate::json::{self, Value};
use crate::util::encoding::{build_query, pct_encode};
use crate::util::ratelimit::TokenBucket;

/// Which Steam host serves a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Host {
    /// `api.steampowered.com` — public Web API.
    Public,
    /// `partner.steam-api.com` — Steamworks partner Web API (publisher key).
    Partner,
    /// `store.steampowered.com` — storefront JSON endpoints.
    Store,
}

impl Host {
    pub fn as_str(&self) -> &'static str {
        match self {
            Host::Public => "api",
            Host::Partner => "partner",
            Host::Store => "store",
        }
    }
}

/// Which credential a call requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    /// No key is sent.
    None,
    /// The public key is sent when configured (some data is richer with it).
    Optional,
    /// A user or publisher key is required.
    Public,
    /// The Steamworks publisher key is required.
    Publisher,
    /// The Financial API group key is required.
    Financial,
}

impl KeyKind {
    pub fn scope_label(&self) -> &'static str {
        match self {
            KeyKind::None | KeyKind::Optional | KeyKind::Public => "public",
            KeyKind::Publisher => "partner",
            KeyKind::Financial => "financial",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// Classification of upstream failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Required credential not configured.
    NotConfigured,
    /// 401 — key invalid.
    Unauthorized,
    /// 403 — key lacks permission for this app/method.
    Forbidden,
    /// 404 / unknown resource.
    NotFound,
    /// 400 — Steam rejected the parameters.
    BadRequest,
    /// 429 or local budget exhaustion.
    RateLimited,
    /// 5xx or malformed response.
    Upstream,
    /// Connection / TLS / timeout.
    Network,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SteamError {
    pub kind: ErrorKind,
    pub message: String,
    pub status: Option<u16>,
}

impl std::fmt::Display for SteamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl SteamError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> SteamError {
        SteamError { kind, message: message.into(), status: None }
    }

    fn from_fetch(e: FetchError) -> SteamError {
        let kind = match e.status {
            Some(401) => ErrorKind::Unauthorized,
            Some(403) => ErrorKind::Forbidden,
            Some(404) => ErrorKind::NotFound,
            Some(400) => ErrorKind::BadRequest,
            Some(429) => ErrorKind::RateLimited,
            Some(0) => ErrorKind::NotConfigured,
            Some(_) => ErrorKind::Upstream,
            None if e.message.starts_with("rate limit") || e.message.contains("budget") => ErrorKind::RateLimited,
            None => ErrorKind::Network,
        };
        SteamError { kind, message: e.message, status: e.status.filter(|s| *s != 0) }
    }
}

/// Detects semantically-negative responses (e.g. `{"success": false}`).
pub type NegativeFn = fn(&Value) -> bool;

/// A single upstream call description.
#[derive(Clone, Debug)]
pub struct Call {
    pub host: Host,
    pub path: String,
    pub params: Vec<(String, String)>,
    pub key: KeyKind,
    pub ttl_class: &'static str,
    pub method: Method,
    pub negative: Option<NegativeFn>,
    /// Cache key namespace override (defaults to host+path).
    pub namespace: Option<String>,
}

impl Call {
    pub fn get(host: Host, path: &str, key: KeyKind, ttl_class: &'static str) -> Call {
        Call { host, path: path.to_string(), params: Vec::new(), key, ttl_class, method: Method::Get, negative: None, namespace: None }
    }

    pub fn post(host: Host, path: &str, key: KeyKind, ttl_class: &'static str) -> Call {
        Call { method: Method::Post, ..Call::get(host, path, key, ttl_class) }
    }

    pub fn param(mut self, k: &str, v: impl ToString) -> Call {
        let v = v.to_string();
        if !v.is_empty() {
            self.params.push((k.to_string(), v));
        }
        self
    }

    pub fn opt_param<T: ToString>(self, k: &str, v: Option<T>) -> Call {
        match v {
            Some(v) => self.param(k, v),
            None => self,
        }
    }

    pub fn negative_if(mut self, f: NegativeFn) -> Call {
        self.negative = Some(f);
        self
    }

    fn cache_key(&self, key_fp: &str) -> String {
        let mut params = self.params.clone();
        params.sort();
        let q = build_query(params.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let ns = self.namespace.clone().unwrap_or_else(|| format!("{}:{}", self.host.as_str(), self.path));
        format!("{ns}?{q}|{}:{key_fp}", self.key.scope_label())
    }
}

/// Upstream metrics.
#[derive(Default, Debug)]
pub struct UpstreamStats {
    pub calls: AtomicU64,
    pub errors: AtomicU64,
    pub retries: AtomicU64,
    pub throttled: AtomicU64,
    pub latency_ms_total: AtomicU64,
    pub by_host: [AtomicU64; 3],
}

struct Budget {
    day: u64,
    used: u64,
    warned: bool,
}

/// The Steam client shared by all tools/resources.
pub struct SteamClient {
    http: HttpClient,
    pub cache: Arc<Cache>,
    config: ConfigHandle,
    limiter: TokenBucket,
    budget: Mutex<Budget>,
    pub stats: UpstreamStats,
    pub catalog: catalog::CatalogIndex,
}

impl SteamClient {
    pub fn new(config: ConfigHandle, cache: Arc<Cache>) -> Result<Arc<SteamClient>, String> {
        let cfg = config.get();
        let http = HttpClient::new(ClientOptions {
            io_timeout: Duration::from_secs(cfg.upstream_timeout_secs.max(1)),
            extra_ca_file: cfg.ca_file.clone(),
            use_env_proxy: cfg.use_proxy_env,
            ..Default::default()
        })
        .map_err(|e| e.to_string())?;
        Ok(Arc::new(SteamClient {
            http,
            cache,
            limiter: TokenBucket::new(cfg.upstream_rps, cfg.upstream_burst),
            budget: Mutex::new(Budget { day: crate::util::time::now_secs() / 86_400, used: 0, warned: false }),
            config,
            stats: UpstreamStats::default(),
            catalog: catalog::CatalogIndex::default(),
        }))
    }

    pub fn config(&self) -> Arc<Config> {
        self.config.get()
    }

    pub fn config_handle(&self) -> &ConfigHandle {
        &self.config
    }

    /// Exposes the raw HTTP client for auxiliary fetches (JWKS, introspection).
    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    fn key_for(cfg: &Config, kind: KeyKind) -> Result<Option<crate::config::Secret>, SteamError> {
        let missing = |what: &str, var: &str| {
            Err(SteamError::new(ErrorKind::NotConfigured, format!("{what} is not configured; set {var} to use this capability")))
        };
        match kind {
            KeyKind::None => Ok(None),
            KeyKind::Optional => Ok(cfg.public_key().cloned()),
            KeyKind::Public => match cfg.public_key() {
                Some(k) => Ok(Some(k.clone())),
                None => missing("A Steam Web API key", "STEAM_API_KEY (or STEAM_PUBLISHER_KEY)"),
            },
            KeyKind::Publisher => match &cfg.publisher_key {
                Some(k) => Ok(Some(k.clone())),
                None => missing("A Steamworks publisher Web API key", "STEAM_PUBLISHER_KEY"),
            },
            KeyKind::Financial => match &cfg.financial_key {
                Some(k) => Ok(Some(k.clone())),
                None => missing("A Steamworks Financial API group key", "STEAM_FINANCIAL_KEY"),
            },
        }
    }

    /// Whether the credential for `kind` is configured.
    pub fn has_key(&self, kind: KeyKind) -> bool {
        Self::key_for(&self.config(), kind).map(|k| k.is_some() || matches!(kind, KeyKind::None | KeyKind::Optional)).unwrap_or(false)
    }

    /// The cache key a call would use (for resource change tracking).
    pub fn cache_key_for(&self, call: &Call) -> Option<String> {
        let cfg = self.config();
        let key = Self::key_for(&cfg, call.key).ok()?;
        Some(call.cache_key(&key.as_ref().map(|k| k.fingerprint()).unwrap_or_else(|| "-".into())))
    }

    /// Performs (or serves from cache) an upstream call.
    pub fn call(self: &Arc<Self>, call: &Call, force_refresh: bool) -> Result<Lookup, SteamError> {
        let cfg = self.config();
        let key = Self::key_for(&cfg, call.key)?;
        let fp = key.as_ref().map(|k| k.fingerprint()).unwrap_or_else(|| "-".into());
        let cache_key = call.cache_key(&fp);
        let policy = cfg.policy(call.ttl_class);
        let this = self.clone();
        let call_owned = call.clone();
        let fetch: FetchFn = Arc::new(move || this.fetch(&call_owned, key.as_ref()));
        self.cache.get_or_fetch(&cache_key, &policy, force_refresh, fetch).map_err(SteamError::from_fetch)
    }

    /// Like [`call`](Self::call) but never cached (one-shot verifications and writes).
    pub fn call_uncached(self: &Arc<Self>, call: &Call) -> Result<Value, SteamError> {
        let cfg = self.config();
        let key = Self::key_for(&cfg, call.key)?;
        match self.fetch(call, key.as_ref()) {
            Ok(Fetched::Value(v)) | Ok(Fetched::Negative(v)) => Ok(v),
            Err(e) => Err(SteamError::from_fetch(e)),
        }
    }

    fn base(&self, cfg: &Config, host: Host) -> String {
        match host {
            Host::Public => cfg.api_base.clone(),
            Host::Partner => cfg.partner_base.clone(),
            Host::Store => cfg.store_base.clone(),
        }
    }

    fn charge_budget(&self, cfg: &Config) -> Result<(), FetchError> {
        let mut b = self.budget.lock().unwrap_or_else(|e| e.into_inner());
        let today = crate::util::time::now_secs() / 86_400;
        if b.day != today {
            *b = Budget { day: today, used: 0, warned: false };
        }
        if cfg.daily_budget > 0 && b.used >= cfg.daily_budget {
            return Err(FetchError { message: "daily upstream call budget exhausted; serving cached data only".into(), status: None, transient: true });
        }
        b.used += 1;
        if cfg.daily_budget > 0 && !b.warned && b.used * 10 >= cfg.daily_budget * 8 {
            b.warned = true;
            crate::log_warn!("steam", "80% of the daily upstream budget used", used = b.used, budget = cfg.daily_budget);
        }
        Ok(())
    }

    pub fn budget_used(&self) -> u64 {
        self.budget.lock().map(|b| b.used).unwrap_or(0)
    }

    fn fetch(&self, call: &Call, key: Option<&crate::config::Secret>) -> Result<Fetched, FetchError> {
        let cfg = self.config();
        let mut params: Vec<(String, String)> = call.params.clone();
        if call.host != Host::Store {
            params.push(("format".into(), "json".into()));
        }
        if let Some(k) = key {
            params.push(("key".into(), k.expose().to_string()));
        }
        let query = build_query(params.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let base = self.base(&cfg, call.host);
        let path = if call.path.starts_with('/') { call.path.clone() } else { format!("/{}", call.path) };
        let req = match call.method {
            Method::Get => Request::get(format!("{base}{path}?{query}")),
            Method::Post => Request::post_form(format!("{base}{path}"), query),
        };
        let log_path = format!("{}{}", call.host.as_str(), path);
        let max_attempts = cfg.upstream_retries + 1;
        let mut attempt = 0;
        loop {
            attempt += 1;
            if !self.limiter.acquire(Duration::from_secs(15)) {
                self.stats.throttled.fetch_add(1, Ordering::Relaxed);
                return Err(FetchError { message: "rate limit: upstream token bucket exhausted".into(), status: None, transient: true });
            }
            self.charge_budget(&cfg)?;
            self.stats.calls.fetch_add(1, Ordering::Relaxed);
            self.stats.by_host[call.host as usize].fetch_add(1, Ordering::Relaxed);
            let started = Instant::now();
            let result = self.http.send(&req);
            let elapsed = started.elapsed().as_millis() as u64;
            self.stats.latency_ms_total.fetch_add(elapsed, Ordering::Relaxed);
            let (err, retry_after) = match result {
                Ok(resp) => {
                    crate::log_debug!("steam", "upstream response", path = log_path, status = resp.status, ms = elapsed, bytes = resp.body.len());
                    match resp.status {
                        200..=299 => {
                            if resp.body.iter().all(|b| b.is_ascii_whitespace()) {
                                return Ok(Fetched::Negative(Value::Null));
                            }
                            return match json::parse_bytes(&resp.body) {
                                Ok(v) => {
                                    if call.negative.is_some_and(|f| f(&v)) {
                                        Ok(Fetched::Negative(v))
                                    } else {
                                        Ok(Fetched::Value(v))
                                    }
                                }
                                Err(e) => {
                                    self.stats.errors.fetch_add(1, Ordering::Relaxed);
                                    Err(FetchError { message: format!("Steam returned a non-JSON response for {log_path}: {e}"), status: Some(502), transient: true })
                                }
                            };
                        }
                        404 => return Ok(Fetched::Negative(Value::Null)),
                        401 | 403 => {
                            self.stats.errors.fetch_add(1, Ordering::Relaxed);
                            let hint = if resp.status == 401 {
                                "the API key was rejected"
                            } else {
                                "the API key lacks access to this method or app (partner methods need a publisher key with permissions for the app)"
                            };
                            return Err(FetchError { message: format!("Steam returned {} for {log_path}: {hint}", resp.status), status: Some(resp.status), transient: false });
                        }
                        400 => {
                            self.stats.errors.fetch_add(1, Ordering::Relaxed);
                            let detail = crate::util::truncate(&crate::util::strip_markup(&resp.text()), 300);
                            return Err(FetchError { message: format!("Steam rejected the request parameters for {log_path} (400): {detail}"), status: Some(400), transient: false });
                        }
                        429 | 500..=599 => {
                            let ra = resp.headers.get("retry-after").and_then(|v| v.trim().parse::<u64>().ok());
                            (FetchError { message: format!("Steam returned {} for {log_path}", resp.status), status: Some(resp.status), transient: true }, ra)
                        }
                        other => {
                            self.stats.errors.fetch_add(1, Ordering::Relaxed);
                            return Err(FetchError { message: format!("unexpected HTTP {other} from Steam for {log_path}"), status: Some(other), transient: false });
                        }
                    }
                }
                Err(e) => (FetchError { message: format!("network error calling Steam ({log_path}): {e}"), status: None, transient: true }, None),
            };
            self.stats.errors.fetch_add(1, Ordering::Relaxed);
            if attempt >= max_attempts {
                crate::log_warn!("steam", "upstream call failed", path = log_path, error = err.message, attempts = attempt);
                return Err(err);
            }
            self.stats.retries.fetch_add(1, Ordering::Relaxed);
            let backoff = retry_after
                .map(|s| Duration::from_secs(s.min(30)))
                .unwrap_or_else(|| Duration::from_millis(400 * (1 << (attempt - 1)) + crate::util::crypto::random_u64() % 250));
            std::thread::sleep(backoff);
        }
    }

    pub fn stats_json(&self) -> Value {
        let s = &self.stats;
        let calls = s.calls.load(Ordering::Relaxed);
        crate::obj! {
            "upstreamCalls" => calls,
            "upstreamErrors" => s.errors.load(Ordering::Relaxed),
            "retries" => s.retries.load(Ordering::Relaxed),
            "throttled" => s.throttled.load(Ordering::Relaxed),
            "avgLatencyMs" => if calls == 0 { 0 } else { s.latency_ms_total.load(Ordering::Relaxed) / calls },
            "byHost" => crate::obj! {
                "api" => s.by_host[0].load(Ordering::Relaxed),
                "partner" => s.by_host[1].load(Ordering::Relaxed),
                "store" => s.by_host[2].load(Ordering::Relaxed),
            },
            "dailyBudget" => self.config().daily_budget,
            "dailyBudgetUsed" => self.budget_used(),
        }
    }
}

/// Validates a 64-bit SteamID (decimal). Accepts SteamID64 only.
pub fn parse_steamid(s: &str) -> Option<u64> {
    let s = s.trim();
    let id: u64 = s.parse().ok()?;
    // Individual accounts in the public universe: 0x0110000100000000 + account id.
    if (76_561_197_960_265_729..=76_561_202_255_233_023).contains(&id) { Some(id) } else { None }
}

/// URL-encodes a path segment (used for vanity names and store paths).
pub fn encode_segment(s: &str) -> String {
    pct_encode(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_are_stable_and_scoped() {
        let a = Call::get(Host::Public, "/X/Y/v1/", KeyKind::Optional, "short").param("b", 2).param("a", 1);
        let b = Call::get(Host::Public, "/X/Y/v1/", KeyKind::Optional, "short").param("a", 1).param("b", 2);
        assert_eq!(a.cache_key("fp"), b.cache_key("fp"));
        assert_ne!(a.cache_key("fp"), a.cache_key("other"));
        assert!(!a.cache_key("fp").contains("key="));
        let empty = Call::get(Host::Public, "/X", KeyKind::None, "short").param("x", "");
        assert!(empty.params.is_empty());
    }

    #[test]
    fn steamid_validation() {
        assert_eq!(parse_steamid("76561197960287930"), Some(76561197960287930));
        assert_eq!(parse_steamid("12345"), None);
        assert_eq!(parse_steamid("abc"), None);
    }
}
