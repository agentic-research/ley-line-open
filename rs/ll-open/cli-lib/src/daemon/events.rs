//! Pub/sub event router for the UDS control socket (ADR-010).
//!
//! Extends the existing `{control}.sock` with `subscribe`, `unsubscribe`,
//! and `emit` operations. The ley-line daemon becomes a lightweight event bus
//! that routes structured events to subscribers by topic pattern.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{RwLock, mpsc};

/// Soft cap on the EventLog's initial backing allocation. The deque
/// grows on demand up to its configured `capacity`, but we don't
/// pre-allocate a multi-MB slab just because someone passed a big
/// log capacity. 1024 is enough headroom for a normal session burst.
const EVENT_LOG_INITIAL_ALLOC: usize = 1024;

// -- Event types --------------------------------------------------------------

/// A fully sequenced event ready for dispatch.
#[derive(Clone, Debug, Serialize)]
pub struct Event {
    /// Always true -- distinguishes pushed events from request/response.
    pub event: bool,
    /// Monotonically increasing sequence number (Lamport timestamp).
    pub seq: u64,
    /// Hierarchical dot-separated topic (e.g., `node.spliced`).
    pub topic: String,
    /// Identity of the emitter (`leyline`, `mache`, `agent:<id>`).
    pub source: String,
    /// Topic-specific payload.
    pub data: serde_json::Value,
}

/// An inbound event before sequence assignment.
struct RawEvent {
    topic: String,
    source: String,
    data: serde_json::Value,
}

// -- Overflow policy ----------------------------------------------------------

/// How to handle a slow subscriber whose channel is full.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverflowPolicy {
    #[default]
    DropOldest,
    Disconnect,
}

// -- Subscriber ---------------------------------------------------------------

struct Subscriber {
    id: u64,
    identity: Option<String>,
    patterns: Vec<TopicPattern>,
    tx: mpsc::Sender<Event>,
    overflow: OverflowPolicy,
}

impl Subscriber {
    fn matches(&self, topic: &str, source: &str) -> bool {
        // Echo suppression: don't send events back to their source
        if let Some(ref id) = self.identity
            && id == source
        {
            return false;
        }
        self.patterns.iter().any(|p| p.matches(topic))
    }
}

// -- Topic pattern matching ---------------------------------------------------

/// Compiled topic pattern supporting `*` (one segment) and `**` (any segments).
#[derive(Clone, Debug)]
struct TopicPattern {
    segments: Vec<PatternSegment>,
    raw: String,
}

#[derive(Clone, Debug)]
enum PatternSegment {
    Literal(String),
    /// Matches exactly one segment.
    Star,
    /// Matches zero or more segments.
    DoubleStar,
}

impl TopicPattern {
    fn parse(pattern: &str) -> Self {
        let segments = pattern
            .split('.')
            .map(|s| match s {
                "**" => PatternSegment::DoubleStar,
                "*" => PatternSegment::Star,
                _ => PatternSegment::Literal(s.to_string()),
            })
            .collect();
        TopicPattern {
            segments,
            raw: pattern.to_string(),
        }
    }

    fn matches(&self, topic: &str) -> bool {
        let topic_parts: Vec<&str> = topic.split('.').collect();
        match_segments(&self.segments, &topic_parts)
    }
}

fn match_segments(pattern: &[PatternSegment], topic: &[&str]) -> bool {
    match (pattern.first(), topic.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(PatternSegment::DoubleStar), _) => {
            // ** matches zero or more segments
            match_segments(&pattern[1..], topic)
                || (!topic.is_empty() && match_segments(pattern, &topic[1..]))
        }
        (Some(PatternSegment::Star), Some(_)) => match_segments(&pattern[1..], &topic[1..]),
        (Some(PatternSegment::Literal(p)), Some(t)) if p == t => {
            match_segments(&pattern[1..], &topic[1..])
        }
        _ => false,
    }
}

// -- Event log (bounded ring buffer) ------------------------------------------

struct EventLog {
    events: VecDeque<Event>,
    capacity: usize,
}

