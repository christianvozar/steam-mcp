//! The `io.modelcontextprotocol/tasks` extension: durable task handles for
//! long-running tool calls, with polling (`tasks/get`), mid-flight input
//! (`tasks/update`), cooperative cancellation (`tasks/cancel`) and status
//! notifications (`notifications/tasks`) via `subscriptions/listen`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::json::{Map, Value};
use crate::obj;
use crate::util::CancelToken;
use crate::util::crypto::random_id;
use crate::util::time::{iso8601_ms, now_ms};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Working,
    InputRequired,
    Completed,
    Failed,
    Cancelled,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Working => "working",
            Status::InputRequired => "input_required",
            Status::Completed => "completed",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
        }
    }
    pub fn is_terminal(&self) -> bool {
        matches!(self, Status::Completed | Status::Failed | Status::Cancelled)
    }
}

struct State {
    status: Status,
    status_message: Option<String>,
    updated_ms: u64,
    result: Option<Value>,
    error: Option<Value>,
    /// Outstanding server-to-client requests keyed by unique id.
    input_requests: Map,
    /// Responses received but not yet consumed by the worker.
    input_responses: Map,
    /// Every key ever issued (keys are never reused within a task).
    issued: HashSet<String>,
}

/// Callback used to publish status changes (wired to the subscription hub).
pub type Notifier = Arc<dyn Fn(&str, &str, Value) + Send + Sync>;

pub struct TaskHandle {
    pub id: String,
    pub owner: String,
    pub created_ms: u64,
    pub ttl_ms: u64,
    pub poll_interval_ms: u64,
    state: Mutex<State>,
    cv: Condvar,
    cancel: CancelToken,
    notifier: Notifier,
}

impl TaskHandle {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn status(&self) -> Status {
        self.lock().status
    }

    /// The `Task` JSON (detailed variant when `detailed`).
    pub fn to_json(&self, detailed: bool) -> Value {
        let s = self.lock();
        self.render(&s, detailed)
    }

    fn render(&self, s: &State, detailed: bool) -> Value {
        let mut t = obj! {
            "taskId" => self.id.as_str(),
            "status" => s.status.as_str(),
            "createdAt" => iso8601_ms(self.created_ms),
            "lastUpdatedAt" => iso8601_ms(s.updated_ms),
            "ttlMs" => self.ttl_ms,
            "pollIntervalMs" => self.poll_interval_ms,
        };
        if let Some(m) = &s.status_message {
            t.set("statusMessage", m.as_str());
        }
        if detailed {
            match s.status {
                Status::InputRequired => t.set("inputRequests", Value::Object(s.input_requests.clone())),
                Status::Completed => t.set("result", s.result.clone().unwrap_or_else(Value::object)),
                Status::Failed => t.set("error", s.error.clone().unwrap_or_else(|| obj! { "code" => -32603, "message" => "Task failed" })),
                _ => {}
            }
        }
        t
    }

    fn changed(&self, s: &State) {
        let snapshot = self.render(s, true);
        (self.notifier)(&self.id, &self.owner, snapshot);
    }

    pub fn set_status_message(&self, msg: &str) {
        let mut s = self.lock();
        if s.status.is_terminal() || s.status_message.as_deref() == Some(msg) {
            return;
        }
        s.status_message = Some(msg.to_string());
        s.updated_ms = now_ms();
        // Status-message-only updates are not pushed to avoid flooding; clients poll.
    }

    /// Requests client input mid-task. Blocks until a matching
    /// `tasks/update` arrives, the task is cancelled, or `timeout` elapses.
    pub fn request_input(&self, key: &str, request: Value, timeout: Duration) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        let mut s = self.lock();
        if s.status.is_terminal() || !s.issued.insert(key.to_string()) {
            return None;
        }
        s.input_requests.insert(key, request);
        s.status = Status::InputRequired;
        s.updated_ms = now_ms();
        self.changed(&s);
        loop {
            if let Some(resp) = s.input_responses.remove(key) {
                s.input_requests.remove(key);
                if s.input_requests.is_empty() && !s.status.is_terminal() {
                    s.status = Status::Working;
                    s.updated_ms = now_ms();
                    self.changed(&s);
                }
                return Some(resp);
            }
            if self.cancel.is_cancelled() || s.status.is_terminal() {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                s.input_requests.remove(key);
                if s.input_requests.is_empty() && !s.status.is_terminal() {
                    s.status = Status::Working;
                    s.updated_ms = now_ms();
                    self.changed(&s);
                }
                return None;
            }
            let (g, _) = self.cv.wait_timeout(s, (deadline - now).min(Duration::from_millis(500))).unwrap_or_else(|e| e.into_inner());
            s = g;
        }
    }

    /// Accepts `inputResponses`; unknown or already-satisfied keys are ignored.
    pub fn update_inputs(&self, responses: &Map) -> usize {
        let mut s = self.lock();
        let mut accepted = 0;
        for (k, v) in responses.iter() {
            if s.input_requests.contains_key(k) && !s.input_responses.contains_key(k) {
                s.input_responses.insert(k.as_str(), v.clone());
                accepted += 1;
            }
        }
        drop(s);
        self.cv.notify_all();
        accepted
    }

    pub fn complete(&self, result: Value) {
        self.finish(Status::Completed, Some(result), None);
    }

    pub fn fail(&self, error: Value) {
        self.finish(Status::Failed, None, Some(error));
    }

    fn finish(&self, status: Status, result: Option<Value>, error: Option<Value>) {
        let mut s = self.lock();
        if s.status.is_terminal() {
            return; // terminal states are immutable
        }
        s.status = status;
        s.result = result;
        s.error = error;
        s.input_requests = Map::new();
        s.updated_ms = now_ms();
        if status == Status::Failed && s.status_message.is_none() {
            s.status_message = Some("The underlying request failed with a JSON-RPC error".into());
        }
        self.changed(&s);
        drop(s);
        self.cv.notify_all();
    }

    /// Cooperative cancellation: signals the worker and marks the task
    /// cancelled unless it already reached a terminal state.
    pub fn request_cancel(&self) {
        self.cancel.cancel();
        let mut s = self.lock();
        if !s.status.is_terminal() {
            s.status = Status::Cancelled;
            s.status_message = Some("Cancelled at the client's request".into());
            s.input_requests = Map::new();
            s.updated_ms = now_ms();
            self.changed(&s);
        }
        drop(s);
        self.cv.notify_all();
    }

    /// Blocks until terminal or `timeout`; returns true if terminal.
    pub fn wait_terminal(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut s = self.lock();
        while !s.status.is_terminal() {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            // Input-required tasks cannot finish synchronously.
            if s.status == Status::InputRequired {
                return false;
            }
            let (g, _) = self.cv.wait_timeout(s, deadline - now).unwrap_or_else(|e| e.into_inner());
            s = g;
        }
        true
    }

    pub fn result(&self) -> Option<Value> {
        self.lock().result.clone()
    }

    fn expired(&self, now: u64) -> bool {
        now > self.created_ms + self.ttl_ms
    }
}

