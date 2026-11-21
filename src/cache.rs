//! In-memory response cache tuned for read-heavy, slowly-changing data.
//!
//! Features:
//! * **TTL freshness** per entry, chosen by the caller's [`Policy`].
//! * **Stale-while-revalidate**: within the `stale` window after expiry a
//!   stale value is served immediately while a single background refresh runs.
//! * **Stale-if-error**: if upstream fails, an expired value up to
//!   `max_stale_on_error` old is served instead of an error.
//! * **Request coalescing** (single-flight): concurrent misses for the same key
//!   share one upstream fetch.
//! * **Negative caching**: "not found"-style results are cached briefly.
//! * **LRU eviction** bounded by approximate bytes and entry count.
//! * **Persistence**: periodic JSONL snapshots so a restart starts warm.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::json::{self, Value};
use crate::util::crypto::sha256_hex;
use crate::util::time::now_ms;

/// Freshness policy for one class of data.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Policy {
    /// How long a value is fresh.
    pub ttl: Duration,
    /// Additional window during which a stale value is served while a
    /// background refresh runs.
    pub stale: Duration,
    /// TTL used when the fetcher reports a negative (not-found) result.
    pub negative_ttl: Duration,
}

impl Policy {
    pub const fn new(ttl_secs: u64, stale_secs: u64) -> Policy {
        Policy {
            ttl: Duration::from_secs(ttl_secs),
            stale: Duration::from_secs(stale_secs),
            negative_ttl: Duration::from_secs(if ttl_secs < 60 { ttl_secs } else { 60 }),
        }
    }

    /// A policy that never caches (used for one-shot, per-user secrets such as
    /// auth ticket validation).
    pub const NONE: Policy = Policy { ttl: Duration::ZERO, stale: Duration::ZERO, negative_ttl: Duration::ZERO };

    pub fn is_none(&self) -> bool {
        self.ttl.is_zero()
    }
}

/// What a fetcher returns.
pub enum Fetched {
    /// A normal value, cached with the policy TTL.
    Value(Value),
    /// A negative result (e.g. unknown app), cached with `negative_ttl`.
    Negative(Value),
}

/// Fetch failure, carried to all coalesced waiters.
#[derive(Clone, Debug, PartialEq)]
pub struct FetchError {
    pub message: String,
    /// Upstream HTTP status if known.
    pub status: Option<u16>,
    /// Whether retrying later may succeed.
    pub transient: bool,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// How a lookup was satisfied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Fresh value from cache.
    Hit,
    /// Stale value served while revalidating in the background.
    Stale,
    /// Fetched from upstream by this caller.
    Miss,
    /// Waited on another caller's in-flight fetch.
    Coalesced,
    /// Upstream failed; an expired value was served.
    StaleOnError,
    /// Caching disabled for this policy.
    Bypass,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Hit => "hit",
            Source::Stale => "stale",
            Source::Miss => "miss",
            Source::Coalesced => "coalesced",
            Source::StaleOnError => "stale-on-error",
            Source::Bypass => "bypass",
        }
    }
}

/// Result of a cache lookup.
#[derive(Clone, Debug)]
pub struct Lookup {
    pub value: Arc<Value>,
    pub source: Source,
    pub fetched_at_ms: u64,
    pub ttl_ms: u64,
    pub negative: bool,
    /// SHA-256 of the serialized value (change detection / ETag).
    pub hash: Arc<str>,
}

impl Lookup {
    pub fn age_ms(&self) -> u64 {
        now_ms().saturating_sub(self.fetched_at_ms)
    }
    /// Milliseconds of freshness remaining (0 if stale).
    pub fn remaining_ttl_ms(&self) -> u64 {
        (self.fetched_at_ms + self.ttl_ms).saturating_sub(now_ms())
    }
}

#[derive(Debug)]
struct Entry {
    value: Arc<Value>,
    hash: Arc<str>,
    fetched_at_ms: u64,
    ttl_ms: u64,
    stale_ms: u64,
    negative: bool,
    size: usize,
    tick: u64,
}