impl EventLog {
    fn new(capacity: usize) -> Self {
        EventLog {
            events: VecDeque::with_capacity(capacity.min(EVENT_LOG_INITIAL_ALLOC)),
            capacity,
        }
    }

    fn push(&mut self, event: Event) {
        if self.events.len() >= self.capacity {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    /// Return events with seq > since.
    fn since(&self, since: u64) -> (Vec<Event>, bool) {
        let first_seq = self.events.front().map(|e| e.seq).unwrap_or(0);
        let gap = since > 0 && since < first_seq;
        let events: Vec<Event> = self
            .events
            .iter()
            .filter(|e| e.seq > since)
            .cloned()
            .collect();
        (events, gap)
    }

    fn head_seq(&self) -> u64 {
        self.events.back().map(|e| e.seq).unwrap_or(0)
    }
}

// -- EventRouter --------------------------------------------------------------

/// The central event bus. Created once by the daemon, shared across connections.
pub struct EventRouter {
    seq: AtomicU64,
    next_sub_id: AtomicU64,
    subscribers: RwLock<Vec<Subscriber>>,
    log: RwLock<EventLog>,
    emit_tx: mpsc::UnboundedSender<RawEvent>,
}

impl EventRouter {
    /// Create a new event router and spawn its dispatch loop.
    pub fn new(log_capacity: usize) -> Arc<Self> {
        let (emit_tx, emit_rx) = mpsc::unbounded_channel();
        let router = Arc::new(EventRouter {
            seq: AtomicU64::new(0),
            next_sub_id: AtomicU64::new(1),
            subscribers: RwLock::new(Vec::new()),
            log: RwLock::new(EventLog::new(log_capacity)),
            emit_tx,
        });

        // Spawn the dispatch loop
        let r = router.clone();
        tokio::spawn(async move {
            r.dispatch_loop(emit_rx).await;
        });

        router
    }

    /// Create an in-process emitter handle (cheap to clone).
    pub fn emitter(&self) -> EventEmitter {
        EventEmitter {
            tx: self.emit_tx.clone(),
        }
    }

    /// Register a subscriber. Returns (subscriber_id, event_receiver, replay_events, replay_gap).
    pub async fn subscribe(
        &self,
        topics: &[String],
        identity: Option<String>,
        since: u64,
        overflow: OverflowPolicy,
        buffer_size: usize,
    ) -> (u64, mpsc::Receiver<Event>, Vec<Event>, bool) {
        let id = self.next_sub_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(buffer_size);
        let patterns: Vec<TopicPattern> = topics.iter().map(|t| TopicPattern::parse(t)).collect();

        let sub = Subscriber {
            id,
            identity,
            patterns,
            tx,
            overflow,
        };

        // Replay from log before registering (so we don't double-deliver)
        let (replay, gap) = {
            let log = self.log.read().await;
            log.since(since)
        };

        self.subscribers.write().await.push(sub);

        (id, rx, replay, gap)
    }

    /// Unsubscribe specific topic patterns for a subscriber.
    pub async fn unsubscribe_topics(&self, sub_id: u64, topics: &[String]) {
        let patterns: Vec<TopicPattern> = topics.iter().map(|t| TopicPattern::parse(t)).collect();
        let mut subs = self.subscribers.write().await;
        if let Some(sub) = subs.iter_mut().find(|s| s.id == sub_id) {
            sub.patterns
                .retain(|p| !patterns.iter().any(|rp| rp.raw == p.raw));
            // Remove subscriber entirely if no patterns remain
            if sub.patterns.is_empty() {
                subs.retain(|s| s.id != sub_id);
            }
        }
    }

    /// Remove a subscriber entirely (called on disconnect).
    pub async fn remove_subscriber(&self, sub_id: u64) {
        self.subscribers.write().await.retain(|s| s.id != sub_id);
    }

    /// Current head sequence number.
    pub async fn head_seq(&self) -> u64 {
        self.log.read().await.head_seq()
    }

    /// Emit an event from an external client (via UDS `emit` op).
    pub async fn emit_external(
        &self,
        topic: String,
        source: String,
        data: serde_json::Value,
    ) -> u64 {
        self.assign_and_dispatch(topic, source, data).await
    }

    /// Internal dispatch loop: processes events from the in-process channel.
    async fn dispatch_loop(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<RawEvent>) {
        while let Some(raw) = rx.recv().await {
            self.assign_and_dispatch(raw.topic, raw.source, raw.data)
                .await;
        }
    }

    /// Assign sequence number, log, and dispatch to matching subscribers.
    async fn assign_and_dispatch(
        &self,
        topic: String,
        source: String,
        data: serde_json::Value,
    ) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;

        let event = Event {
            event: true,
            seq,
            topic,
            source,
            data,
        };

        // Append to log
        self.log.write().await.push(event.clone());

        // Dispatch to matching subscribers
        let mut to_remove = Vec::new();
        {
            let subs = self.subscribers.read().await;
            for sub in subs.iter() {
                if !sub.matches(&event.topic, &event.source) {
                    continue;
                }
                match sub.tx.try_send(event.clone()) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        match sub.overflow {
                            OverflowPolicy::Disconnect => {
                                log::warn!("subscriber {} overflow (disconnect policy)", sub.id);
                                to_remove.push(sub.id);
                            }
                            OverflowPolicy::DropOldest => {
                                // Bounded channel is full -- we can't pop from it here,
                                // but the receiver will see the gap in seq numbers.
                                log::debug!("subscriber {} channel full, event dropped", sub.id);
                            }
                        }
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        to_remove.push(sub.id);
                    }
                }
            }
        }