/// In-memory task registry.
pub struct TaskStore {
    tasks: Mutex<HashMap<String, Arc<TaskHandle>>>,
    notifier: Mutex<Option<Notifier>>,
}

impl Default for TaskStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskStore {
    pub fn new() -> TaskStore {
        TaskStore { tasks: Mutex::new(HashMap::new()), notifier: Mutex::new(None) }
    }

    pub fn set_notifier(&self, n: Notifier) {
        *self.notifier.lock().unwrap_or_else(|e| e.into_inner()) = Some(n);
    }

    /// Creates (and registers) a task before it is returned to the client.
    pub fn create(&self, owner: &str, ttl: Duration, poll: Duration) -> Arc<TaskHandle> {
        let notifier = self.notifier.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| Arc::new(|_: &str, _: &str, _: Value| {}));
        let now = now_ms();
        let t = Arc::new(TaskHandle {
            id: format!("task_{}", random_id(16)),
            owner: owner.to_string(),
            created_ms: now,
            ttl_ms: ttl.as_millis() as u64,
            poll_interval_ms: poll.as_millis() as u64,
            state: Mutex::new(State { status: Status::Working, status_message: Some("Started".into()), updated_ms: now, result: None, error: None, input_requests: Map::new(), input_responses: Map::new(), issued: HashSet::new() }),
            cv: Condvar::new(),
            cancel: CancelToken::new(),
            notifier,
        });
        let mut map = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, t| !t.expired(now));
        map.insert(t.id.clone(), t.clone());
        t
    }

    /// Looks up a task owned by `owner` (other principals see "not found").
    pub fn get(&self, id: &str, owner: &str) -> Option<Arc<TaskHandle>> {
        let now = now_ms();
        let map = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        map.get(id).filter(|t| t.owner == owner && !t.expired(now)).cloned()
    }

    pub fn purge(&self) -> usize {
        let now = now_ms();
        let mut map = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        let before = map.len();
        map.retain(|_, t| !t.expired(now));
        before - map.len()
    }

    pub fn len(&self) -> usize {
        self.tasks.lock().map(|m| m.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn cancel_all(&self) {
        let map = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        for t in map.values() {
            t.request_cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_and_input() {
        let store = TaskStore::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let ev = events.clone();
        store.set_notifier(Arc::new(move |_id: &str, _o: &str, v: Value| ev.lock().unwrap().push(v["status"].as_str().unwrap().to_string())));
        let t = store.create("alice", Duration::from_secs(60), Duration::from_millis(500));
        assert!(store.get(&t.id, "bob").is_none());
        let t2 = store.get(&t.id, "alice").unwrap();
        let worker = std::thread::spawn(move || {
            let answer = t2.request_input("confirm_1", obj! { "method" => "elicitation/create" }, Duration::from_secs(5));
            t2.complete(obj! { "answer" => answer.unwrap_or_default() });
        });
        while t.status() != Status::InputRequired {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(t.to_json(true)["inputRequests"]["confirm_1"].is_object());
        let mut resp = Map::new();
        resp.insert("unknown", obj! {});
        resp.insert("confirm_1", obj! { "action" => "accept" });
        assert_eq!(t.update_inputs(&resp), 1);
        worker.join().unwrap();
        assert!(t.wait_terminal(Duration::from_secs(1)));
        let j = t.to_json(true);
        assert_eq!(j["status"].as_str(), Some("completed"));
        assert_eq!(j["result"]["answer"]["action"].as_str(), Some("accept"));
        t.request_cancel(); // terminal: no change
        assert_eq!(t.status(), Status::Completed);
        assert_eq!(*events.lock().unwrap(), vec!["input_required", "working", "completed"]);
    }

    #[test]
    fn cancellation() {
        let store = TaskStore::new();
        let t = store.create("a", Duration::from_secs(60), Duration::from_millis(100));
        t.request_cancel();
        assert!(t.is_cancelled());
        assert_eq!(t.status(), Status::Cancelled);
        t.complete(obj! {});
        assert_eq!(t.status(), Status::Cancelled);
        let expired = store.create("a", Duration::ZERO, Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(2));
        assert!(store.get(&expired.id, "a").is_none());
    }
}