impl Entry {
    fn lookup(&self, source: Source) -> Lookup {
        Lookup {
            value: self.value.clone(),
            source,
            fetched_at_ms: self.fetched_at_ms,
            ttl_ms: self.ttl_ms,
            negative: self.negative,
            hash: self.hash.clone(),
        }
    }
}

#[derive(Default)]
struct Inner {
    map: HashMap<String, Entry>,
    lru: BTreeMap<u64, String>,
    bytes: usize,
    tick: u64,
}

type FlightResult = Option<Result<Lookup, FetchError>>;

struct Flight {
    result: Mutex<FlightResult>,
    cv: Condvar,
}

/// Counters exposed via `cache_stats` and `/metrics`.
#[derive(Default, Debug)]
pub struct Stats {
    pub hits: AtomicU64,
    pub stale_hits: AtomicU64,
    pub misses: AtomicU64,
    pub coalesced: AtomicU64,
    pub refreshes: AtomicU64,
    pub fetch_errors: AtomicU64,
    pub stale_on_error: AtomicU64,
    pub evictions: AtomicU64,
    pub negative_hits: AtomicU64,
    pub persisted_entries: AtomicU64,
    pub loaded_entries: AtomicU64,
}

/// Cache configuration.
#[derive(Clone, Debug)]
pub struct CacheConfig {
    pub max_bytes: usize,
    pub max_entries: usize,
    /// How long past expiry an entry may still be served when upstream fails.
    pub max_stale_on_error: Duration,
    pub persist_path: Option<PathBuf>,
    pub persist_interval: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            max_bytes: 256 * 1024 * 1024,
            max_entries: 100_000,
            max_stale_on_error: Duration::from_secs(24 * 3600),
            persist_path: None,
            persist_interval: Duration::from_secs(60),
        }
    }
}

pub type FetchFn = Arc<dyn Fn() -> Result<Fetched, FetchError> + Send + Sync>;

pub struct Cache {
    inner: Mutex<Inner>,
    inflight: Mutex<HashMap<String, Arc<Flight>>>,
    refreshing: Mutex<std::collections::HashSet<String>>,
    pub stats: Stats,
    config: CacheConfig,
    dirty: AtomicBool,
}

impl Cache {
    pub fn new(config: CacheConfig) -> Arc<Cache> {
        let cache = Arc::new(Cache {
            inner: Mutex::new(Inner::default()),
            inflight: Mutex::new(HashMap::new()),
            refreshing: Mutex::new(Default::default()),
            stats: Stats::default(),
            config,
            dirty: AtomicBool::new(false),
        });
        if cache.config.persist_path.is_some() {
            cache.load();
        }
        cache
    }

