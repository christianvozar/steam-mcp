//! The transport-independent MCP request dispatcher.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::context::{NullSink, Principal, RequestContext, Sink, Transport, scopes};
use super::protocol::*;
use super::state::{Binding, StateSealer};
use super::subscriptions::{Filter, Hub, Subscription};
use super::tasks::{Status, TaskStore};
use super::tools::{Mrtr, ToolDef, ToolEnv, ToolOutcome, all_tools};
use super::{completion, prompts, resources, schema};
use crate::cache::{Cache, CacheConfig};
use crate::config::{AuthMode, ConfigHandle};
use crate::json::{Map, Value};
use crate::obj;
use crate::steam::{KeyKind, SteamClient};
use crate::util::CancelToken;
use crate::util::encoding::{b64_decode, b64url_encode};
use crate::util::ratelimit::KeyedLimiter;

/// What the transport should do with a handled message.
pub enum Reply {
    /// Send this JSON-RPC response; the u16 is the HTTP status the
    /// Streamable HTTP binding should use.
    Response(Value, u16),
    /// Nothing to send (notification, or a cancelled request).
    NoResponse,
    /// Keep the stream open for this subscription until it closes.
    Subscription(Arc<Subscription>),
}

#[derive(Default)]
pub struct ServerStats {
    pub requests: AtomicU64,
    pub errors: AtomicU64,
    pub tool_calls: AtomicU64,
    pub tool_errors: AtomicU64,
    pub notifications: AtomicU64,
    pub tasks_created: AtomicU64,
    pub input_required: AtomicU64,
    pub by_method: Mutex<BTreeMap<String, u64>>,
}

pub struct Core {
    pub config: ConfigHandle,
    pub steam: Arc<SteamClient>,
    pub sealer: StateSealer,
    pub tasks: TaskStore,
    pub hub: Arc<Hub>,
    pub limiter: KeyedLimiter,
    pub tools: Vec<ToolDef>,
    pub prompts: Vec<prompts::PromptDef>,
    pub started: Instant,
    pub stats: ServerStats,
    pub stop: Arc<AtomicBool>,
    /// Requests currently being processed (all transports).
    pub inflight: AtomicU64,
    persisted: AtomicBool,
    watch_state: Mutex<WatchState>,
}

/// Decrements the in-flight counter when a request finishes (even on panic).
struct InflightGuard<'a>(&'a AtomicU64);

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct WatchState {
    resource_hashes: HashMap<String, String>,
    resource_list_hash: Option<String>,
    tools_hash: Option<String>,
    prompts_hash: Option<String>,
    config_mtime: Option<std::time::SystemTime>,
}

enum Dispatched {
    Result(Value),
    Subscription(Arc<Subscription>),
}

type Res = Result<Dispatched, McpError>;

const PAGE_CURSOR_PREFIX: &str = "p1:";

impl Core {
    pub fn new(config: ConfigHandle) -> Result<Arc<Core>, String> {
        let cfg = config.get();
        let cache = Cache::new(CacheConfig {
            max_bytes: cfg.cache_max_bytes,
            max_entries: cfg.cache_max_entries,
            max_stale_on_error: Duration::from_secs(cfg.max_stale_on_error_secs),
            persist_path: cfg.cache_file.clone(),
            persist_interval: Duration::from_secs(cfg.cache_persist_interval_secs.max(1)),
        });
        let steam = SteamClient::new(config.clone(), cache)?;
        let hub = Arc::new(Hub::new());
        let tasks = TaskStore::new();
        let h = hub.clone();
        tasks.set_notifier(Arc::new(move |id: &str, owner: &str, task: Value| {
            h.notify_task(id, owner, task);
        }));
        Ok(Arc::new(Core {
            sealer: StateSealer::new(cfg.state_secret.clone()),
            limiter: KeyedLimiter::new(cfg.client_rps, cfg.client_burst),
            tools: all_tools(),
            prompts: prompts::prompts(),
            started: Instant::now(),
            stats: ServerStats::default(),
            stop: Arc::new(AtomicBool::new(false)),
            inflight: AtomicU64::new(0),
            persisted: AtomicBool::new(false),
            watch_state: Mutex::new(WatchState::default()),
            config,
            steam,
            hub,
            tasks,
        }))
    }

    /// A context for server-internal reads (watchers, metrics).
    pub fn internal_ctx(&self) -> RequestContext {
        let meta = RequestMeta {
            protocol_version: PROTOCOL_VERSION.into(),
            client_capabilities: Value::object(),
            ..Default::default()
        };
        RequestContext::new(
            None,
            "internal",
            meta,
            Principal::local(),
            Transport::Internal,
            Arc::new(NullSink),
            CancelToken::new(),
        )
    }

    // -----------------------------------------------------------------------
    // Entry point
    // -----------------------------------------------------------------------

