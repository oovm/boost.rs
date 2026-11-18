//! PostgreSQL `LISTEN` / `NOTIFY` / `UNLISTEN` fan-out.
//!
//! The hub is session-level state, not storage state: one hub belongs to one
//! runtime, so every connection that runtime serves shares the channel
//! registry. A listener owns a numeric sink; `NOTIFY` queues one message per
//! subscriber and the protocol session delivers them as `NotificationResponse`
//! frames. Dropping the session unregisters the sink, so a closed connection
//! never receives a later notification.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Mutex, MutexGuard},
};

/// One queued notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// Sending backend process id.
    pub process_id: u32,
    /// Channel name.
    pub channel: String,
    /// Payload, empty when `NOTIFY` carried none.
    pub payload: String,
}

#[derive(Default)]
struct HubState {
    next_sink: u64,
    subscribers: BTreeMap<String, BTreeSet<u64>>,
    queues: BTreeMap<u64, VecDeque<Notification>>,
}

/// Channel registry shared by every session of one runtime.
#[derive(Default)]
pub struct NotificationHub {
    state: Mutex<HubState>,
}

impl std::fmt::Debug for NotificationHub {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("NotificationHub").finish_non_exhaustive()
    }
}

impl NotificationHub {
    /// Creates an empty hub.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a session sink and returns its id.
    pub fn register(&self) -> u64 {
        let mut state = self.lock();
        state.next_sink += 1;
        let id = state.next_sink;
        state.queues.insert(id, VecDeque::new());
        id
    }

    /// Unregisters a sink and drops every message still queued for it.
    pub fn unregister(&self, sink: u64) {
        let mut state = self.lock();
        state.queues.remove(&sink);
        for subscribers in state.subscribers.values_mut() {
            subscribers.remove(&sink);
        }
        state.subscribers.retain(|_, subscribers| !subscribers.is_empty());
    }

    /// Subscribes `sink` to `channel`.
    pub fn listen(&self, sink: u64, channel: &str) {
        let mut state = self.lock();
        state.subscribers.entry(channel.to_owned()).or_default().insert(sink);
    }

    /// Removes one subscription.
    pub fn unlisten(&self, sink: u64, channel: &str) {
        let mut state = self.lock();
        if let Some(subscribers) = state.subscribers.get_mut(channel) {
            subscribers.remove(&sink);
            if subscribers.is_empty() {
                state.subscribers.remove(channel);
            }
        }
    }

    /// Removes every subscription of `sink`.
    pub fn unlisten_all(&self, sink: u64) {
        let mut state = self.lock();
        for subscribers in state.subscribers.values_mut() {
            subscribers.remove(&sink);
        }
        state.subscribers.retain(|_, subscribers| !subscribers.is_empty());
    }

    /// Queues `notification` for every subscriber of its channel.
    pub fn notify(&self, notification: Notification) {
        let mut state = self.lock();
        let Some(subscribers) = state.subscribers.get(&notification.channel).cloned()
        else {
            return;
        };
        for sink in subscribers {
            state.queues.entry(sink).or_default().push_back(notification.clone());
        }
    }

    /// Drains the messages queued for `sink`, in delivery order.
    pub fn take(&self, sink: u64) -> Vec<Notification> {
        let mut state = self.lock();
        match state.queues.get_mut(&sink) {
            Some(queue) => queue.drain(..).collect(),
            None => Vec::new(),
        }
    }

    /// Whether `sink` is subscribed to `channel`.
    pub fn is_listening(&self, sink: u64, channel: &str) -> bool {
        self.lock()
            .subscribers
            .get(channel)
            .is_some_and(|subscribers| subscribers.contains(&sink))
    }

    fn lock(&self) -> MutexGuard<'_, HubState> {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(channel: &str, payload: &str) -> Notification {
        Notification { process_id: 7, channel: channel.to_owned(), payload: payload.to_owned() }
    }

    #[test]
    fn only_subscribers_receive_their_channels() {
        let hub = NotificationHub::new();
        let listener = hub.register();
        let other = hub.register();
        hub.listen(listener, "events");
        hub.notify(message("events", "first"));
        hub.notify(message("other", "ignored"));
        assert_eq!(hub.take(listener), vec![message("events", "first")]);
        assert!(hub.take(other).is_empty());
    }

    #[test]
    fn unlisten_and_unregister_stop_delivery() {
        let hub = NotificationHub::new();
        let listener = hub.register();
        hub.listen(listener, "events");
        hub.unlisten(listener, "events");
        hub.notify(message("events", "lost"));
        assert!(hub.take(listener).is_empty());
        hub.listen(listener, "events");
        hub.unregister(listener);
        hub.notify(message("events", "lost"));
        assert!(hub.take(listener).is_empty());
        assert!(!hub.is_listening(listener, "events"));
    }
}