    pub fn config(&self) -> &CacheConfig {
        &self.config
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Looks up a key without fetching, regardless of freshness (as long as it
    /// is within the stale-on-error horizon).
    pub fn peek(&self, key: &str) -> Option<Lookup> {
        let mut inner = self.lock();
        let now = now_ms();
        let horizon = self.config.max_stale_on_error.as_millis() as u64;
        let entry = inner.map.get(key)?;
        if now > entry.fetched_at_ms + entry.ttl_ms + entry.stale_ms.max(horizon) {
            return None;
        }
        let source = if now < entry.fetched_at_ms + entry.ttl_ms { Source::Hit } else { Source::Stale };
        let l = entry.lookup(source);
        Self::touch(&mut inner, key);
        Some(l)
    }

    fn touch(inner: &mut Inner, key: &str) {
        inner.tick += 1;
        let tick = inner.tick;
        if let Some(e) = inner.map.get_mut(key) {
            let old = std::mem::replace(&mut e.tick, tick);
            inner.lru.remove(&old);
            inner.lru.insert(tick, key.to_string());
        }
    }

    /// Stores a value directly (used by fetchers that produce several keys).
    pub fn put(&self, key: &str, value: Value, policy: &Policy, negative: bool) -> Lookup {
        let hash: Arc<str> = sha256_hex(value.to_string().as_bytes()).into();
        let size = value.approx_size() + key.len() + 128;
        let ttl = if negative { policy.negative_ttl } else { policy.ttl };
        let entry = Entry {
            value: Arc::new(value),
            hash,
            fetched_at_ms: now_ms(),
            ttl_ms: ttl.as_millis() as u64,
            stale_ms: if negative { 0 } else { policy.stale.as_millis() as u64 },
            negative,
            size,
            tick: 0,
        };
        let lookup = entry.lookup(Source::Miss);
        self.insert_entry(key.to_string(), entry);
        lookup
    }

    fn insert_entry(&self, key: String, mut entry: Entry) {
        let mut inner = self.lock();
        inner.tick += 1;
        entry.tick = inner.tick;
        if let Some(old) = inner.map.remove(&key) {
            inner.lru.remove(&old.tick);
            inner.bytes = inner.bytes.saturating_sub(old.size);
        }
        inner.bytes += entry.size;
        inner.lru.insert(entry.tick, key.clone());
        inner.map.insert(key, entry);
        // Evict least-recently-used entries over budget.
        while inner.bytes > self.config.max_bytes || inner.map.len() > self.config.max_entries {
            let Some((&tick, _)) = inner.lru.iter().next() else { break };
            let k = inner.lru.remove(&tick).unwrap_or_default();
            if let Some(e) = inner.map.remove(&k) {
                inner.bytes = inner.bytes.saturating_sub(e.size);
                self.stats.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Returns the cached value or fetches it, applying the full policy.
    pub fn get_or_fetch(self: &Arc<Self>, key: &str, policy: &Policy, force_refresh: bool, fetch: FetchFn) -> Result<Lookup, FetchError> {
        if policy.is_none() {
            return match fetch()? {
                Fetched::Value(v) | Fetched::Negative(v) => {
                    let hash: Arc<str> = sha256_hex(v.to_string().as_bytes()).into();
                    Ok(Lookup { value: Arc::new(v), source: Source::Bypass, fetched_at_ms: now_ms(), ttl_ms: 0, negative: false, hash })
                }
            };
        }
        let now = now_ms();
        if !force_refresh {
            let mut inner = self.lock();
            if let Some(e) = inner.map.get(key) {
                let fresh_until = e.fetched_at_ms + e.ttl_ms;
                if now < fresh_until {
                    let l = e.lookup(Source::Hit);
                    if l.negative {
                        self.stats.negative_hits.fetch_add(1, Ordering::Relaxed);
                    }
                    self.stats.hits.fetch_add(1, Ordering::Relaxed);
                    Self::touch(&mut inner, key);
                    return Ok(l);
                }
                if now < fresh_until + e.stale_ms {
                    let l = e.lookup(Source::Stale);
                    Self::touch(&mut inner, key);
                    drop(inner);
                    self.stats.stale_hits.fetch_add(1, Ordering::Relaxed);
                    self.spawn_refresh(key, *policy, fetch);
                    return Ok(l);
                }
            }
        }
        self.fetch_coalesced(key, policy, fetch)
    }

    fn spawn_refresh(self: &Arc<Self>, key: &str, policy: Policy, fetch: FetchFn) {
        {
            let mut r = self.refreshing.lock().unwrap_or_else(|e| e.into_inner());
            if !r.insert(key.to_string()) {
                return; // a refresh is already running
            }
        }
        let cache = self.clone();
        let key = key.to_string();
        let spawned = std::thread::Builder::new().name("cache-refresh".into()).spawn(move || {
            cache.stats.refreshes.fetch_add(1, Ordering::Relaxed);
            let _ = cache.fetch_coalesced(&key, &policy, fetch);
            cache.refreshing.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        });
        if spawned.is_err() {
            crate::log_warn!("cache", "could not spawn refresh thread");
        }
    }

    fn fetch_coalesced(self: &Arc<Self>, key: &str, policy: &Policy, fetch: FetchFn) -> Result<Lookup, FetchError> {
        let (flight, leader) = {
            let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
            match inflight.get(key) {
                Some(f) => (f.clone(), false),
                None => {
                    let f = Arc::new(Flight { result: Mutex::new(None), cv: Condvar::new() });
                    inflight.insert(key.to_string(), f.clone());
                    (f, true)
                }
            }
        };
        if !leader {
            self.stats.coalesced.fetch_add(1, Ordering::Relaxed);
            let mut guard = flight.result.lock().unwrap_or_else(|e| e.into_inner());
            while guard.is_none() {
                let (g, timeout) = flight
                    .cv
                    .wait_timeout(guard, Duration::from_secs(120))
                    .unwrap_or_else(|e| e.into_inner());
                guard = g;
                if timeout.timed_out() && guard.is_none() {
                    return Err(FetchError { message: "timed out waiting for in-flight fetch".into(), status: None, transient: true });
                }
            }
            return guard.clone().unwrap_or_else(|| Err(FetchError { message: "fetch abandoned".into(), status: None, transient: true }))
                .map(|mut l| {
                    l.source = if l.source == Source::StaleOnError { Source::StaleOnError } else { Source::Coalesced };
                    l
                });
        }
        self.stats.misses.fetch_add(1, Ordering::Relaxed);
        // Run the fetch outside of every lock; catch panics so waiters are released.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fetch()))
            .unwrap_or_else(|_| Err(FetchError { message: "fetcher panicked".into(), status: None, transient: true }));
        let result = match outcome {
            Ok(Fetched::Value(v)) => Ok(self.put(key, v, policy, false)),
            Ok(Fetched::Negative(v)) => Ok(self.put(key, v, policy, true)),
            Err(e) => {
                self.stats.fetch_errors.fetch_add(1, Ordering::Relaxed);
                // Stale-if-error: serve an expired value within the horizon.
                let horizon = self.config.max_stale_on_error.as_millis() as u64;
                let inner = self.lock();
                match inner.map.get(key) {
                    Some(entry) if !entry.negative && now_ms() < entry.fetched_at_ms + entry.ttl_ms + horizon => {
                        self.stats.stale_on_error.fetch_add(1, Ordering::Relaxed);
                        crate::log_warn!("cache", "serving stale value after upstream error", key = key, error = e.message);
                        Ok(entry.lookup(Source::StaleOnError))
                    }
                    _ => Err(e),
                }
            }
        };
        {
            let mut guard = flight.result.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(result.clone());
            flight.cv.notify_all();
        }
        self.inflight.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
        result
    }

    /// Removes one key. Returns whether it existed.
    pub fn invalidate(&self, key: &str) -> bool {
        let mut inner = self.lock();
        if let Some(e) = inner.map.remove(key) {
            inner.lru.remove(&e.tick);
            inner.bytes = inner.bytes.saturating_sub(e.size);
            self.dirty.store(true, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Removes all keys matching a predicate. Returns the count removed.
    pub fn invalidate_where(&self, pred: impl Fn(&str) -> bool) -> usize {
        let mut inner = self.lock();
        let keys: Vec<String> = inner.map.keys().filter(|k| pred(k)).cloned().collect();
        for k in &keys {
            if let Some(e) = inner.map.remove(k) {
                inner.lru.remove(&e.tick);
                inner.bytes = inner.bytes.saturating_sub(e.size);
            }
        }
        if !keys.is_empty() {
            self.dirty.store(true, Ordering::Relaxed);
        }
        keys.len()
    }

    pub fn clear(&self) -> usize {
        self.invalidate_where(|_| true)
    }

    /// Drops entries past the stale-on-error horizon.
    pub fn purge_expired(&self) -> usize {
        let now = now_ms();
        let horizon = self.config.max_stale_on_error.as_millis() as u64;
        let mut inner = self.lock();
        let dead: Vec<String> = inner
            .map
            .iter()
            .filter(|(_, e)| now > e.fetched_at_ms + e.ttl_ms + e.stale_ms.max(if e.negative { 0 } else { horizon }))
            .map(|(k, _)| k.clone())
            .collect();
        for k in &dead {
            if let Some(e) = inner.map.remove(k) {
                inner.lru.remove(&e.tick);
                inner.bytes = inner.bytes.saturating_sub(e.size);
            }
        }
        dead.len()
    }

    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn bytes(&self) -> usize {
        self.lock().bytes
    }

    /// Keys grouped by their first `:`-separated segment, with counts.
    pub fn key_namespaces(&self) -> BTreeMap<String, usize> {
        let inner = self.lock();
        let mut out = BTreeMap::new();
        for k in inner.map.keys() {
            let ns = k.split(['|', '?']).next().unwrap_or(k).to_string();
            *out.entry(ns).or_insert(0) += 1;
        }
        out
    }

    /// JSON snapshot of statistics.
    pub fn stats_json(&self) -> Value {
        let s = &self.stats;
        let hits = s.hits.load(Ordering::Relaxed);
        let stale = s.stale_hits.load(Ordering::Relaxed);
        let misses = s.misses.load(Ordering::Relaxed);
        let coalesced = s.coalesced.load(Ordering::Relaxed);
        let served = hits + stale + misses + coalesced;
        let ratio = if served == 0 { 0.0 } else { (hits + stale + coalesced) as f64 / served as f64 };
        let (entries, bytes) = {
            let inner = self.lock();
            (inner.map.len(), inner.bytes)
        };
        crate::obj! {
            "entries" => entries,
            "approxBytes" => bytes,
            "maxBytes" => self.config.max_bytes,
            "maxEntries" => self.config.max_entries,
            "hits" => hits,
            "staleHits" => stale,
            "misses" => misses,
            "coalesced" => coalesced,
            "negativeHits" => s.negative_hits.load(Ordering::Relaxed),
            "backgroundRefreshes" => s.refreshes.load(Ordering::Relaxed),
            "fetchErrors" => s.fetch_errors.load(Ordering::Relaxed),
            "staleOnError" => s.stale_on_error.load(Ordering::Relaxed),
            "evictions" => s.evictions.load(Ordering::Relaxed),
            "hitRatio" => (ratio * 10_000.0).round() / 10_000.0,
            "persistPath" => self.config.persist_path.as_ref().map(|p| p.display().to_string()),
            "persistedEntries" => s.persisted_entries.load(Ordering::Relaxed),
            "loadedEntries" => s.loaded_entries.load(Ordering::Relaxed),
        }
    }

    /// Writes a snapshot to the persistence file (atomic rename). Negative
    /// entries are not persisted.
    pub fn persist(&self) -> std::io::Result<usize> {
        let Some(path) = &self.config.persist_path else { return Ok(0) };
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return Ok(0);
        }
        let snapshot: Vec<(String, Arc<Value>, u64, u64, u64)> = {
            let inner = self.lock();
            // Oldest-first so that LRU order is reconstructed on load.
            inner
                .lru
                .values()
                .filter_map(|k| inner.map.get(k).filter(|e| !e.negative).map(|e| (k.clone(), e.value.clone(), e.fetched_at_ms, e.ttl_ms, e.stale_ms)))
                .collect()
        };
        let tmp = path.with_extension("jsonl.tmp");
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let file = std::fs::File::create(&tmp)?;
        let mut w = std::io::BufWriter::new(file);
        writeln!(w, "{}", crate::obj! { "format" => "steam-mcp-cache", "version" => 1 })?;
        for (k, v, f, t, s) in &snapshot {
            let mut line = String::with_capacity(256);
            line.push_str("{\"k\":");
            json::write_string(&mut line, k);
            line.push_str(&format!(",\"f\":{f},\"t\":{t},\"s\":{s},\"v\":"));
            line.push_str(&v.to_string());
            line.push('}');
            writeln!(w, "{line}")?;
        }
        w.flush()?;
        w.get_ref().sync_all()?;
        drop(w);
        std::fs::rename(&tmp, path)?;
        self.stats.persisted_entries.store(snapshot.len() as u64, Ordering::Relaxed);
        Ok(snapshot.len())
    }

    /// Loads a snapshot written by [`persist`](Self::persist).
    pub fn load(&self) -> usize {
        let Some(path) = &self.config.persist_path else { return 0 };
        let Ok(file) = std::fs::File::open(path) else { return 0 };
        let now = now_ms();
        let horizon = self.config.max_stale_on_error.as_millis() as u64;
        let mut loaded = 0;
        for (i, line) in std::io::BufReader::new(file).lines().enumerate() {
            let Ok(line) = line else { break };
            let Ok(v) = json::parse(&line) else { continue };
            if i == 0 {
                if v["format"].as_str() != Some("steam-mcp-cache") {
                    crate::log_warn!("cache", "ignoring unrecognized cache file", path = path.display());
                    return 0;
                }
                continue;
            }
            let (Some(k), Some(f), Some(t), Some(s)) = (v["k"].as_str(), v["f"].as_u64(), v["t"].as_u64(), v["s"].as_u64()) else {
                continue;
            };
            if now > f + t + s.max(horizon) {
                continue;
            }
            let value = v.get("v").cloned().unwrap_or(Value::Null);
            let hash: Arc<str> = sha256_hex(value.to_string().as_bytes()).into();
            let size = value.approx_size() + k.len() + 128;
            self.insert_entry(
                k.to_string(),
                Entry { value: Arc::new(value), hash, fetched_at_ms: f, ttl_ms: t, stale_ms: s, negative: false, size, tick: 0 },
            );
            loaded += 1;
        }
        self.dirty.store(false, Ordering::Relaxed);
        self.stats.loaded_entries.store(loaded as u64, Ordering::Relaxed);
        crate::log_info!("cache", "loaded persisted cache", path = path.display(), entries = loaded);
        loaded
    }

    /// Starts the maintenance thread: purges dead entries and persists
    /// snapshots on an interval. Stops when `stop` is set.
    pub fn start_maintenance(self: &Arc<Self>, stop: Arc<AtomicBool>) {
        let cache = self.clone();
        let _ = std::thread::Builder::new().name("cache-maint".into()).spawn(move || {
            let interval = cache.config.persist_interval.max(Duration::from_secs(1));
            let mut elapsed = Duration::ZERO;
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                elapsed += Duration::from_millis(250);
                if elapsed >= interval {
                    elapsed = Duration::ZERO;
                    let purged = cache.purge_expired();
                    if purged > 0 {
                        crate::log_debug!("cache", "purged expired entries", count = purged);
                    }
                    if let Err(e) = cache.persist() {
                        crate::log_warn!("cache", "persist failed", error = e);
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn counting_fetch(counter: Arc<AtomicUsize>, value: i64) -> FetchFn {
        Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(30));
            Ok(Fetched::Value(Value::from(value)))
        })
    }

    #[test]
    fn hit_after_miss() {
        let cache = Cache::new(CacheConfig::default());
        let n = Arc::new(AtomicUsize::new(0));
        let p = Policy::new(60, 60);
        let a = cache.get_or_fetch("k", &p, false, counting_fetch(n.clone(), 1)).unwrap();
        assert_eq!(a.source, Source::Miss);
        let b = cache.get_or_fetch("k", &p, false, counting_fetch(n.clone(), 2)).unwrap();
        assert_eq!(b.source, Source::Hit);
        assert_eq!(*b.value, Value::from(1));
        assert_eq!(n.load(Ordering::SeqCst), 1);
        let c = cache.get_or_fetch("k", &p, true, counting_fetch(n.clone(), 3)).unwrap();
        assert_eq!(*c.value, Value::from(3));
    }

    #[test]
    fn coalesces_concurrent_misses() {
        let cache = Cache::new(CacheConfig::default());
        let n = Arc::new(AtomicUsize::new(0));
        let p = Policy::new(60, 0);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let cache = cache.clone();
                let f = counting_fetch(n.clone(), 7);
                std::thread::spawn(move || cache.get_or_fetch("same", &p, false, f).unwrap())
            })
            .collect();
        for h in handles {
            assert_eq!(*h.join().unwrap().value, Value::from(7));
        }
        assert_eq!(n.load(Ordering::SeqCst), 1);
        assert!(cache.stats.coalesced.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn stale_while_revalidate_and_stale_on_error() {
        let cache = Cache::new(CacheConfig::default());
        let p = Policy { ttl: Duration::from_millis(1), stale: Duration::from_secs(60), negative_ttl: Duration::ZERO };
        let n = Arc::new(AtomicUsize::new(0));
        cache.get_or_fetch("k", &p, false, counting_fetch(n.clone(), 1)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let s = cache.get_or_fetch("k", &p, false, counting_fetch(n.clone(), 2)).unwrap();
        assert_eq!(s.source, Source::Stale);
        assert_eq!(*s.value, Value::from(1));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(n.load(Ordering::SeqCst), 2, "background refresh ran");

        let p2 = Policy { ttl: Duration::from_millis(1), stale: Duration::ZERO, negative_ttl: Duration::ZERO };
        cache.put("e", Value::from("old"), &p2, false);
        std::thread::sleep(Duration::from_millis(5));
        let failing: FetchFn = Arc::new(|| Err(FetchError { message: "down".into(), status: Some(503), transient: true }));
        let r = cache.get_or_fetch("e", &p2, false, failing.clone()).unwrap();
        assert_eq!(r.source, Source::StaleOnError);
        assert_eq!(*r.value, Value::from("old"));
        assert!(cache.get_or_fetch("missing", &p2, false, failing).is_err());
    }

    #[test]
    fn lru_eviction_by_bytes() {
        let cache = Cache::new(CacheConfig { max_bytes: 2000, ..Default::default() });
        let p = Policy::new(60, 0);
        for i in 0..50 {
            cache.put(&format!("k{i}"), Value::from("x".repeat(100)), &p, false);
        }
        assert!(cache.bytes() <= 2000);
        assert!(cache.peek("k49").is_some());
        assert!(cache.peek("k0").is_none());
        assert!(cache.stats.evictions.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn persistence_roundtrip() {
        let dir = std::env::temp_dir().join(format!("steam-mcp-test-{}", crate::util::crypto::random_id(6)));
        let path = dir.join("cache.jsonl");
        let cfg = CacheConfig { persist_path: Some(path.clone()), ..Default::default() };
        {
            let cache = Cache::new(cfg.clone());
            cache.put("a|1", crate::obj! { "steamid" => 76561197960287930u64 }, &Policy::new(3600, 0), false);
            cache.put("neg", Value::Null, &Policy::new(3600, 0), true);
            assert_eq!(cache.persist().unwrap(), 1);
        }
        let cache = Cache::new(cfg);
        let l = cache.peek("a|1").unwrap();
        assert_eq!(l.value["steamid"].as_u64(), Some(76561197960287930));
        assert!(cache.peek("neg").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn invalidation() {
        let cache = Cache::new(CacheConfig::default());
        let p = Policy::new(60, 0);
        cache.put("app:1", Value::Null, &p, false);
        cache.put("app:2", Value::Null, &p, false);
        cache.put("player:1", Value::Null, &p, false);
        assert_eq!(cache.invalidate_where(|k| k.starts_with("app:")), 2);
        assert!(cache.invalidate("player:1"));
        assert!(cache.is_empty());
    }
}
