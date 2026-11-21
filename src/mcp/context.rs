//! Per-request execution context: identity, capabilities, request-scoped
//! notifications (progress and logging) and cancellation.

use std::sync::{Arc, Mutex};

use super::protocol::{EXT_TASKS, LogLevel, RequestMeta, notification};
use crate::json::Value;
use crate::obj;
use crate::util::CancelToken;

/// Destination for messages scoped to one request (an SSE stream, stdout, …).
pub trait Sink: Send + Sync {
    /// Sends a message; returns false if the destination is gone.
    fn send(&self, msg: &Value) -> bool;

    /// Emits a transport keep-alive (SSE comment); false if the peer is gone.
    fn keepalive(&self) -> bool {
        true
    }
}

/// Discards everything (used for task workers and HTTP JSON responses).
pub struct NullSink;

impl Sink for NullSink {
    fn send(&self, _msg: &Value) -> bool {
        true
    }
}

/// Transport the request arrived on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Stdio,
    Http,
    /// In-process (tests, task workers).
    Internal,
}

/// OAuth-style scopes understood by the server, broadest first.
pub mod scopes {
    pub const ADMIN: &str = "steam:admin";
    pub const WRITE: &str = "steam:write";
    pub const FINANCIAL: &str = "steam:financial";
    pub const PARTNER: &str = "steam:partner";
    pub const PUBLIC: &str = "steam:public";
    pub const ALL: &[&str] = &[PUBLIC, PARTNER, FINANCIAL, WRITE, ADMIN];

    /// Scopes implied by holding `scope` (scope hierarchy).
    pub fn implied(scope: &str) -> &'static [&'static str] {
        match scope {
            ADMIN => &[ADMIN, WRITE, FINANCIAL, PARTNER, PUBLIC],
            WRITE => &[WRITE, PARTNER, PUBLIC],
            FINANCIAL => &[FINANCIAL, PUBLIC],
            PARTNER => &[PARTNER, PUBLIC],
            PUBLIC => &[PUBLIC],
            _ => &[],
        }
    }
}

/// The authenticated caller.
#[derive(Clone, Debug, PartialEq)]
pub struct Principal {
    pub id: String,
    pub scopes: Vec<String>,
    pub authenticated: bool,
}

impl Principal {
    /// Local/unauthenticated deployments are fully trusted.
    pub fn local() -> Principal {
        Principal { id: "local".into(), scopes: vec![scopes::ADMIN.into()], authenticated: false }
    }

    pub fn has_scope(&self, required: &str) -> bool {
        self.scopes.iter().any(|s| s == required || scopes::implied(s).contains(&required))
    }
}

/// Everything a handler needs to know about the request being served.
pub struct RequestContext {
    pub id: Option<Value>,
    pub method: String,
    pub meta: RequestMeta,
    pub principal: Principal,
    pub transport: Transport,
    pub sink: Arc<dyn Sink>,
    pub cancel: CancelToken,
    last_progress: Mutex<f64>,
}

impl RequestContext {
    pub fn new(id: Option<Value>, method: &str, meta: RequestMeta, principal: Principal, transport: Transport, sink: Arc<dyn Sink>, cancel: CancelToken) -> RequestContext {
        RequestContext { id, method: method.to_string(), meta, principal, transport, sink, cancel, last_progress: Mutex::new(f64::NEG_INFINITY) }
    }

    /// A detached context for background task execution: same identity and
    /// capabilities, but no request-scoped notifications.
    pub fn detached(&self, cancel: CancelToken) -> RequestContext {
        let mut meta = self.meta.clone();
        meta.progress_token = None;
        meta.log_level = None;
        RequestContext::new(None, &self.method, meta, self.principal.clone(), Transport::Internal, Arc::new(NullSink), cancel)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Emits `notifications/progress` if the request opted in. Progress values
    /// are forced to be strictly increasing.
    pub fn progress(&self, progress: f64, total: Option<f64>, message: Option<&str>) {
        let Some(token) = &self.meta.progress_token else { return };
        if self.is_cancelled() {
            return;
        }
        let mut last = self.last_progress.lock().unwrap_or_else(|e| e.into_inner());
        if progress <= *last {
            return;
        }
        *last = progress;
        let mut params = obj! { "progressToken" => token.clone(), "progress" => progress };
        if let Some(t) = total {
            params.set("total", t);
        }
        if let Some(m) = message {
            params.set("message", m);
        }
        self.sink.send(&notification("notifications/progress", params));
    }

    /// Emits `notifications/message` at or above the requested level, only
    /// when the request carried `io.modelcontextprotocol/logLevel`.
    pub fn log(&self, level: LogLevel, logger: &str, data: Value) {
        let Some(min) = self.meta.log_level else { return };
        if level < min || self.is_cancelled() {
            return;
        }
        self.sink.send(&notification("notifications/message", obj! { "level" => level.as_str(), "logger" => logger, "data" => data }));
    }

    fn caps(&self) -> &Value {
        &self.meta.client_capabilities
    }

    /// Form-mode elicitation (an empty `elicitation` object implies form).
    pub fn supports_elicitation_form(&self) -> bool {
        match self.caps().get("elicitation") {
            Some(Value::Object(m)) => m.contains_key("form") || !m.contains_key("url"),
            _ => false,
        }
    }

    pub fn supports_elicitation_url(&self) -> bool {
        self.caps().at(&["elicitation", "url"]).is_object()
    }

    pub fn supports_sampling(&self) -> bool {
        self.caps().get("sampling").is_some_and(|v| v.is_object())
    }

    pub fn supports_roots(&self) -> bool {
        self.caps().get("roots").is_some_and(|v| v.is_object())
    }

    pub fn supports_tasks(&self) -> bool {
        self.caps().at(&["extensions", EXT_TASKS]).is_object()
    }
}