        // Clean up dead/overflowed subscribers
        if !to_remove.is_empty() {
            let mut subs = self.subscribers.write().await;
            subs.retain(|s| !to_remove.contains(&s.id));
        }

        seq
    }
}

// -- EventEmitter (in-process handle) -----------------------------------------

/// Lightweight handle for in-process event emission. Cheap to clone.
#[derive(Clone)]
pub struct EventEmitter {
    tx: mpsc::UnboundedSender<RawEvent>,
}

impl EventEmitter {
    /// Emit an event into the bus. Non-blocking, fire-and-forget.
    pub fn emit(&self, topic: &str, source: &str, data: serde_json::Value) {
        let _ = self.tx.send(RawEvent {
            topic: topic.to_string(),
            source: source.to_string(),
            data,
        });
    }

    /// Create a no-op emitter that silently drops all events.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn noop() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        EventEmitter { tx }
    }
}

// -- ConnectionState (per-connection UDS state) -------------------------------

/// Per-connection state for the UDS socket.
pub struct ConnectionState {
    router: Arc<EventRouter>,
    sub_id: Option<u64>,
    event_rx: Option<mpsc::Receiver<Event>>,
}

impl ConnectionState {
    pub fn new(router: Arc<EventRouter>) -> Self {
        ConnectionState {
            router,
            sub_id: None,
            event_rx: None,
        }
    }

    /// Create an in-process event emitter from this connection's router.
    pub fn emitter(&self) -> EventEmitter {
        self.router.emitter()
    }

    /// Handle a subscribe command. Returns the JSON response.
    pub async fn handle_subscribe(&mut self, req: &serde_json::Value) -> String {
        let topics: Vec<String> = req
            .get("topics")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        if topics.is_empty() {
            return r#"{"error":"subscribe requires non-empty 'topics' array"}"#.to_string();
        }

        let identity = req
            .get("identity")
            .and_then(|v| v.as_str())
            .map(String::from);

        let since = req.get("since").and_then(|v| v.as_u64()).unwrap_or(0);

        let overflow: OverflowPolicy = req
            .get("overflow")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        // If already subscribed, remove old subscription
        if let Some(old_id) = self.sub_id.take() {
            self.router.remove_subscriber(old_id).await;
        }

        let (sub_id, event_rx, replay, gap) = self
            .router
            .subscribe(&topics, identity, since, overflow, 1024)
            .await;

        self.sub_id = Some(sub_id);
        self.event_rx = Some(event_rx);

        let head_seq = self.router.head_seq().await;
        let topic_strs: Vec<&str> = topics.iter().map(|s| s.as_str()).collect();

        let mut resp = serde_json::json!({
            "ok": true,
            "subscribed": topic_strs,
            "head_seq": head_seq,
        });
        if gap {
            resp["replay_gap"] = serde_json::json!(true);
        }

        // Return replay events as a separate field so the connection handler
        // can push them before entering the event loop.
        if !replay.is_empty() {
            resp["replay_count"] = serde_json::json!(replay.len());
        }

        // Store replay events for the connection handler to drain
        // (they'll be sent as pushed events after the response)
        // We send them through the event_rx channel by pushing them
        // back -- but that's complex. Instead, just return them.
        let mut result = serde_json::to_string(&resp).unwrap();

        // Append replay events as separate lines
        for event in &replay {
            result.push('\n');
            result.push_str(&serde_json::to_string(event).unwrap());
        }

        result
    }

