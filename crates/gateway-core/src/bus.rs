//! In-process publish/subscribe for events.
//!
//! 进程内的事件发布 / 订阅。
//!
//! One [`tokio::sync::broadcast`] channel per session, plus one gateway-wide
//! channel for session lifecycle changes. Slow subscribers are *lagged*, never
//! blocking: a phone on a bad network must not be able to stall an agent.
//! Subscribers that lag recover by replaying from the event store, which is
//! the same code path used after a reconnect.
//!
//! 每个 Session 一个 [`tokio::sync::broadcast`] channel，另外有一个全局 channel 用于
//! Session 生命周期变化。慢订阅者只会被*落后*（lagged），绝不会阻塞：网络差的手机
//! 不能拖死 Agent。落后的订阅者通过从事件存储回放来重新同步——与断线重连走的是同一条代码路径。

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::event::AgentEvent;
use crate::ids::SessionId;
use crate::session::SessionStatus;

/// Default per-session ring buffer size.
///
/// Chunked agent output is bursty; 1024 events covers several seconds of
/// streaming on a stalled subscriber before it has to fall back to replay.
///
/// 每个 Session 默认的环形缓冲区大小。
///
/// Agent 的分片输出是突发式的；1024 个事件足以让一个卡住的订阅者撑几秒钟，
/// 之后才需要回退到回放。
pub const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

/// A gateway-wide notification that a session's identity or state changed.
///
/// Session *content* stays on the per-session channel; this channel only
/// carries what a session-list view needs, so a client watching the list does
/// not receive every token of every agent.
///
/// 全局通知：某个 Session 的身份或状态发生了变化。
///
/// Session 的*内容*仍然走各自的 channel；本 channel 只携带“Session 列表视图”所需的信息，
/// 因此监听列表的客户端不会收到每个 Agent 的每个 token。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionLifecycle {
    /// Session affected. / 受影响的 Session。
    pub session_id: SessionId,
    /// New status. / 新状态。
    pub status: SessionStatus,
    /// Whether the session was just created. / 是否刚刚创建。
    pub created: bool,
}

/// Handle to one session's event stream.
///
/// 单个 Session 事件流的句柄。
#[derive(Debug)]
pub struct EventSubscription {
    receiver: broadcast::Receiver<Arc<AgentEvent>>,
}

impl EventSubscription {
    pub(crate) fn new(receiver: broadcast::Receiver<Arc<AgentEvent>>) -> Self {
        Self { receiver }
    }

    /// Await the next event.
    ///
    /// 等待下一个事件。
    ///
    /// # Errors
    /// [`broadcast::error::RecvError::Lagged`] means the subscriber fell
    /// behind and must resynchronise by replaying from its last seen `seq`.
    ///
    /// [`broadcast::error::RecvError::Lagged`] 表示订阅者已经落后，必须从它最后看到的
    /// `seq` 开始回放以重新同步。
    pub async fn recv(&mut self) -> Result<Arc<AgentEvent>, broadcast::error::RecvError> {
        self.receiver.recv().await
    }
}

/// Fan-out for one session.
///
/// 单个 Session 的事件分发器。
#[derive(Debug)]
pub struct EventBus {
    sender: broadcast::Sender<Arc<AgentEvent>>,
}

impl EventBus {
    /// Create a bus with the given ring buffer capacity.
    ///
    /// 按指定的环形缓冲区容量创建一个 bus。
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Publish. Returns the number of live subscribers.
    ///
    /// 发布事件，返回当前存活的订阅者数量。
    pub fn publish(&self, event: Arc<AgentEvent>) -> usize {
        self.sender.send(event).unwrap_or(0)
    }

    /// Subscribe to everything published from now on.
    ///
    /// 订阅从现在开始发布的所有事件。
    #[must_use]
    pub fn subscribe(&self) -> EventSubscription {
        EventSubscription::new(self.sender.subscribe())
    }

    /// Number of live subscribers.
    ///
    /// 存活订阅者数量。
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CHANNEL_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventType;
    use crate::ids::EventId;
    use chrono::Utc;

    fn event(seq: u64) -> Arc<AgentEvent> {
        Arc::new(AgentEvent {
            id: EventId::generate(),
            session_id: SessionId::new("sess_1"),
            seq,
            timestamp: Utc::now(),
            event_type: EventType::AgentMessageChunk.as_str().to_owned(),
            payload: serde_json::json!({}),
        })
    }

    #[tokio::test]
    async fn every_subscriber_sees_every_event() {
        let bus = EventBus::with_capacity(8);
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        assert_eq!(bus.publish(event(1)), 2);
        assert_eq!(a.recv().await.unwrap().seq, 1);
        assert_eq!(b.recv().await.unwrap().seq, 1);
    }

    #[tokio::test]
    async fn a_stalled_subscriber_is_lagged_not_blocking() {
        let bus = EventBus::with_capacity(2);
        let mut slow = bus.subscribe();
        for seq in 1..=5 {
            bus.publish(event(seq));
        }
        assert!(matches!(
            slow.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
        // …and the subscriber can keep going from whatever is still buffered.
        // …而且订阅者可以从缓冲区里剩下的内容继续读。
        assert!(slow.recv().await.is_ok());
    }

    #[test]
    fn publishing_without_subscribers_is_not_an_error() {
        let bus = EventBus::default();
        assert_eq!(bus.publish(event(1)), 0);
    }
}
