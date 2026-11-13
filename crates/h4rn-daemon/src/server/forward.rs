//! Generic transient-event forwarding.
//!
//! Every service subscribes to its own broadcast channel and pushes frames at
//! the client. The loops differ only in the event type, the topic filter, the
//! mapping into a [`ServerFrame`] and how they react to a lagged receiver or a
//! full outbound queue, so one [`forward_loop`] covers them all.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use h4rn_protocol::{ServerFrame, Topic};
use tokio::sync::{Notify, broadcast, mpsc};

use super::{Connection, Outbound};

/// What a forwarder does when the broadcast receiver falls behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LagPolicy {
    /// Report the overflow and drop the connection so the client resumes.
    Disconnect,
    /// Log the miss and keep forwarding newer events.
    Skip,
}

/// What a forwarder does when the client's outbound queue is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FullPolicy {
    /// Report the overflow and drop the connection so the client resumes.
    Disconnect,
    /// Drop this event; another loop owns the disconnect.
    Drop,
}

/// Per-channel behavior for [`forward_loop`].
#[derive(Clone, Copy, Debug)]
pub(super) struct ForwardConfig {
    /// Human label used in lag warnings ("event broadcast", "skill broadcast").
    pub(super) label: &'static str,
    /// Topic a client must subscribe to, or `None` for always-forwarded frames.
    pub(super) topic: Option<Topic>,
    /// Reaction to a lagged receiver.
    pub(super) lag: LagPolicy,
    /// Reaction to a full outbound queue.
    pub(super) full: FullPolicy,
}

impl ForwardConfig {
    /// A durable-thread-journal forwarder.
    pub(super) const JOURNAL: Self = Self {
        label: "event broadcast",
        topic: Some(Topic::Threads),
        lag: LagPolicy::Disconnect,
        full: FullPolicy::Disconnect,
    };

    /// A forwarder with no topic gate and fail-closed overflow handling.
    pub(super) const fn always_on(label: &'static str) -> Self {
        Self {
            label,
            topic: None,
            lag: LagPolicy::Disconnect,
            full: FullPolicy::Disconnect,
        }
    }
}

/// The per-connection plumbing every forwarder needs.
#[derive(Clone)]
pub(super) struct ForwardContext {
    /// This connection's outbound queue.
    pub(super) tx: mpsc::Sender<Outbound>,
    /// Topics the client subscribed to after `Hello`.
    pub(super) topics: Arc<RwLock<Vec<Topic>>>,
    /// False until the client completes `Hello`.
    pub(super) ready: Arc<AtomicBool>,
    /// Set when this connection must be dropped with a resume hint.
    pub(super) overflowed: Arc<AtomicBool>,
    /// Wakes the writer loop on overflow.
    pub(super) notify: Arc<Notify>,
    /// Highest durable sequence this connection has seen.
    pub(super) last_seq: Arc<AtomicU64>,
}

impl ForwardContext {
    /// Snapshots the shared state of one connection.
    pub(super) fn for_connection(conn: &Connection) -> Self {
        Self {
            tx: conn.tx.clone(),
            topics: Arc::clone(&conn.topics),
            ready: Arc::clone(&conn.ready),
            overflowed: Arc::clone(&conn.overflowed),
            notify: Arc::clone(&conn.notify),
            last_seq: Arc::clone(&conn.last_seq),
        }
    }
}

/// One mapped frame plus, for the durable journal, the sequence to remember.
pub(super) struct ForwardFrame {
    /// Frame sent to the client.
    pub(super) frame: ServerFrame,
    /// New durable sequence this connection has reached, if any.
    pub(super) advance_to: Option<u64>,
}

impl ForwardFrame {
    /// A journal event that advances the client's resume cursor.
    pub(super) fn durable(frame: ServerFrame, seq: u64) -> Self {
        Self {
            frame,
            advance_to: Some(seq),
        }
    }

    /// A transient frame that carries the current durable sequence unchanged.
    pub(super) fn transient(frame: ServerFrame) -> Self {
        Self {
            frame,
            advance_to: None,
        }
    }
}

/// Forwards one broadcast channel into a client's outbound queue.
///
/// `map` turns a channel item into a protocol frame. The current `last_seq` is
/// passed in so a transient frame can echo the durable cursor, and the closure
/// may capture service context such as the local host id. Returning `None`
/// drops the item, which services use for internal events the UI never sees.
pub(super) async fn forward_loop<T, F>(
    mut subscription: broadcast::Receiver<T>,
    context: ForwardContext,
    config: ForwardConfig,
    map: F,
) where
    T: Clone + Send + 'static,
    F: Fn(T, &AtomicU64) -> Option<ForwardFrame> + Send + 'static,
{
    loop {
        let item = match subscription.recv().await {
            Ok(item) => item,
            Err(broadcast::error::RecvError::Lagged(missed)) => match config.lag {
                LagPolicy::Disconnect => {
                    tracing::warn!(missed, "client fell behind the {}", config.label);
                    context.overflowed.store(true, Ordering::Relaxed);
                    context.notify.notify_waiters();
                    return;
                }
                LagPolicy::Skip => continue,
            },
            Err(broadcast::error::RecvError::Closed) => return,
        };

        if !context.ready.load(Ordering::Relaxed) {
            continue;
        }
        if let Some(required) = config.topic {
            let subscribed = {
                let topics = context
                    .topics
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                topics
                    .iter()
                    .any(|topic| matches!(topic, Topic::All) || *topic == required)
            };
            if !subscribed {
                continue;
            }
        }

        let Some(forwarded) = map(item, &context.last_seq) else {
            continue;
        };
        if let Some(seq) = forwarded.advance_to {
            context.last_seq.store(seq, Ordering::Relaxed);
        }
        let Ok(text) = serde_json::to_string(&forwarded.frame) else {
            continue;
        };
        match context.tx.try_send(Outbound::Text(text)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => match config.full {
                FullPolicy::Disconnect => {
                    tracing::warn!("client outbound queue is full; dropping the connection");
                    context.overflowed.store(true, Ordering::Relaxed);
                    context.notify.notify_waiters();
                    return;
                }
                FullPolicy::Drop => return,
            },
            Err(mpsc::error::TrySendError::Closed(_)) => return,
        }
    }
}