    /// Handle an unsubscribe command.
    pub async fn handle_unsubscribe(&mut self, req: &serde_json::Value) -> String {
        let topics: Vec<String> = req
            .get("topics")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        if let Some(sub_id) = self.sub_id {
            self.router.unsubscribe_topics(sub_id, &topics).await;
        }

        serde_json::to_string(&serde_json::json!({"ok": true})).unwrap()
    }

    /// Handle an emit command (external client publishing an event).
    pub async fn handle_emit(&self, req: &serde_json::Value) -> String {
        let topic = match req.get("topic").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return r#"{"error":"emit requires 'topic' field"}"#.to_string(),
        };
        let source = req
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let data = req.get("data").cloned().unwrap_or(serde_json::json!({}));

        let seq = self.router.emit_external(topic, source, data).await;
        serde_json::to_string(&serde_json::json!({"ok": true, "seq": seq})).unwrap()
    }

    /// Take the event receiver (used by the connection loop).
    pub fn take_event_rx(&mut self) -> Option<mpsc::Receiver<Event>> {
        self.event_rx.take()
    }

    /// Clean up on disconnect.
    pub async fn cleanup(&self) {
        if let Some(sub_id) = self.sub_id {
            self.router.remove_subscriber(sub_id).await;
        }
    }
}

