#![forbid(unsafe_code)]

//! Runtime/Kernel 与 Terminal Renderer 之间的 typed EventBus。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::time::Duration;

use harness_types::{
    AgentDefinitionId, ApprovalId, GoalRevisionId, MissionId, RunId, SessionId, TaskId,
    ToolInvocationId, TraceId,
};
use serde::{Deserialize, Serialize};

/// Event 的业务作用域。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventScope {
    pub session_id: Option<SessionId>,
    pub mission_id: Option<MissionId>,
    pub task_id: Option<TaskId>,
    pub run_id: Option<RunId>,
    pub trace_id: Option<TraceId>,
}

/// 背压时的处理等级。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventPriority {
    Critical,
    Normal,
    Delta,
}

/// Terminal/JSON 能观察的事件；不包含隐藏 Chain-of-Thought。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum HarnessEvent {
    SystemStarted {
        version: String,
        mode: String,
    },
    SystemReady {
        project_root: String,
    },
    SessionChanged {
        status: String,
    },
    GoalChanged {
        revision_id: Option<GoalRevisionId>,
        text: Option<String>,
        locked: bool,
    },
    ModelChanged {
        provider: String,
        model: String,
        reasoning_requested: String,
        reasoning_effective: Option<String>,
        reasoning_mapping: String,
    },
    ModelUsage {
        input_tokens: u64,
        cached_input_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: u64,
        total_tokens: u64,
    },
    PlanChanged {
        accepted: usize,
        running: usize,
        pending: usize,
        blocked: usize,
    },
    AgentStatus {
        agent_id: AgentDefinitionId,
        role: String,
        status: String,
        detail: String,
    },
    ReasoningSummary {
        agent_id: AgentDefinitionId,
        summary: String,
    },
    TextOutput {
        text: String,
    },
    ToolStatus {
        tool: String,
        status: String,
        summary: String,
    },
    BrowserStatus {
        session_id: String,
        status: String,
        detail: String,
    },
    McpStatus {
        server_id: String,
        status: String,
        detail: String,
    },
    PluginStatus {
        plugin_id: String,
        status: String,
        detail: String,
    },
    SkillStatus {
        skill_id: String,
        status: String,
        detail: String,
    },
    PermissionRequested {
        approval_id: ApprovalId,
        invocation_id: Option<ToolInvocationId>,
        action: String,
        risk: String,
        reason: String,
    },
    ContextChanged {
        used_tokens: u64,
        max_tokens: u64,
        cache_percent: Option<u8>,
    },
    Error {
        code: String,
        message: String,
        action: Option<String>,
    },
    SystemShutdown {
        reason: String,
    },
}

/// 每个 Subscriber 收到的统一 Envelope。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventEnvelope {
    pub schema_version: u32,
    pub sequence: u64,
    pub recorded_at_millis: i64,
    pub scope: EventScope,
    pub priority: EventPriority,
    pub event: HarnessEvent,
}

/// 一次 publish 的背压结果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublishReport {
    pub delivered: usize,
    pub dropped_delta_or_normal: usize,
    pub disconnected_slow_subscribers: usize,
}

/// EventBus 配置错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventBusError {
    pub code: &'static str,
    pub message: String,
}

impl Display for EventBusError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl Error for EventBusError {}

struct EventBusInner {
    subscribers: Mutex<BTreeMap<u64, SyncSender<EventEnvelope>>>,
    next_sequence: AtomicU64,
    next_subscriber_id: AtomicU64,
}

