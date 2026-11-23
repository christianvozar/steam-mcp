//! Multi-principal, agent-oriented controls (ADR 0018):
//!
//! * **Fairness and quotas**: a per-principal cap on concurrent tool calls,
//!   daily tool-call and upstream-fetch budgets, and per-principal rate
//!   overrides, so one runaway agent cannot starve others or spend the
//!   deployment's upstream quota.
//! * **Upstream attribution**: the dispatcher records the calling principal
//!   in a thread-local; the upstream client charges cache misses to it.
//! * **Idempotency** for state-changing tools: a repeated key with the same
//!   arguments replays the first successful result instead of acting twice.
//! * **Write policy**: operator rules that pre-approve specific writes for
//!   unattended agents, with daily caps.
//! * **Usage metering and an audit ring** queryable through tools.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::protocol::{McpError, RATE_LIMITED};
use crate::config::{Config, PrincipalLimit, WriteRule};
use crate::json::Value;
use crate::obj;
use crate::util::ratelimit::{KeyedLimiter, TokenBucket};
use crate::util::time::{iso8601_secs, now_secs};

/// `_meta` key carrying an idempotency key on `tools/call`.
pub const META_IDEMPOTENCY_KEY: &str = "io.github.christianvozar/idempotencyKey";
/// `_meta` key requesting a dry run of a state-changing tool.
pub const META_DRY_RUN: &str = "io.github.christianvozar/dryRun";
/// `_meta` key marking a replayed idempotent result.
pub const META_REPLAYED: &str = "io.github.christianvozar/idempotentReplay";
/// `_meta` key listing response fields that hold secrets returned once.
pub const META_SECRET_FIELDS: &str = "io.github.christianvozar/secretFields";

/// A copy of a result that is safe to keep for replay: secrets named in
/// `_meta[META_SECRET_FIELDS]` are removed from the structured response and
/// the JSON text mirror is dropped.
fn replay_safe(result: &Value) -> Value {
    let fields: Vec<String> = result
        .at(&["_meta", META_SECRET_FIELDS])
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if fields.is_empty() {
        return result.clone();
    }
    let mut r = result.clone();
    if let Some(Value::Object(sc)) = r.get_mut("structuredContent") {
        if let Some(Value::Object(resp)) = sc.get_mut("response") {
            for f in &fields {
                if resp.get(f).is_some() {
                    resp.insert(
                        f.as_str(),
                        "[returned once by the original call; not stored for replay]",
                    );
                }
            }
        }
    }
    let summary = r
        .at(&["content", "0", "text"])
        .as_str()
        .unwrap_or("")
        .to_string();
    r.set("content", crate::arr![obj! { "type" => "text", "text" => format!("{summary} (Replayed: the secret is not stored and was only in the original response.)") }]);
    r
}

const AUDIT_CAPACITY: usize = 2000;
const IDEMPOTENCY_CAPACITY: usize = 20_000;

thread_local! {
    static CURRENT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Runs `f` with `principal` recorded as the current principal of this
/// thread (restoring the previous value afterwards).
pub fn with_principal<R>(principal: &str, f: impl FnOnce() -> R) -> R {
    let prev = CURRENT.with(|c| c.replace(Some(principal.to_string())));
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0.take();
            CURRENT.with(|c| *c.borrow_mut() = prev);
        }
    }
    let _restore = Restore(prev);
    f()
}

/// The principal whose request this thread is serving, if any.
pub fn current_principal() -> Option<String> {
    CURRENT.with(|c| c.borrow().clone())
}

/// Per-principal usage counters.
#[derive(Default, Clone)]
struct Usage {
    tool_calls: u64,
    tool_errors: u64,
    rate_limited: u64,
    writes: u64,
    policy_writes: u64,
    dry_runs: u64,
    idempotent_replays: u64,
    upstream_calls: u64,
    batch_calls: u64,
    per_tool: BTreeMap<String, u64>,
    first_seen: u64,
    last_seen: u64,
}

#[derive(Default)]
struct PrincipalState {
    inflight: u32,
    day: u64,
    calls_today: u64,
    upstream_today: u64,
    /// Executions per write-policy rule index today.
    policy_today: HashMap<usize, u64>,
    usage: Usage,
}