// -- Tests --------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_exact_match() {
        let p = TopicPattern::parse("node.spliced");
        assert!(p.matches("node.spliced"));
        assert!(!p.matches("node.created"));
        assert!(!p.matches("lsp.diagnostics"));
    }

    #[test]
    fn topic_star_matches_one_segment() {
        let p = TopicPattern::parse("node.*");
        assert!(p.matches("node.spliced"));
        assert!(p.matches("node.created"));
        assert!(!p.matches("node.staging.committed"));
        assert!(!p.matches("lsp.diagnostics"));
    }

    #[test]
    fn topic_double_star_matches_any() {
        let p = TopicPattern::parse("node.**");
        assert!(p.matches("node.spliced"));
        assert!(p.matches("node.created"));
        assert!(p.matches("node.staging.committed"));
        assert!(!p.matches("lsp.diagnostics"));
    }

    #[test]
    fn topic_firehose() {
        let p = TopicPattern::parse("**");
        assert!(p.matches("node.spliced"));
        assert!(p.matches("lsp.diagnostics"));
        assert!(p.matches("arena.generation"));
        assert!(p.matches("anything.at.all"));
    }

    #[test]
    fn topic_star_only() {
        let p = TopicPattern::parse("*");
        // Single-segment topics
        assert!(p.matches("heartbeat"));
        // Multi-segment should not match
        assert!(!p.matches("node.spliced"));
    }

    #[test]
    fn topic_mixed_pattern() {
        let p = TopicPattern::parse("lsp.**.updated");
        assert!(p.matches("lsp.hover.updated"));
        assert!(p.matches("lsp.definitions.updated"));
        assert!(!p.matches("lsp.diagnostics"));
    }

    #[test]
    fn event_log_capacity() {
        let mut log = EventLog::new(3);
        for i in 1..=5 {
            log.push(Event {
                event: true,
                seq: i,
                topic: "test".into(),
                source: "test".into(),
                data: serde_json::json!({}),
            });
        }
        assert_eq!(log.events.len(), 3);
        assert_eq!(log.events.front().unwrap().seq, 3);
        assert_eq!(log.events.back().unwrap().seq, 5);
    }

    #[test]
    fn event_log_replay_since() {
        let mut log = EventLog::new(100);
        for i in 1..=5 {
            log.push(Event {
                event: true,
                seq: i,
                topic: "test".into(),
                source: "test".into(),
                data: serde_json::json!({}),
            });
        }
        let (events, gap) = log.since(3);
        assert!(!gap);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 4);
        assert_eq!(events[1].seq, 5);
    }

    #[test]
    fn event_log_replay_gap() {
        let mut log = EventLog::new(3);
        for i in 1..=5 {
            log.push(Event {
                event: true,
                seq: i,
                topic: "test".into(),
                source: "test".into(),
                data: serde_json::json!({}),
            });
        }
        // Requesting since=1, but log starts at 3 -> gap
        let (events, gap) = log.since(1);
        assert!(gap);
        assert_eq!(events.len(), 3);
    }

    #[tokio::test]
    async fn router_subscribe_and_emit() {
        let router = EventRouter::new(100);
        let emitter = router.emitter();

        let (_, mut rx, _, _) = router
            .subscribe(
                &["node.*".to_string()],
                None,
                0,
                OverflowPolicy::DropOldest,
                64,
            )
            .await;

        emitter.emit(
            "node.spliced",
            "leyline",
            serde_json::json!({"node_ids": ["a"]}),
        );

        // Give the dispatch loop a moment
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let event = rx.try_recv().unwrap();
        assert_eq!(event.seq, 1);
        assert_eq!(event.topic, "node.spliced");
        assert_eq!(event.source, "leyline");
    }

    #[tokio::test]
    async fn router_echo_suppression() {
        let router = EventRouter::new(100);
        let emitter = router.emitter();

        let (_, mut rx, _, _) = router
            .subscribe(
                &["node.*".to_string()],
                Some("mache".to_string()),
                0,
                OverflowPolicy::DropOldest,
                64,
            )
            .await;

        // Event from mache should be suppressed
        emitter.emit("node.spliced", "mache", serde_json::json!({}));
        // Event from leyline should be delivered
        emitter.emit("node.created", "leyline", serde_json::json!({}));

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let event = rx.try_recv().unwrap();
        assert_eq!(event.topic, "node.created");
        assert!(rx.try_recv().is_err()); // No more events (mache one was suppressed)
    }

    #[tokio::test]
    async fn router_topic_filtering() {
        let router = EventRouter::new(100);
        let emitter = router.emitter();

        let (_, mut rx, _, _) = router
            .subscribe(
                &["lsp.*".to_string()],
                None,
                0,
                OverflowPolicy::DropOldest,
                64,
            )
            .await;

        emitter.emit("node.spliced", "leyline", serde_json::json!({}));
        emitter.emit("lsp.diagnostics", "leyline", serde_json::json!({}));

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let event = rx.try_recv().unwrap();
        assert_eq!(event.topic, "lsp.diagnostics");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn router_replay_on_subscribe() {
        let router = EventRouter::new(100);

        // Emit some events before subscribing
        router
            .emit_external(
                "node.spliced".into(),
                "leyline".into(),
                serde_json::json!({}),
            )
            .await;
        router
            .emit_external(
                "node.created".into(),
                "leyline".into(),
                serde_json::json!({}),
            )
            .await;

        // Subscribe with since=0 (replay all)
        let (_, _, replay, gap) = router
            .subscribe(
                &["node.*".to_string()],
                None,
                0,
                OverflowPolicy::DropOldest,
                64,
            )
            .await;

        assert!(!gap);
        assert_eq!(replay.len(), 2);
        assert_eq!(replay[0].seq, 1);
        assert_eq!(replay[1].seq, 2);
    }
}