/// 多 Subscriber、非阻塞、有限缓冲的 EventBus。
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<EventBusInner>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(EventBusInner {
                subscribers: Mutex::new(BTreeMap::new()),
                next_sequence: AtomicU64::new(1),
                next_subscriber_id: AtomicU64::new(1),
            }),
        }
    }

    pub fn subscribe(&self, capacity: usize) -> Result<EventSubscription, EventBusError> {
        if capacity == 0 {
            return Err(EventBusError {
                code: "invalid-event-capacity",
                message: "Subscriber capacity 必须大于 0".to_owned(),
            });
        }
        let id = self.inner.next_subscriber_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::sync_channel(capacity);
        self.inner
            .subscribers
            .lock()
            .map_err(|_| EventBusError {
                code: "event-bus-poisoned",
                message: "Subscriber registry lock poisoned".to_owned(),
            })?
            .insert(id, sender);
        Ok(EventSubscription {
            id,
            receiver,
            bus: Arc::downgrade(&self.inner),
        })
    }

    pub fn publish(
        &self,
        event: HarnessEvent,
        scope: EventScope,
        priority: EventPriority,
        recorded_at_millis: i64,
    ) -> Result<PublishReport, EventBusError> {
        let envelope = EventEnvelope {
            schema_version: 1,
            sequence: self.inner.next_sequence.fetch_add(1, Ordering::SeqCst),
            recorded_at_millis,
            scope,
            priority,
            event,
        };
        let mut subscribers = self.inner.subscribers.lock().map_err(|_| EventBusError {
            code: "event-bus-poisoned",
            message: "Subscriber registry lock poisoned".to_owned(),
        })?;
        let mut report = PublishReport::default();
        let mut disconnect = Vec::new();
        for (id, sender) in subscribers.iter() {
            match sender.try_send(envelope.clone()) {
                Ok(()) => report.delivered += 1,
                Err(TrySendError::Full(_)) if priority == EventPriority::Critical => {
                    disconnect.push(*id);
                    report.disconnected_slow_subscribers += 1;
                }
                Err(TrySendError::Full(_)) => report.dropped_delta_or_normal += 1,
                Err(TrySendError::Disconnected(_)) => disconnect.push(*id),
            }
        }
        for id in disconnect {
            subscribers.remove(&id);
        }
        Ok(report)
    }

    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.inner
            .subscribers
            .lock()
            .map_or(0, |subscribers| subscribers.len())
    }
}

/// 一个 EventBus 订阅；Drop 自动注销。
pub struct EventSubscription {
    id: u64,
    receiver: Receiver<EventEnvelope>,
    bus: Weak<EventBusInner>,
}

impl EventSubscription {
    pub fn try_recv(&self) -> Result<EventEnvelope, TryRecvError> {
        self.receiver.try_recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<EventEnvelope, mpsc::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

impl Drop for EventSubscription {
    fn drop(&mut self) {
        if let Some(bus) = self.bus.upgrade()
            && let Ok(mut subscribers) = bus.subscribers.lock()
        {
            subscribers.remove(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> HarnessEvent {
        HarnessEvent::TextOutput {
            text: value.to_owned(),
        }
    }

    #[test]
    fn sequences_are_monotonic_for_every_subscriber() {
        let bus = EventBus::new();
        let first = bus.subscribe(4).expect("first subscription");
        let second = bus.subscribe(4).expect("second subscription");
        for value in ["a", "b"] {
            bus.publish(text(value), EventScope::default(), EventPriority::Normal, 1)
                .expect("publish");
        }
        assert_eq!(first.try_recv().expect("first event").sequence, 1);
        assert_eq!(first.try_recv().expect("second event").sequence, 2);
        assert_eq!(second.try_recv().expect("first event").sequence, 1);
        assert_eq!(second.try_recv().expect("second event").sequence, 2);
    }

    #[test]
    fn delta_can_drop_without_blocking_runtime() {
        let bus = EventBus::new();
        let subscription = bus.subscribe(1).expect("subscription");
        bus.publish(
            text("first"),
            EventScope::default(),
            EventPriority::Delta,
            1,
        )
        .expect("first publish");
        let report = bus
            .publish(
                text("second"),
                EventScope::default(),
                EventPriority::Delta,
                2,
            )
            .expect("second publish");
        assert_eq!(report.dropped_delta_or_normal, 1);
        assert_eq!(subscription.try_recv().expect("buffered event").sequence, 1);
    }

    #[test]
    fn critical_event_disconnects_slow_view_instead_of_blocking() {
        let bus = EventBus::new();
        let _slow = bus.subscribe(1).expect("slow subscription");
        bus.publish(
            text("fill"),
            EventScope::default(),
            EventPriority::Normal,
            1,
        )
        .expect("fill");
        let report = bus
            .publish(
                HarnessEvent::SystemShutdown {
                    reason: "test".to_owned(),
                },
                EventScope::default(),
                EventPriority::Critical,
                2,
            )
            .expect("critical publish");
        assert_eq!(report.disconnected_slow_subscribers, 1);
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn dropping_subscription_unregisters_it() {
        let bus = EventBus::new();
        let subscription = bus.subscribe(1).expect("subscription");
        assert_eq!(bus.subscriber_count(), 1);
        drop(subscription);
        assert_eq!(bus.subscriber_count(), 0);
    }
}