    /// Handles one inbound JSON-RPC message.
    pub fn handle_message(
        self: &Arc<Self>,
        msg: &Value,
        principal: Principal,
        transport: Transport,
        sink: Arc<dyn Sink>,
        cancel: CancelToken,
    ) -> Reply {
        let Some(m) = msg.as_object() else {
            {
                let e = McpError::invalid_request(
                    "A JSON-RPC message must be an object (batches are not supported)",
                );
                return Reply::Response(error_response(None, &e), e.http_status);
            }
        };
        let id = m.get("id").filter(|v| !v.is_null()).cloned();
        let valid_id = id.as_ref().filter(|i| valid_request_id(i)).cloned();
        if m.get("jsonrpc").and_then(|v| v.as_str()) != Some(JSONRPC_VERSION) {
            {
                let e = McpError::invalid_request("jsonrpc must be \"2.0\"");
                return Reply::Response(error_response(valid_id.as_ref(), &e), e.http_status);
            }
        }
        let Some(method) = m.get("method").and_then(|v| v.as_str()) else {
            if m.contains_key("result") || m.contains_key("error") {
                crate::log_debug!("mcp", "ignoring JSON-RPC response sent by client");
                return Reply::NoResponse;
            }
            {
                let e = McpError::invalid_request("missing method");
                return Reply::Response(error_response(valid_id.as_ref(), &e), e.http_status);
            }
        };
        let params = m.get("params");
        if params.is_some_and(|p| !p.is_object() && !p.is_null()) {
            {
                let e = McpError::invalid_request("params must be an object");
                return Reply::Response(error_response(valid_id.as_ref(), &e), e.http_status);
            }
        }
        let Some(id) = id else {
            self.stats.notifications.fetch_add(1, Ordering::Relaxed);
            self.handle_notification(method, params);
            return Reply::NoResponse;
        };
        if valid_id.is_none() {
            {
                let e = McpError::invalid_request("id must be a string or integer");
                return Reply::Response(error_response(None, &e), e.http_status);
            }
        }
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        *self
            .stats
            .by_method
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(method.to_string())
            .or_default() += 1;

        if method == "initialize" {
            // Legacy (handshake-based) clients: name what we support.
            let requested = params
                .and_then(|p| p["protocolVersion"].as_str())
                .unwrap_or("unknown");
            let mut e = McpError::unsupported_version(requested);
            e.message = format!(
                "This server implements the stateless MCP revision {PROTOCOL_VERSION} only; the initialize handshake is not supported. Supported versions: {}",
                SUPPORTED_VERSIONS.join(", ")
            );
            self.stats.errors.fetch_add(1, Ordering::Relaxed);
            return Reply::Response(error_response(Some(&id), &e), e.http_status);
        }

        let meta = match RequestMeta::parse(params) {
            Ok(m) => m,
            Err(e) => {
                self.stats.errors.fetch_add(1, Ordering::Relaxed);
                return Reply::Response(error_response(Some(&id), &e), e.http_status);
            }
        };
        if let Err(wait) = self.limiter.check(&principal.id) {
            self.stats.errors.fetch_add(1, Ordering::Relaxed);
            {
                let e = McpError::rate_limited(wait.as_millis() as u64 + 1);
                return Reply::Response(error_response(Some(&id), &e), e.http_status);
            }
        }
        if self.stop.load(Ordering::SeqCst) {
            let e = McpError::internal("Server is shutting down; retry against another instance")
                .http(503);
            return Reply::Response(error_response(Some(&id), &e), e.http_status);
        }
        self.inflight.fetch_add(1, Ordering::SeqCst);
        let _inflight = InflightGuard(&self.inflight);
        let ctx = RequestContext::new(
            Some(id.clone()),
            method,
            meta,
            principal,
            transport,
            sink,
            cancel,
        );
        let started = Instant::now();
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.dispatch(&ctx, params)))
                .unwrap_or_else(|_| Err(McpError::internal("internal server error")));
        let elapsed = started.elapsed().as_millis();
        if ctx.is_cancelled() {
            crate::log_debug!(
                "mcp",
                "request cancelled; response suppressed",
                method = method,
                ms = elapsed
            );
            return Reply::NoResponse;
        }
        match outcome {
            Ok(Dispatched::Result(v)) => Reply::Response(result_response(&id, v), 200),
            Ok(Dispatched::Subscription(s)) => Reply::Subscription(s),
            Err(e) => {
                self.stats.errors.fetch_add(1, Ordering::Relaxed);
                crate::log_debug!(
                    "mcp",
                    "request failed",
                    method = method,
                    code = e.code,
                    error = e.message
                );
                Reply::Response(error_response(Some(&id), &e), e.http_status)
            }
        }
    }

    fn handle_notification(&self, method: &str, params: Option<&Value>) {
        match method {
            "notifications/cancelled" => {
                if let Some(id) = params.and_then(|p| p.get("requestId")) {
                    // Transports cancel in-flight work; subscriptions end here.
                    if self.hub.cancel(id) {
                        crate::log_debug!("mcp", "subscription cancelled by client", id = id);
                    }
                }
            }
            other => crate::log_debug!("mcp", "ignoring notification", method = other),
        }
    }

    fn dispatch(self: &Arc<Self>, ctx: &RequestContext, params: Option<&Value>) -> Res {
        let empty = Value::object();
        let p = params.unwrap_or(&empty);
        match ctx.method.as_str() {
            "server/discover" => Ok(Dispatched::Result(self.discover())),
            "tools/list" => self.list_tools(ctx, p),
            "tools/call" => self.call_tool(ctx, p),
            "resources/list" => self.list_resources(ctx, p),
            "resources/templates/list" => self.list_templates(ctx, p),
            "resources/read" => self.read_resource(ctx, p),
            "prompts/list" => self.list_prompts(ctx, p),
            "prompts/get" => self.get_prompt(ctx, p),
            "completion/complete" => self.complete(ctx, p),
            "subscriptions/listen" => self.listen(ctx, p),
            "tasks/get" | "tasks/update" | "tasks/cancel" => self.task_method(ctx, p),
            other => Err(McpError::method_not_found(other)),
        }
    }

    // -----------------------------------------------------------------------
    // Discovery
    // -----------------------------------------------------------------------

    pub fn capabilities(&self) -> Value {
        obj! {
            "tools" => obj! { "listChanged" => true },
            "resources" => obj! { "subscribe" => true, "listChanged" => true },
            "prompts" => obj! { "listChanged" => true },
            "completions" => obj! {},
            "logging" => obj! {},
            "extensions" => obj! { EXT_TASKS => obj! {} },
        }
    }

    pub fn instructions(&self) -> String {
        let cfg = self.config.get();
        let mut s = String::from(
            "Steam data server. Use steam_search_apps to turn game names into app IDs, then steam_get_app_overview for a one-call snapshot or the specific steam_get_* tools for detail. \
             Player tools take 64-bit SteamIDs (resolve profile names with steam_resolve_vanity_url). \
             All data is cached with per-category freshness (live player counts ~1 min, news/reviews ~15 min, store pages ~1 h, schemas ~1 day), so repeated calls are cheap; pass force_refresh only when you need the very latest value. \
             Resources mirror the main tools under steam:// URIs and can be subscribed to for change notifications.",
        );
        if cfg.has_partner() {
            s.push_str(" A Steamworks publisher key is configured: steam_partner_* tools expose partner data (apps, builds, branches, ownership, leaderboards, microtransactions, cheating reports, inventory).");
        }
        if cfg.has_financial() {
            s.push_str(" A Financial API key is configured: use steam_partner_get_changed_sales_dates, then steam_partner_sales_summary or steam_partner_get_detailed_sales.");
        }
        if cfg.public_key().is_none() {
            s.push_str(
                " No Steam Web API key is configured, so player-profile tools are unavailable.",
            );
        }
        s
    }

    fn discover(&self) -> Value {
        obj! {
            "supportedVersions" => SUPPORTED_VERSIONS.to_vec(),
            "capabilities" => self.capabilities(),
            "instructions" => self.instructions(),
            "ttlMs" => 3_600_000u64,
            "cacheScope" => "public",
            "_meta" => obj! { META_SERVER_INFO => server_info() },
        }
    }

    // -----------------------------------------------------------------------
    // Pagination
    // -----------------------------------------------------------------------

    fn paginate(
        &self,
        items: Vec<Value>,
        params: &Value,
    ) -> Result<(Vec<Value>, Option<String>), McpError> {
        let page = self.config.get().page_size.max(1);
        let offset = match params.get("cursor") {
            None | Some(Value::Null) => 0,
            Some(Value::String(c)) => {
                let decoded = b64_decode(c).and_then(|b| String::from_utf8(b).ok());
                decoded
                    .as_deref()
                    .and_then(|d| d.strip_prefix(PAGE_CURSOR_PREFIX))
                    .and_then(|n| n.parse::<usize>().ok())
                    .filter(|n| *n <= items.len())
                    .ok_or_else(|| McpError::invalid_params("Invalid cursor"))?
            }
            Some(_) => return Err(McpError::invalid_params("Invalid cursor: must be a string")),
        };
        let end = (offset + page).min(items.len());
        let next = (end < items.len())
            .then(|| b64url_encode(format!("{PAGE_CURSOR_PREFIX}{end}").as_bytes()));
        Ok((items[offset..end].to_vec(), next))
    }

    fn list_result(
        &self,
        key: &str,
        items: Vec<Value>,
        params: &Value,
        ttl_ms: u64,
        scope: &str,
    ) -> Res {
        let (page, next) = self.paginate(items, params)?;
        let mut r = obj! { key => page, "ttlMs" => ttl_ms, "cacheScope" => scope };
        if let Some(n) = next {
            r.set("nextCursor", n);
        }
        Ok(Dispatched::Result(r))
    }

    // -----------------------------------------------------------------------
    // Tools
    // -----------------------------------------------------------------------

    /// Whether a tool is offered under the current configuration (ignoring scopes).
    pub fn tool_enabled(&self, t: &ToolDef) -> bool {
        let cfg = self.config.get();
        !cfg.tool_disabled(t.name) && (!t.write || cfg.enable_writes) && self.steam.has_key(t.key)
    }

    pub fn visible_tools(&self, principal: &Principal) -> Vec<&ToolDef> {
        self.tools
            .iter()
            .filter(|t| self.tool_enabled(t) && principal.has_scope(t.scope))
            .collect()
    }

    pub fn find_tool(&self, name: &str) -> Option<&ToolDef> {
        self.tools.iter().find(|t| t.name == name)
    }

    fn list_tools(&self, ctx: &RequestContext, p: &Value) -> Res {
        let items: Vec<Value> = self
            .visible_tools(&ctx.principal)
            .iter()
            .map(|t| t.to_json())
            .collect();
        self.list_result("tools", items, p, 300_000, resources::list_scope(self))
    }

    fn insufficient_scope(scope: &str) -> McpError {
        McpError::new(
            FORBIDDEN,
            format!("Insufficient scope: this operation requires {scope}"),
        )
        .with_data(obj! { "requiredScope" => scope })
        .http(403)
    }

    fn call_tool(self: &Arc<Self>, ctx: &RequestContext, p: &Value) -> Res {
        let name = p["name"]
            .as_str()
            .ok_or_else(|| McpError::invalid_params("tools/call requires a string 'name'"))?;
        let args_value = match p.get("arguments") {
            None | Some(Value::Null) => Value::object(),
            Some(v @ Value::Object(_)) => v.clone(),
            Some(_) => return Err(McpError::invalid_params("'arguments' must be an object")),
        };
        let tool = self
            .find_tool(name)
            .filter(|t| self.tool_enabled(t))
            .ok_or_else(|| McpError::invalid_params(format!("Unknown tool: {name}")))?;
        if !ctx.principal.has_scope(tool.scope) {
            return Err(Self::insufficient_scope(tool.scope));
        }
        self.stats.tool_calls.fetch_add(1, Ordering::Relaxed);

        // MRTR inputs.
        let responses = match p.get("inputResponses") {
            None | Some(Value::Null) => None,
            Some(Value::Object(m)) if m.values().all(|v| v.is_object()) => Some(m.clone()),
            Some(_) => {
                return Err(McpError::invalid_params(
                    "'inputResponses' must be an object mapping keys to result objects",
                ));
            }
        };
        let salient = obj! { "name" => name, "arguments" => args_value.clone() };
        let binding = Binding {
            method: "tools/call",
            salient: &salient,
            principal: &ctx.principal.id,
        };
        let state = match p.get("requestState") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(self.sealer.open(s, &binding)?),
            Some(_) => return Err(McpError::invalid_params("'requestState' must be a string")),
        };

        // Validate (after lenient coercion) — failures are tool execution errors.
        let mut args_value = args_value;
        schema::coerce(&tool.input_schema, &mut args_value);
        if let Err(errs) = schema::validate(&tool.input_schema, &args_value) {
            self.stats.tool_errors.fetch_add(1, Ordering::Relaxed);
            return Ok(Dispatched::Result(obj! {
                "content" => crate::arr![obj! { "type" => "text", "text" => format!("Invalid arguments for {name}: {}", errs.join("; ")) }],
                "isError" => true,
            }));
        }
        let args = args_value.as_object().cloned().unwrap_or_default();
        ctx.log(
            LogLevel::Debug,
            "tools",
            obj! { "tool" => name, "event" => "start" },
        );
        let started = Instant::now();

        let outcome = if tool.task && ctx.supports_tasks() && responses.is_none() && state.is_none()
        {
            return Ok(Dispatched::Result(self.run_as_task(ctx, tool, args)?));
        } else {
            let env = ToolEnv {
                core: self,
                ctx,
                mrtr: Mrtr { responses, state },
                task: None,
            };
            (tool.handler)(&env, &args)
        };
        let elapsed = started.elapsed().as_millis();
        match outcome {
            ToolOutcome::Done(r) => {
                let is_error = r["isError"].as_bool() == Some(true);
                if is_error {
                    self.stats.tool_errors.fetch_add(1, Ordering::Relaxed);
                }
                crate::log_info!(
                    "tools",
                    "tool call",
                    tool = name,
                    principal = ctx.principal.id,
                    ms = elapsed,
                    error = is_error
                );
                ctx.log(if is_error { LogLevel::Warning } else { LogLevel::Info }, "tools", obj! { "tool" => name, "event" => "done", "ms" => elapsed as u64, "isError" => is_error });
                Ok(Dispatched::Result(r))
            }
            ToolOutcome::InputRequired { requests, state } => {
                self.check_input_requests(ctx, &requests)?;
                self.stats.input_required.fetch_add(1, Ordering::Relaxed);
                let mut r = obj! { "resultType" => "input_required", "inputRequests" => Value::Object(requests) };
                if let Some(s) = state {
                    r.set(
                        "requestState",
                        self.sealer.seal(&binding, s, Duration::from_secs(600)),
                    );
                }
                Ok(Dispatched::Result(r))
            }
            ToolOutcome::Error(e) => Err(e),
        }
    }

    /// Servers must not send input requests the client has not declared.
    fn check_input_requests(&self, ctx: &RequestContext, requests: &Map) -> Result<(), McpError> {
        for (_, req) in requests.iter() {
            match req["method"].as_str() {
                Some("elicitation/create") => {
                    let url = req.at(&["params", "mode"]).as_str() == Some("url");
                    if url && !ctx.supports_elicitation_url() {
                        return Err(McpError::missing_capability(
                            obj! { "elicitation" => obj! { "url" => obj! {} } },
                            "URL-mode elicitation",
                        ));
                    }
                    if !url && !ctx.supports_elicitation_form() {
                        return Err(McpError::missing_capability(
                            obj! { "elicitation" => obj! { "form" => obj! {} } },
                            "form-mode elicitation",
                        ));
                    }
                }
                Some("sampling/createMessage") if !ctx.supports_sampling() => {
                    return Err(McpError::missing_capability(
                        obj! { "sampling" => obj! {} },
                        "sampling",
                    ));
                }
                Some("roots/list") if !ctx.supports_roots() => {
                    return Err(McpError::missing_capability(
                        obj! { "roots" => obj! {} },
                        "roots",
                    ));
                }
                Some("sampling/createMessage" | "roots/list") => {}
                _ => return Err(McpError::internal("invalid input request generated")),
            }
        }
        Ok(())
    }

    /// Runs a task-capable tool in the background. Returns the final result if
    /// it completes within the synchronous window, otherwise a CreateTaskResult.
    fn run_as_task(
        self: &Arc<Self>,
        ctx: &RequestContext,
        tool: &ToolDef,
        args: Map,
    ) -> Result<Value, McpError> {
        let cfg = self.config.get();
        let task = self.tasks.create(
            &ctx.principal.id,
            Duration::from_secs(cfg.task_ttl_secs.max(60)),
            Duration::from_millis(1000),
        );
        self.stats.tasks_created.fetch_add(1, Ordering::Relaxed);
        let core = self.clone();
        let worker_task = task.clone();
        let worker_ctx = ctx.detached(task.cancel_token());
        let name = tool.name;
        let spawned = std::thread::Builder::new()
            .name(format!("task-{name}"))
            .spawn(move || {
                let Some(tool) = core.find_tool(name) else {
                    return;
                };
                let env = ToolEnv {
                    core: &core,
                    ctx: &worker_ctx,
                    mrtr: Mrtr::default(),
                    task: Some(worker_task.clone()),
                };
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (tool.handler)(&env, &args)
                }));
                match outcome {
                    Ok(ToolOutcome::Done(mut r)) => {
                        if let Value::Object(m) = &mut r {
                            m.insert("resultType", "complete");
                        }
                        worker_task.complete(r)
                    }
                    Ok(ToolOutcome::InputRequired { .. }) => worker_task.fail(
                        McpError::internal("task-capable tool requested synchronous input")
                            .to_json(),
                    ),
                    Ok(ToolOutcome::Error(e)) => worker_task.fail(e.to_json()),
                    Err(_) => worker_task.fail(McpError::internal("tool panicked").to_json()),
                }
                crate::log_info!(
                    "tasks",
                    "task finished",
                    task = worker_task.id,
                    tool = name,
                    status = worker_task.status().as_str()
                );
            });
        if spawned.is_err() {
            return Err(McpError::internal("could not start task worker"));
        }
        if task.wait_terminal(Duration::from_millis(cfg.task_sync_window_ms)) {
            match task.status() {
                Status::Completed => return Ok(task.result().unwrap_or_else(Value::object)),
                Status::Failed => {
                    let e = task.to_json(true)["error"].clone();
                    return Err(McpError::new(
                        e["code"].as_i64().unwrap_or(INTERNAL_ERROR),
                        e["message"].as_str().unwrap_or("task failed"),
                    ));
                }
                _ => {}
            }
        }
        let mut r = task.to_json(false);
        r.set("resultType", "task");
        crate::log_info!(
            "tasks",
            "tool call continues as task",
            task = task.id,
            tool = tool.name,
            principal = ctx.principal.id
        );
        Ok(r)
    }

    // -----------------------------------------------------------------------
    // Resources
    // -----------------------------------------------------------------------

    fn list_resources(self: &Arc<Self>, ctx: &RequestContext, p: &Value) -> Res {
        let items = resources::list(self, ctx);
        self.list_result("resources", items, p, 300_000, "private")
    }

    fn list_templates(self: &Arc<Self>, ctx: &RequestContext, p: &Value) -> Res {
        let items = resources::list_templates(self, ctx);
        self.list_result(
            "resourceTemplates",
            items,
            p,
            3_600_000,
            resources::list_scope(self),
        )
    }

    fn read_resource(self: &Arc<Self>, ctx: &RequestContext, p: &Value) -> Res {
        let uri = p["uri"]
            .as_str()
            .ok_or_else(|| McpError::invalid_params("resources/read requires a string 'uri'"))?;
        if uri.len() > 2048 {
            return Err(McpError::invalid_params("Resource not found: URI too long"));
        }
        let r = resources::read(self, uri, ctx)?;
        Ok(Dispatched::Result(
            obj! { "contents" => r.contents, "ttlMs" => r.ttl_ms, "cacheScope" => r.cache_scope },
        ))
    }

    // -----------------------------------------------------------------------
    // Prompts
    // -----------------------------------------------------------------------

    fn list_prompts(&self, ctx: &RequestContext, p: &Value) -> Res {
        let items: Vec<Value> = prompts::visible(self, ctx, &self.prompts)
            .into_iter()
            .map(prompts::to_json)
            .collect();
        self.list_result("prompts", items, p, 3_600_000, resources::list_scope(self))
    }

    fn get_prompt(self: &Arc<Self>, ctx: &RequestContext, p: &Value) -> Res {
        let name = p["name"]
            .as_str()
            .ok_or_else(|| McpError::invalid_params("prompts/get requires a string 'name'"))?;
        let visible = prompts::visible(self, ctx, &self.prompts);
        let def = visible
            .into_iter()
            .find(|d| d.name == name)
            .ok_or_else(|| McpError::invalid_params(format!("Unknown prompt: {name}")))?;
        let raw_args = match p.get("arguments") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(m)) => m.clone(),
            Some(_) => {
                return Err(McpError::invalid_params(
                    "'arguments' must be an object of strings",
                ));
            }
        };
        let mut args: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in raw_args.iter() {
            match v {
                Value::String(s) => {
                    args.insert(k.clone(), s.clone());
                }
                _ => {
                    return Err(McpError::invalid_params(format!(
                        "Prompt argument '{k}' must be a string"
                    )));
                }
            }
        }
        let salient = obj! { "name" => name, "arguments" => Value::Object(raw_args) };
        let binding = Binding {
            method: "prompts/get",
            salient: &salient,
            principal: &ctx.principal.id,
        };
        if let Some(Value::String(s)) = p.get("requestState") {
            let state = self.sealer.open(s, &binding)?;
            if state["step"].as_str() == Some("prompt_args") {
                if let Some(resp) = p
                    .get("inputResponses")
                    .and_then(|r| r.get("prompt_arguments"))
                {
                    if resp["action"].as_str() == Some("accept") {
                        if let Some(content) = resp["content"].as_object() {
                            for (k, v) in content.iter() {
                                if def.args.iter().any(|a| a.name == k) {
                                    args.insert(k.clone(), v.to_param_string());
                                }
                            }
                        }
                    } else {
                        return Err(McpError::invalid_params(
                            "Required prompt arguments were not provided",
                        ));
                    }
                }
            }
        }
        match prompts::get(self, ctx, def, &args)? {
            prompts::PromptOutcome::Done(v) => Ok(Dispatched::Result(v)),
            prompts::PromptOutcome::NeedArgs(missing) => {
                if ctx.supports_elicitation_form() {
                    let requests = prompts::elicit_missing(def, &missing);
                    let state = self.sealer.seal(
                        &binding,
                        obj! { "step" => "prompt_args" },
                        Duration::from_secs(600),
                    );
                    Ok(Dispatched::Result(
                        obj! { "resultType" => "input_required", "inputRequests" => Value::Object(requests), "requestState" => state },
                    ))
                } else {
                    Err(McpError::invalid_params(format!(
                        "Missing required arguments: {}",
                        missing.join(", ")
                    )))
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Completion
    // -----------------------------------------------------------------------

    fn complete(self: &Arc<Self>, ctx: &RequestContext, p: &Value) -> Res {
        let r = &p["ref"];
        let arg_name = p.at(&["argument", "name"]).as_str().ok_or_else(|| {
            McpError::invalid_params("completion/complete requires argument.name")
        })?;
        let value = p.at(&["argument", "value"]).as_str().ok_or_else(|| {
            McpError::invalid_params("completion/complete requires argument.value")
        })?;
        let known = match r["type"].as_str() {
            Some("ref/prompt") => {
                let name = r["name"]
                    .as_str()
                    .ok_or_else(|| McpError::invalid_params("ref/prompt requires 'name'"))?;
                let def = prompts::visible(self, ctx, &self.prompts)
                    .into_iter()
                    .find(|d| d.name == name)
                    .ok_or_else(|| McpError::invalid_params(format!("Unknown prompt: {name}")))?;
                def.args.iter().any(|a| a.name == arg_name)
            }
            Some("ref/resource") => {
                let uri = r["uri"]
                    .as_str()
                    .ok_or_else(|| McpError::invalid_params("ref/resource requires 'uri'"))?;
                let tpl = resources::templates()
                    .into_iter()
                    .find(|t| t.template == uri);
                match tpl {
                    Some(t) => crate::util::uritemplate::UriTemplate::parse(t.template)
                        .is_some_and(|u| u.variables().contains(&arg_name)),
                    None if resources::resolve(uri).is_some() => false,
                    None => {
                        return Err(McpError::invalid_params(format!(
                            "Unknown resource or template: {uri}"
                        )));
                    }
                }
            }
            _ => {
                return Err(McpError::invalid_params(
                    "ref.type must be ref/prompt or ref/resource",
                ));
            }
        };
        let (values, total, has_more) = if known {
            completion::complete(self, ctx, arg_name, value)
        } else {
            (Vec::new(), 0, false)
        };
        Ok(Dispatched::Result(
            obj! { "completion" => obj! { "values" => values, "total" => total, "hasMore" => has_more } },
        ))
    }

    // -----------------------------------------------------------------------
    // Subscriptions
    // -----------------------------------------------------------------------

    fn listen(self: &Arc<Self>, ctx: &RequestContext, p: &Value) -> Res {
        let n = p
            .get("notifications")
            .and_then(|v| v.as_object())
            .ok_or_else(|| {
                McpError::invalid_params(
                    "subscriptions/listen requires a 'notifications' filter object",
                )
            })?;
        let flag = |k: &str| n.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
        let mut filter = Filter {
            tools_list_changed: flag("toolsListChanged"),
            prompts_list_changed: flag("promptsListChanged"),
            resources_list_changed: flag("resourcesListChanged"),
            ..Default::default()
        };
        if let Some(uris) = n.get("resourceSubscriptions") {
            let list = uris.as_array().ok_or_else(|| {
                McpError::invalid_params("resourceSubscriptions must be an array of URIs")
            })?;
            for u in list.iter().filter_map(|u| u.as_str()).take(200) {
                let ok = match resources::resolve(u) {
                    Some(resources::Resolved::Static(s)) => resources::available(self, s.tool, ctx),
                    Some(resources::Resolved::Template(t, vars)) => {
                        resources::available(self, t.tool, ctx) && (t.args)(&vars).is_ok()
                    }
                    _ => false,
                };
                if ok && !filter.resource_uris.iter().any(|x| x == u) {
                    filter.resource_uris.push(u.to_string());
                }
            }
        }
        let task_ids: Vec<String> = n
            .get("taskIds")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if n.contains_key("taskIds") {
            if !ctx.supports_tasks() {
                return Err(McpError::missing_capability(
                    obj! { "extensions" => obj! { EXT_TASKS => obj! {} } },
                    "task status notifications require the tasks extension",
                ));
            }
            filter.task_ids = task_ids
                .into_iter()
                .filter(|t| self.tasks.get(t, &ctx.principal.id).is_some())
                .collect();
        }
        let id = ctx.id.clone().unwrap_or(Value::Null);
        let sub = Subscription::new(id, filter, &ctx.principal.id, ctx.sink.clone());
        sub.acknowledge(ctx.supports_tasks());
        self.hub.add(sub.clone());
        crate::log_info!(
            "subscriptions",
            "subscription opened",
            principal = ctx.principal.id,
            filter = sub.filter.to_json(true)
        );
        // Prime change detection so the first notification reflects a real change.
        self.prime_watch(&sub.filter);
        Ok(Dispatched::Subscription(sub))
    }

    // -----------------------------------------------------------------------
    // Tasks extension
    // -----------------------------------------------------------------------

    fn task_method(&self, ctx: &RequestContext, p: &Value) -> Res {
        if !ctx.supports_tasks() {
            return Err(McpError::missing_capability(
                obj! { "extensions" => obj! { EXT_TASKS => obj! {} } },
                "the tasks extension",
            ));
        }
        let id = p["taskId"]
            .as_str()
            .ok_or_else(|| McpError::invalid_params("taskId is required"))?;
        let task = self
            .tasks
            .get(id, &ctx.principal.id)
            .ok_or_else(|| McpError::invalid_params(format!("Unknown or expired task: {id}")))?;
        match ctx.method.as_str() {
            "tasks/get" => Ok(Dispatched::Result(task.to_json(true))),
            "tasks/update" => {
                let responses = p
                    .get("inputResponses")
                    .and_then(|v| v.as_object())
                    .filter(|m| m.values().all(|v| v.is_object()))
                    .ok_or_else(|| {
                        McpError::invalid_params("tasks/update requires an 'inputResponses' object")
                    })?;
                task.update_inputs(responses);
                Ok(Dispatched::Result(Value::object()))
            }
            _ => {
                task.request_cancel();
                Ok(Dispatched::Result(Value::object()))
            }
        }
    }

    // -----------------------------------------------------------------------
    // Background work: change detection and config reload
    // -----------------------------------------------------------------------

    fn names_hash(names: impl Iterator<Item = String>) -> String {
        let joined: Vec<String> = names.collect();
        crate::util::crypto::sha256_hex(joined.join("\n").as_bytes())
    }

    fn current_tools_hash(&self) -> String {
        let admin = Principal::local();
        Self::names_hash(
            self.visible_tools(&admin)
                .iter()
                .map(|t| t.name.to_string()),
        )
    }

    fn current_prompts_hash(self: &Arc<Self>) -> String {
        let ctx = self.internal_ctx();
        Self::names_hash(
            prompts::visible(self, &ctx, &self.prompts)
                .iter()
                .map(|p| p.name.to_string()),
        )
    }

    fn current_resource_list_hash(self: &Arc<Self>) -> String {
        let ctx = self.internal_ctx();
        Self::names_hash(resources::list(self, &ctx).iter().map(|r| r.to_string()))
    }

    fn prime_watch(self: &Arc<Self>, filter: &Filter) {
        let ctx = self.internal_ctx();
        let mut uris_to_read = Vec::new();
        {
            let w = self.watch_state.lock().unwrap_or_else(|e| e.into_inner());
            for u in &filter.resource_uris {
                if !w.resource_hashes.contains_key(u) {
                    uris_to_read.push(u.clone());
                }
            }
        }
        let mut new_hashes = Vec::new();
        for u in uris_to_read {
            if let Ok(r) = resources::read(self, &u, &ctx) {
                new_hashes.push((u, r.hash));
            }
        }
        let list_hash = if filter.resources_list_changed {
            Some(self.current_resource_list_hash())
        } else {
            None
        };
        let mut w = self.watch_state.lock().unwrap_or_else(|e| e.into_inner());
        for (u, h) in new_hashes {
            w.resource_hashes.entry(u).or_insert(h);
        }
        if let Some(h) = list_hash {
            w.resource_list_hash.get_or_insert(h);
        }
        if w.tools_hash.is_none() {
            w.tools_hash = Some(self.current_tools_hash());
        }
        drop(w);
        let ph = self.current_prompts_hash();
        self.watch_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .prompts_hash
            .get_or_insert(ph);
    }

    /// One pass of change detection. Public for tests.
    pub fn watch_tick(self: &Arc<Self>) {
        self.tasks.purge();
        // Config hot reload.
        let cfg = self.config.get();
        if let Some(path) = &cfg.config_file {
            let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
            let changed = {
                let mut w = self.watch_state.lock().unwrap_or_else(|e| e.into_inner());
                let changed =
                    w.config_mtime.is_some() && mtime.is_some() && w.config_mtime != mtime;
                if mtime.is_some() {
                    w.config_mtime = mtime;
                }
                changed
            };
            if changed {
                match self.config.reload() {
                    Ok(c) => {
                        if let Some(l) = crate::util::log::Level::parse(&c.log_level) {
                            crate::util::log::set_level(l);
                        }
                        crate::log_info!("config", "configuration reloaded", path = path.display());
                    }
                    Err(e) => crate::log_error!(
                        "config",
                        "configuration reload failed; keeping previous settings",
                        error = e
                    ),
                }
            }
        }
        // Tool and prompt list changes.
        let th = self.current_tools_hash();
        let ph = self.current_prompts_hash();
        let (tools_changed, prompts_changed) = {
            let mut w = self.watch_state.lock().unwrap_or_else(|e| e.into_inner());
            let t = w.tools_hash.as_ref().is_some_and(|h| *h != th);
            let p = w.prompts_hash.as_ref().is_some_and(|h| *h != ph);
            w.tools_hash = Some(th);
            w.prompts_hash = Some(ph);
            (t, p)
        };
        if tools_changed {
            let n = self.hub.notify_tools_changed();
            crate::log_info!("subscriptions", "tool list changed", notified = n);
        }
        if prompts_changed {
            self.hub.notify_prompts_changed();
        }
        if self.hub.is_empty() {
            return;
        }
        let ctx = self.internal_ctx();
        // Resource list changes.
        if self.hub.wants(|f| f.resources_list_changed) {
            let h = self.current_resource_list_hash();
            let changed = {
                let mut w = self.watch_state.lock().unwrap_or_else(|e| e.into_inner());
                let c = w.resource_list_hash.as_ref().is_some_and(|old| *old != h);
                w.resource_list_hash = Some(h);
                c
            };
            if changed {
                self.hub.notify_resources_changed();
            }
        }
        // Resource content changes (reads are served from cache; stale
        // entries refresh in the background and are picked up next tick).
        for uri in self.hub.watched_uris() {
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            let Ok(r) = resources::read(self, &uri, &ctx) else {
                continue;
            };
            let changed = {
                let mut w = self.watch_state.lock().unwrap_or_else(|e| e.into_inner());
                let prev = w.resource_hashes.insert(uri.clone(), r.hash.clone());
                prev.is_some_and(|p| p != r.hash)
            };
            if changed {
                let n = self.hub.notify_resource_updated(&uri);
                crate::log_debug!("subscriptions", "resource updated", uri = uri, notified = n);
            }
        }
    }

    /// Starts cache maintenance and the change watcher.
    pub fn start_background(self: &Arc<Self>) {
        self.steam.cache.start_maintenance(self.stop.clone());
        {
            let mut w = self.watch_state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(p) = &self.config.get().config_file {
                w.config_mtime = std::fs::metadata(p).and_then(|m| m.modified()).ok();
            }
        }
        let core = self.clone();
        let _ = std::thread::Builder::new()
            .name("mcp-watch".into())
            .spawn(move || {
                let mut elapsed = Duration::ZERO;
                while !core.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(250));
                    elapsed += Duration::from_millis(250);
                    if elapsed >= Duration::from_secs(core.config.get().watch_interval_secs) {
                        elapsed = Duration::ZERO;
                        let c = core.clone();
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                            c.watch_tick()
                        }))
                        .is_err()
                        {
                            crate::log_error!("mcp", "watcher iteration panicked");
                        }
                    }
                }
            });
    }

    /// Graceful shutdown: end subscriptions, cancel tasks, persist the cache.
    pub fn shutdown(&self) {
        self.begin_shutdown();
        self.finish_shutdown();
    }

    /// Phase 1: stop accepting work, end subscriptions gracefully and cancel
    /// tasks. In-flight requests keep running.
    pub fn begin_shutdown(&self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        crate::log_info!(
            "main",
            "shutting down",
            inflight = self.inflight.load(Ordering::SeqCst),
            subscriptions = self.hub.len()
        );
        self.hub.shutdown();
        self.tasks.cancel_all();
    }

    /// Waits until no request is in flight or `grace` elapses. Returns the
    /// number of requests still running.
    pub fn drain(&self, grace: Duration) -> u64 {
        let deadline = Instant::now() + grace;
        loop {
            let n = self.inflight.load(Ordering::SeqCst);
            if n == 0 || Instant::now() >= deadline {
                if n > 0 {
                    crate::log_warn!(
                        "main",
                        "grace period elapsed with requests in flight",
                        inflight = n
                    );
                }
                return n;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Phase 2: persist the cache (once).
    pub fn finish_shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if self.persisted.swap(true, Ordering::SeqCst) {
            return;
        }
        match self.steam.cache.persist() {
            Ok(n) if n > 0 => crate::log_info!("cache", "persisted cache on shutdown", entries = n),
            Ok(_) => {}
            Err(e) => crate::log_warn!("cache", "could not persist cache on shutdown", error = e),
        }
    }

    // -----------------------------------------------------------------------
    // Observability
    // -----------------------------------------------------------------------

    /// Prometheus text exposition.
    pub fn metrics_text(&self) -> String {
        let c = self.steam.cache.stats_json();
        let u = self.steam.stats_json();
        let s = &self.stats;
        let mut out = String::new();
        let mut metric = |name: &str, help: &str, kind: &str, value: String| {
            out.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}\n"
            ));
        };
        metric(
            "steam_mcp_uptime_seconds",
            "Seconds since start",
            "gauge",
            self.started.elapsed().as_secs().to_string(),
        );
        metric(
            "steam_mcp_requests_total",
            "JSON-RPC requests received",
            "counter",
            s.requests.load(Ordering::Relaxed).to_string(),
        );
        metric(
            "steam_mcp_request_errors_total",
            "JSON-RPC error responses",
            "counter",
            s.errors.load(Ordering::Relaxed).to_string(),
        );
        metric(
            "steam_mcp_tool_calls_total",
            "tools/call requests",
            "counter",
            s.tool_calls.load(Ordering::Relaxed).to_string(),
        );
        metric(
            "steam_mcp_tool_errors_total",
            "Tool results with isError",
            "counter",
            s.tool_errors.load(Ordering::Relaxed).to_string(),
        );
        metric(
            "steam_mcp_tasks_created_total",
            "Tasks created",
            "counter",
            s.tasks_created.load(Ordering::Relaxed).to_string(),
        );
        metric(
            "steam_mcp_tasks_active",
            "Tasks retained",
            "gauge",
            self.tasks.len().to_string(),
        );
        metric(
            "steam_mcp_subscriptions_active",
            "Open subscriptions/listen streams",
            "gauge",
            self.hub.len().to_string(),
        );
        metric(
            "steam_mcp_cache_entries",
            "Cache entries",
            "gauge",
            c["entries"].to_string(),
        );
        metric(
            "steam_mcp_cache_bytes",
            "Approximate cache bytes",
            "gauge",
            c["approxBytes"].to_string(),
        );
        for (k, name) in [
            ("hits", "hits"),
            ("staleHits", "stale_hits"),
            ("misses", "misses"),
            ("coalesced", "coalesced"),
            ("staleOnError", "stale_on_error"),
            ("evictions", "evictions"),
            ("backgroundRefreshes", "refreshes"),
        ] {
            metric(
                &format!("steam_mcp_cache_{name}_total"),
                &format!("Cache {k}"),
                "counter",
                c[k].to_string(),
            );
        }
        metric(
            "steam_mcp_upstream_calls_total",
            "Calls made to Steam",
            "counter",
            u["upstreamCalls"].to_string(),
        );
        metric(
            "steam_mcp_upstream_errors_total",
            "Failed calls to Steam",
            "counter",
            u["upstreamErrors"].to_string(),
        );
        metric(
            "steam_mcp_upstream_avg_latency_ms",
            "Mean upstream latency",
            "gauge",
            u["avgLatencyMs"].to_string(),
        );
        metric(
            "steam_mcp_upstream_daily_budget_used",
            "Upstream calls today",
            "gauge",
            u["dailyBudgetUsed"].to_string(),
        );
        out
    }

    pub fn auth_enabled(&self) -> bool {
        self.config.get().auth.mode != AuthMode::None
    }

    /// Scopes the server can grant (for protected resource metadata).
    pub fn scopes_supported(&self) -> Vec<&'static str> {
        let cfg = self.config.get();
        let mut s = vec![scopes::PUBLIC];
        if cfg.has_partner() {
            s.push(scopes::PARTNER);
        }
        if cfg.has_financial() {
            s.push(scopes::FINANCIAL);
        }
        if cfg.enable_writes {
            s.push(scopes::WRITE);
        }
        s.push(scopes::ADMIN);
        s
    }

    pub fn tools_requiring(&self, key: KeyKind) -> usize {
        self.tools.iter().filter(|t| t.key == key).count()
    }
}
