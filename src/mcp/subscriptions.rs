//! `subscriptions/listen`: long-lived notification streams for list changes,
//! resource updates and (with the tasks extension) task status.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::context::Sink;
use super::protocol::{META_SUBSCRIPTION_ID, notification, result_response};
use crate::json::Value;
use crate::obj;

/// The notification types a subscription agreed to deliver.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filter {
    pub tools_list_changed: bool,
    pub prompts_list_changed: bool,
    pub resources_list_changed: bool,
    pub resource_uris: Vec<String>,
    pub task_ids: Vec<String>,
}

impl Filter {
    pub fn to_json(&self, include_tasks: bool) -> Value {
        let mut v = Value::object();
        if self.tools_list_changed {
            v.set("toolsListChanged", true);
        }
        if self.prompts_list_changed {
            v.set("promptsListChanged", true);
        }
        if self.resources_list_changed {
            v.set("resourcesListChanged", true);
        }
        if !self.resource_uris.is_empty() {
            v.set("resourceSubscriptions", self.resource_uris.clone());
        }
        if include_tasks && !self.task_ids.is_empty() {
            v.set("taskIds", self.task_ids.clone());
        }
        v
    }
}

pub struct Subscription {
    /// JSON-RPC id of the `subscriptions/listen` request.
    pub id: Value,
    pub filter: Filter,
    pub principal: String,
    sink: Arc<dyn Sink>,
    closed: AtomicBool,
    signal: (Mutex<bool>, Condvar),
}

impl Subscription {
    pub fn new(
        id: Value,
        filter: Filter,
        principal: &str,
        sink: Arc<dyn Sink>,
    ) -> Arc<Subscription> {
        Arc::new(Subscription {
            id,
            filter,
            principal: principal.to_string(),
            sink,
            closed: AtomicBool::new(false),
            signal: (Mutex::new(false), Condvar::new()),
        })
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Sends a notification tagged with this subscription's id.
    pub fn notify(&self, method: &str, mut params: Value) -> bool {
        if self.is_closed() {
            return false;
        }
        let meta = obj! { META_SUBSCRIPTION_ID => self.id.clone() };
        match params.get_mut("_meta").and_then(|m| m.as_object_mut()) {
            Some(m) => {
                m.insert(META_SUBSCRIPTION_ID, self.id.clone());
            }
            None => params.set("_meta", meta),
        }
        let ok = self.sink.send(&notification(method, params));
        if !ok {
            self.close();
        }
        ok
    }

    /// Sends the acknowledgment (must be the first message on the stream).
    pub fn acknowledge(&self, include_tasks: bool) -> bool {
        self.notify(
            "notifications/subscriptions/acknowledged",
            obj! { "notifications" => self.filter.to_json(include_tasks) },
        )
    }

    /// Ends the subscription gracefully with a completion response.
    pub fn complete(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let resp = result_response(
            &self.id,
            obj! { "_meta" => obj! { META_SUBSCRIPTION_ID => self.id.clone() } },
        );
        self.sink.send(&resp);
        self.wake();
    }

    /// Tears the subscription down without a response (client cancelled or
    /// the transport dropped).
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.wake();
    }

    /// Server-initiated abnormal teardown: `notifications/cancelled`
    /// referencing the listen request id.
    pub fn abort(&self, reason: &str) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.sink.send(&notification(
            "notifications/cancelled",
            obj! { "requestId" => self.id.clone(), "reason" => reason },
        ));
        self.wake();
    }

    fn wake(&self) {
        let (m, cv) = &self.signal;
        *m.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }

    /// Waits up to `timeout`; returns true once closed.
    pub fn wait(&self, timeout: Duration) -> bool {
        let (m, cv) = &self.signal;
        let g = m.lock().unwrap_or_else(|e| e.into_inner());
        if *g || self.is_closed() {
            return true;
        }
        let (g, _) = cv
            .wait_timeout(g, timeout)
            .unwrap_or_else(|e| e.into_inner());
        *g || self.is_closed()
    }

    pub fn keepalive(&self) -> bool {
        let ok = self.sink.keepalive();
        if !ok {
            self.close();
        }
        ok
    }
}

/// Registry of active subscriptions.
#[derive(Default)]
pub struct Hub {
    subs: Mutex<Vec<Arc<Subscription>>>,
}

impl Hub {
    pub fn new() -> Hub {
        Hub::default()
    }