impl PrincipalState {
    fn roll(&mut self, today: u64) {
        if self.day != today {
            self.day = today;
            self.calls_today = 0;
            self.upstream_today = 0;
            self.policy_today.clear();
        }
    }
}

enum Idem {
    InProgress {
        fingerprint: String,
        since: u64,
    },
    Done {
        fingerprint: String,
        result: Value,
        at: u64,
    },
}

/// Outcome of starting an idempotent call.
pub enum IdemBegin {
    /// No prior call: proceed, then call [`Agents::idem_complete`].
    Fresh,
    /// The same call already succeeded: return this result.
    Replay(Value),
    /// The same key is being processed right now.
    InProgress,
    /// The key was used with different arguments.
    Mismatch,
}

/// The effective limits for one principal.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub rps: Option<(f64, f64)>,
    pub max_concurrent: u32,
    pub daily_calls: u64,
    pub daily_upstream: u64,
}

/// A rate override's (rate, burst) and its bucket.
type Override = (f64, f64, Arc<TokenBucket>);

/// Agent controls shared by the server and the upstream client.
#[derive(Default)]
pub struct Agents {
    state: Mutex<HashMap<String, PrincipalState>>,
    /// Signalled whenever a concurrency slot is released.
    slots: Condvar,
    /// Token buckets for principals with a rate override (keyed by principal).
    overrides: Mutex<HashMap<String, Override>>,
    idempotency: Mutex<HashMap<(String, String), Idem>>,
    audit: Mutex<VecDeque<Value>>,
}

/// Releases a concurrency slot when dropped.
pub struct Permit {
    agents: Arc<Agents>,
    principal: String,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut st = self.agents.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = st.get_mut(&self.principal) {
            p.inflight = p.inflight.saturating_sub(1);
        }
        drop(st);
        self.agents.slots.notify_all();
    }
}

fn today() -> u64 {
    now_secs() / 86_400
}

fn ms_until_midnight() -> u64 {
    let now = now_secs();
    ((now / 86_400 + 1) * 86_400 - now) * 1000
}

fn quota_error(reason: &str, limit: u64, retry_after_ms: u64, message: &str) -> McpError {
    McpError::new(RATE_LIMITED, message)
        .with_data(obj! { "retryAfterMs" => retry_after_ms, "reason" => reason, "limit" => limit })
        .http(429)
}

/// Shell-style match with `*` wildcards, ignoring ASCII case.
pub fn glob(pattern: &str, value: &str) -> bool {
    let p = pattern.to_ascii_lowercase();
    let v = value.to_ascii_lowercase();
    let parts: Vec<&str> = p.split('*').collect();
    if parts.len() == 1 {
        return p == v;
    }
    let mut rest = v.as_str();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(pos) => rest = &rest[pos + part.len()..],
                None => return false,
            }
        }
    }
    true
}

impl Agents {
    pub fn new() -> Arc<Agents> {
        Arc::new(Agents::default())
    }

    /// The limits that apply to `principal` (first matching override wins).
    pub fn limits(cfg: &Config, principal: &str) -> Limits {
        let base = Limits {
            rps: None,
            max_concurrent: cfg.principal_max_concurrent,
            daily_calls: cfg.principal_daily_tool_calls,
            daily_upstream: cfg.principal_daily_upstream,
        };
        match cfg
            .principal_limits
            .iter()
            .find(|l| glob(&l.principal, principal))
        {
            None => base,
            Some(PrincipalLimit {
                rps,
                burst,
                max_concurrent,
                daily_calls,
                daily_upstream,
                ..
            }) => Limits {
                rps: rps.map(|r| (r, burst.unwrap_or(r * 3.0))),
                max_concurrent: max_concurrent.unwrap_or(base.max_concurrent),
                daily_calls: daily_calls.unwrap_or(base.daily_calls),
                daily_upstream: daily_upstream.unwrap_or(base.daily_upstream),
            },
        }
    }

    fn with_state<R>(&self, principal: &str, f: impl FnOnce(&mut PrincipalState) -> R) -> R {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let p = st.entry(principal.to_string()).or_default();
        p.roll(today());
        let now = now_secs();
        if p.usage.first_seen == 0 {
            p.usage.first_seen = now;
        }
        p.usage.last_seen = now;
        f(p)
    }

    /// Per-request rate check: a principal's override bucket, else the shared keyed limiter.
    pub fn check_rate(
        &self,
        cfg: &Config,
        principal: &str,
        default: &KeyedLimiter,
    ) -> Result<(), McpError> {
        let limits = Self::limits(cfg, principal);
        let res = match limits.rps {
            Some((rate, burst)) => {
                let bucket = {
                    let mut o = self.overrides.lock().unwrap_or_else(|e| e.into_inner());
                    let entry = o
                        .entry(principal.to_string())
                        .or_insert_with(|| (rate, burst, Arc::new(TokenBucket::new(rate, burst))));
                    if entry.0 != rate || entry.1 != burst {
                        *entry = (rate, burst, Arc::new(TokenBucket::new(rate, burst)));
                    }
                    entry.2.clone()
                };
                bucket.try_acquire()
            }
            None => default.check(principal),
        };
        res.map_err(|wait| {
            self.with_state(principal, |p| p.usage.rate_limited += 1);
            McpError::rate_limited(wait.as_millis() as u64 + 1)
        })
    }

    /// Admits a tool call: enforces the daily call budget and the concurrency
    /// cap. A call over the cap waits (up to `principal_queue_ms`) for one of
    /// the principal's own calls to finish, so bursts queue instead of failing.
    pub fn admit(
        self: &Arc<Self>,
        cfg: &Config,
        principal: &str,
        tool: &str,
    ) -> Result<Permit, McpError> {
        let limits = Self::limits(cfg, principal);
        let deadline = std::time::Instant::now() + Duration::from_millis(cfg.principal_queue_ms);
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let p = st.entry(principal.to_string()).or_default();
            p.roll(today());
            let now = now_secs();
            if p.usage.first_seen == 0 {
                p.usage.first_seen = now;
            }
            p.usage.last_seen = now;
            if limits.daily_calls > 0 && p.calls_today >= limits.daily_calls {
                p.usage.rate_limited += 1;
                return Err(quota_error(
                    "daily_tool_calls",
                    limits.daily_calls,
                    ms_until_midnight(),
                    &format!(
                        "Daily tool-call budget of {} exhausted for this principal; it resets at 00:00 UTC",
                        limits.daily_calls
                    ),
                ));
            }
            if limits.max_concurrent == 0 || p.inflight < limits.max_concurrent {
                p.inflight += 1;
                p.calls_today += 1;
                p.usage.tool_calls += 1;
                *p.usage.per_tool.entry(tool.to_string()).or_default() += 1;
                return Ok(Permit {
                    agents: self.clone(),
                    principal: principal.to_string(),
                });
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                p.usage.rate_limited += 1;
                return Err(quota_error(
                    "concurrency",
                    u64::from(limits.max_concurrent),
                    250,
                    &format!(
                        "Too many concurrent tool calls for this principal (limit {}); retry when one finishes",
                        limits.max_concurrent
                    ),
                ));
            }
            st = self
                .slots
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Records the outcome of an admitted tool call.
    pub fn record(&self, principal: &str, is_error: bool) {
        if is_error {
            self.with_state(principal, |p| p.usage.tool_errors += 1);
        }
    }

    pub fn note(&self, principal: &str, what: &str) {
        self.with_state(principal, |p| match what {
            "write" => p.usage.writes += 1,
            "policy_write" => {
                p.usage.writes += 1;
                p.usage.policy_writes += 1
            }
            "dry_run" => p.usage.dry_runs += 1,
            "replay" => p.usage.idempotent_replays += 1,
            "batch" => p.usage.batch_calls += 1,
            _ => {}
        });
    }

    /// Charges one upstream fetch to the current principal (if any).
    /// Fails when that principal's daily upstream budget is spent.
    pub fn charge_upstream(&self, cfg: &Config) -> Result<(), String> {
        let Some(principal) = current_principal() else {
            return Ok(());
        };
        let limits = Self::limits(cfg, &principal);
        self.with_state(&principal, |p| {
            if limits.daily_upstream > 0 && p.upstream_today >= limits.daily_upstream {
                p.usage.rate_limited += 1;
                return Err(format!("rate limit: daily upstream budget of {} fetches exhausted for principal {principal}; serving cached data only", limits.daily_upstream));
            }
            p.upstream_today += 1;
            p.usage.upstream_calls += 1;
            Ok(())
        })
    }

    // -- write policy ----------------------------------------------------------

    /// The first rule pre-approving `tool` on `target` for `principal` that
    /// still has room under its daily cap.
    pub fn policy_match(
        &self,
        cfg: &Config,
        principal: &str,
        tool: &str,
        target: &str,
    ) -> Option<(usize, WriteRule)> {
        let rules: Vec<(usize, WriteRule)> = cfg
            .write_policy
            .iter()
            .cloned()
            .enumerate()
            .filter(|(_, r)| {
                glob(&r.principal, principal) && glob(&r.tool, tool) && glob(&r.target, target)
            })
            .collect();
        self.with_state(principal, |p| {
            rules.into_iter().find(|(i, r)| {
                r.max_per_day == 0 || p.policy_today.get(i).copied().unwrap_or(0) < r.max_per_day
            })
        })
    }

    /// Like [`policy_match`](Self::policy_match), and counts the execution
    /// against the rule's daily cap in the same step.
    pub fn policy_take(
        &self,
        cfg: &Config,
        principal: &str,
        tool: &str,
        target: &str,
    ) -> Option<(usize, WriteRule)> {
        let rules: Vec<(usize, WriteRule)> = cfg
            .write_policy
            .iter()
            .cloned()
            .enumerate()
            .filter(|(_, r)| {
                glob(&r.principal, principal) && glob(&r.tool, tool) && glob(&r.target, target)
            })
            .collect();
        self.with_state(principal, |p| {
            let found = rules.into_iter().find(|(i, r)| {
                r.max_per_day == 0 || p.policy_today.get(i).copied().unwrap_or(0) < r.max_per_day
            })?;
            *p.policy_today.entry(found.0).or_default() += 1;
            Some(found)
        })
    }

    // -- idempotency -----------------------------------------------------------

    fn idem_gc(map: &mut HashMap<(String, String), Idem>, ttl: u64) {
        let now = now_secs();
        map.retain(|_, v| match v {
            Idem::Done { at, .. } => *at + ttl > now,
            Idem::InProgress { since, .. } => *since + 3600 > now,
        });
        if map.len() > IDEMPOTENCY_CAPACITY {
            let mut ages: Vec<((String, String), u64)> = map
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        match v {
                            Idem::Done { at, .. } => *at,
                            Idem::InProgress { since, .. } => *since,
                        },
                    )
                })
                .collect();
            ages.sort_by_key(|a| a.1);
            for (k, _) in ages.into_iter().take(map.len() - IDEMPOTENCY_CAPACITY) {
                map.remove(&k);
            }
        }
    }

    pub fn idem_begin(
        &self,
        cfg: &Config,
        principal: &str,
        key: &str,
        fingerprint: &str,
    ) -> IdemBegin {
        let mut m = self.idempotency.lock().unwrap_or_else(|e| e.into_inner());
        Self::idem_gc(&mut m, cfg.idempotency_ttl_secs);
        let k = (principal.to_string(), key.to_string());
        match m.get(&k) {
            Some(Idem::Done {
                fingerprint: f,
                result,
                ..
            }) => {
                if f == fingerprint {
                    IdemBegin::Replay(result.clone())
                } else {
                    IdemBegin::Mismatch
                }
            }
            Some(Idem::InProgress { fingerprint: f, .. }) => {
                if f == fingerprint {
                    IdemBegin::InProgress
                } else {
                    IdemBegin::Mismatch
                }
            }
            None => {
                m.insert(
                    k,
                    Idem::InProgress {
                        fingerprint: fingerprint.to_string(),
                        since: now_secs(),
                    },
                );
                IdemBegin::Fresh
            }
        }
    }

    /// Finishes an idempotent call: stores a successful result, or forgets the
    /// key (so a failed or interrupted call can be retried).
    pub fn idem_complete(
        &self,
        principal: &str,
        key: &str,
        fingerprint: &str,
        result: Option<&Value>,
    ) {
        let mut m = self.idempotency.lock().unwrap_or_else(|e| e.into_inner());
        let k = (principal.to_string(), key.to_string());
        match result {
            Some(r) => {
                m.insert(
                    k,
                    Idem::Done {
                        fingerprint: fingerprint.to_string(),
                        result: replay_safe(r),
                        at: now_secs(),
                    },
                );
            }
            None => {
                if matches!(m.get(&k), Some(Idem::InProgress { .. })) {
                    m.remove(&k);
                }
            }
        }
    }

    // -- audit and usage -------------------------------------------------------

    pub fn audit(
        &self,
        principal: &str,
        tool: &str,
        action: &str,
        approved_by: &str,
        subject: Value,
    ) {
        let mut a = self.audit.lock().unwrap_or_else(|e| e.into_inner());
        if a.len() >= AUDIT_CAPACITY {
            a.pop_front();
        }
        a.push_back(obj! { "at" => iso8601_secs(now_secs()), "principal" => principal, "tool" => tool, "action" => action, "approvedBy" => approved_by, "subject" => subject });
    }

    /// Newest first; `principal = None` means everyone.
    pub fn audit_entries(&self, principal: Option<&str>, limit: usize) -> Vec<Value> {
        let a = self.audit.lock().unwrap_or_else(|e| e.into_inner());
        a.iter()
            .rev()
            .filter(|e| principal.is_none_or(|p| e["principal"].as_str() == Some(p)))
            .take(limit)
            .cloned()
            .collect()
    }

    fn usage_json(cfg: &Config, principal: &str, p: &PrincipalState) -> Value {
        let l = Self::limits(cfg, principal);
        let u = &p.usage;
        let mut tools: Vec<(&String, &u64)> = u.per_tool.iter().collect();
        tools.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        obj! {
            "principal" => principal,
            "toolCalls" => u.tool_calls,
            "toolErrors" => u.tool_errors,
            "rateLimited" => u.rate_limited,
            "writes" => u.writes,
            "policyApprovedWrites" => u.policy_writes,
            "dryRuns" => u.dry_runs,
            "idempotentReplays" => u.idempotent_replays,
            "batchCalls" => u.batch_calls,
            "upstreamCalls" => u.upstream_calls,
            "inflight" => u64::from(p.inflight),
            "today" => obj! { "toolCalls" => p.calls_today, "upstreamCalls" => p.upstream_today },
            "limits" => obj! {
                "requestsPerSecond" => l.rps.map(|r| Value::from(r.0)).unwrap_or(Value::from(cfg.client_rps)),
                "maxConcurrent" => u64::from(l.max_concurrent),
                "dailyToolCalls" => if l.daily_calls == 0 { Value::Null } else { Value::from(l.daily_calls) },
                "dailyUpstream" => if l.daily_upstream == 0 { Value::Null } else { Value::from(l.daily_upstream) },
            },
            "topTools" => tools.iter().take(10).map(|(t, n)| obj! { "tool" => t.as_str(), "calls" => **n }).collect::<Vec<_>>(),
            "firstSeen" => if u.first_seen == 0 { Value::Null } else { Value::from(iso8601_secs(u.first_seen)) },
            "lastSeen" => if u.last_seen == 0 { Value::Null } else { Value::from(iso8601_secs(u.last_seen)) },
        }
    }

    /// Usage for one principal, or (with `None`) every principal, busiest first.
    pub fn usage(&self, cfg: &Config, principal: Option<&str>) -> Vec<Value> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut rows: Vec<(&String, &PrincipalState)> = st
            .iter()
            .filter(|(k, _)| principal.is_none_or(|p| p == k.as_str()))
            .collect();
        rows.sort_by(|a, b| {
            b.1.usage
                .tool_calls
                .cmp(&a.1.usage.tool_calls)
                .then(a.0.cmp(b.0))
        });
        rows.into_iter()
            .map(|(k, p)| Self::usage_json(cfg, k, p))
            .collect()
    }

    /// Prometheus lines for the busiest principals.
    pub fn metrics(&self, prefix: &str, max: usize) -> String {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut rows: Vec<(&String, &PrincipalState)> = st.iter().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1.usage.tool_calls));
        let esc = |s: &str| {
            s.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', " ")
        };
        let mut out = String::new();
        for (name, help, get) in [
            (
                "principal_tool_calls_total",
                "Tool calls by principal",
                (|p: &PrincipalState| p.usage.tool_calls) as fn(&PrincipalState) -> u64,
            ),
            (
                "principal_tool_errors_total",
                "Tool errors by principal",
                |p| p.usage.tool_errors,
            ),
            (
                "principal_rate_limited_total",
                "Rejected by rate limits or quotas, by principal",
                |p| p.usage.rate_limited,
            ),
            (
                "principal_upstream_calls_total",
                "Upstream fetches charged to the principal",
                |p| p.usage.upstream_calls,
            ),
            (
                "principal_writes_total",
                "Executed writes by principal",
                |p| p.usage.writes,
            ),
            (
                "principal_inflight",
                "In-flight tool calls by principal",
                |p| u64::from(p.inflight),
            ),
        ] {
            out.push_str(&format!(
                "# HELP {prefix}_{name} {help}\n# TYPE {prefix}_{name} {}\n",
                if name.ends_with("_total") {
                    "counter"
                } else {
                    "gauge"
                }
            ));
            for (principal, p) in rows.iter().take(max) {
                out.push_str(&format!(
                    "{prefix}_{name}{{principal=\"{}\"}} {}\n",
                    esc(principal),
                    get(p)
                ));
            }
        }
        out
    }
}