    fn list(&self) -> Vec<Arc<Subscription>> {
        let mut subs = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        subs.retain(|s| !s.is_closed());
        subs.clone()
    }

    pub fn add(&self, sub: Arc<Subscription>) {
        self.subs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(sub);
    }

    /// Removes (closes) the subscription opened by request `id`.
    pub fn cancel(&self, id: &Value) -> bool {
        let mut found = false;
        let mut subs = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        subs.retain(|s| {
            if &s.id == id {
                s.close();
                found = true;
                false
            } else {
                true
            }
        });
        found
    }

    pub fn len(&self) -> usize {
        self.list().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn notify_tools_changed(&self) -> usize {
        self.list()
            .iter()
            .filter(|s| s.filter.tools_list_changed)
            .filter(|s| s.notify("notifications/tools/list_changed", obj! {}))
            .count()
    }

    pub fn notify_prompts_changed(&self) -> usize {
        self.list()
            .iter()
            .filter(|s| s.filter.prompts_list_changed)
            .filter(|s| s.notify("notifications/prompts/list_changed", obj! {}))
            .count()
    }

    pub fn notify_resources_changed(&self) -> usize {
        self.list()
            .iter()
            .filter(|s| s.filter.resources_list_changed)
            .filter(|s| s.notify("notifications/resources/list_changed", obj! {}))
            .count()
    }

    pub fn notify_resource_updated(&self, uri: &str) -> usize {
        self.list()
            .iter()
            .filter(|s| s.filter.resource_uris.iter().any(|u| u == uri))
            .filter(|s| s.notify("notifications/resources/updated", obj! { "uri" => uri }))
            .count()
    }

    /// Delivers a task status snapshot to the owner's subscriptions on it.
    pub fn notify_task(&self, task_id: &str, owner: &str, task: Value) -> usize {
        self.list()
            .iter()
            .filter(|s| s.principal == owner && s.filter.task_ids.iter().any(|t| t == task_id))
            .filter(|s| s.notify("notifications/tasks", task.clone()))
            .count()
    }

    /// Distinct resource URIs currently watched.
    pub fn watched_uris(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .list()
            .iter()
            .flat_map(|s| s.filter.resource_uris.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    pub fn wants(&self, f: impl Fn(&Filter) -> bool) -> bool {
        self.list().iter().any(|s| f(&s.filter))
    }

    /// Graceful shutdown: completes every subscription.
    pub fn shutdown(&self) {
        let subs = std::mem::take(&mut *self.subs.lock().unwrap_or_else(|e| e.into_inner()));
        for s in subs {
            s.complete();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Capture(Mutex<Vec<Value>>);
    impl Sink for Capture {
        fn send(&self, msg: &Value) -> bool {
            self.0.lock().unwrap().push(msg.clone());
            true
        }
    }

    #[test]
    fn routing_and_shutdown() {
        let hub = Hub::new();
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let filter = Filter {
            tools_list_changed: true,
            resource_uris: vec!["steam://cache/stats".into()],
            ..Default::default()
        };
        let sub = Subscription::new(Value::from(7), filter, "p", cap.clone());
        assert!(sub.acknowledge(false));
        hub.add(sub.clone());
        assert_eq!(hub.notify_tools_changed(), 1);
        assert_eq!(hub.notify_prompts_changed(), 0);
        assert_eq!(hub.notify_resource_updated("steam://cache/stats"), 1);
        assert_eq!(hub.notify_resource_updated("steam://other"), 0);
        assert_eq!(hub.watched_uris(), vec!["steam://cache/stats".to_string()]);
        hub.shutdown();
        assert!(sub.is_closed());
        let msgs = cap.0.lock().unwrap();
        assert_eq!(
            msgs[0]["method"].as_str(),
            Some("notifications/subscriptions/acknowledged")
        );
        assert_eq!(
            msgs[0]["params"]["_meta"][META_SUBSCRIPTION_ID].as_i64(),
            Some(7)
        );
        assert_eq!(
            msgs[1]["method"].as_str(),
            Some("notifications/tools/list_changed")
        );
        assert_eq!(
            msgs[2]["params"]["uri"].as_str(),
            Some("steam://cache/stats")
        );
        assert_eq!(msgs[3]["id"].as_i64(), Some(7));
        assert_eq!(
            msgs[3]["result"]["_meta"][META_SUBSCRIPTION_ID].as_i64(),
            Some(7)
        );
        assert!(hub.is_empty());
    }
}