/// How long to wait before retrying, for callers that need a `Duration`.
pub fn retry_after(e: &McpError) -> Option<Duration> {
    e.data
        .as_ref()
        .and_then(|d| d["retryAfterMs"].as_u64())
        .map(Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globbing() {
        assert!(glob("*", "anything"));
        assert!(glob("bot-*", "BOT-7"));
        assert!(glob("*_send_*", "twitch_send_chat_message"));
        assert!(glob("streamer_one", "Streamer_One"));
        assert!(!glob("bot-*", "human"));
        assert!(!glob("a*b", "ac"));
        assert!(glob("a*b*c", "aXbYc"));
    }

    #[test]
    fn principal_attribution_is_thread_local_and_restored() {
        assert_eq!(current_principal(), None);
        with_principal("alice", || {
            assert_eq!(current_principal().as_deref(), Some("alice"));
            with_principal("bob", || {
                assert_eq!(current_principal().as_deref(), Some("bob"))
            });
            assert_eq!(current_principal().as_deref(), Some("alice"));
            std::thread::spawn(|| assert_eq!(current_principal(), None))
                .join()
                .unwrap();
        });
        assert_eq!(current_principal(), None);
    }

    #[test]
    fn idempotency_states() {
        let a = Agents::new();
        let cfg = Config::from_map(&Default::default()).unwrap();
        assert!(matches!(
            a.idem_begin(&cfg, "p", "k", "f1"),
            IdemBegin::Fresh
        ));
        assert!(matches!(
            a.idem_begin(&cfg, "p", "k", "f1"),
            IdemBegin::InProgress
        ));
        assert!(matches!(
            a.idem_begin(&cfg, "p", "k", "f2"),
            IdemBegin::Mismatch
        ));
        assert!(
            matches!(a.idem_begin(&cfg, "other", "k", "f2"), IdemBegin::Fresh),
            "keys are per principal"
        );
        a.idem_complete("p", "k", "f1", Some(&obj! { "ok" => true }));
        assert!(
            matches!(a.idem_begin(&cfg, "p", "k", "f1"), IdemBegin::Replay(v) if v["ok"].as_bool() == Some(true))
        );
        assert!(matches!(
            a.idem_begin(&cfg, "p", "k", "f2"),
            IdemBegin::Mismatch
        ));
        // A failed call forgets the key so it can be retried.
        assert!(matches!(
            a.idem_begin(&cfg, "p", "k2", "f"),
            IdemBegin::Fresh
        ));
        a.idem_complete("p", "k2", "f", None);
        assert!(matches!(
            a.idem_begin(&cfg, "p", "k2", "f"),
            IdemBegin::Fresh
        ));
    }
}
