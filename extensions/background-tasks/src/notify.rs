//! Completion notifications that reach the session once across pi restarts.
//!
//! pi takes a message it gets during a turn into a queue, and a pi that is stopped loses that
//! queue. So a notification counts as delivered (`notified`) only once its `custom_message`
//! entry is in the session. `noticeSentAt` in task.json records each send before it happens:
//! a process that finds it checks the session before it sends again, and sends again only when
//! the notification is still missing after pi was idle for [`LOST_AFTER`].

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use pi_ext_sdk::prelude::*;

use crate::registry::Registry;
use crate::task::{self, Task};
use crate::ui::{NOTIFICATION, notification_text};

/// pi saves a message it accepted before it is idle again, so a sent notification that is not
/// in the session after pi was idle this long was lost.
pub const LOST_AFTER: Duration = Duration::from_secs(10);
/// The least time between two checks of pi. A check asks whether pi is idle and, only then and
/// only for an unconfirmed notification, reads the session entries (a copy of the whole session).
pub const CHECK_EVERY: Duration = Duration::from_secs(3);
/// Sends of one notification by one process before it stops sending it.
pub const MAX_SENDS: u32 = 3;
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// Bus channel that ends a `sleep` early when a notification arrives.
const WAKE_CHANNEL: &str = "agent:wake";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// The connection to pi closed, so pi may hold the message or not.
    Closed,
    /// pi answered with an error, so it does not hold the message.
    Refused(String),
}

/// What the notifier needs from pi.
pub(crate) trait Pi {
    /// Whether pi runs no turn, compaction or retry; `false` when it does not answer.
    async fn is_idle(&self) -> bool;
    /// The task ids of the notifications in the session; `None` when pi does not answer.
    async fn saved(&self) -> Option<HashSet<String>>;
    async fn send(&self, task: &Task) -> Result<(), SendError>;
}

/// pi through the extension's connection.
pub(crate) struct HostPi<'a>(pub &'a Host);

impl Pi for HostPi<'_> {
    async fn is_idle(&self) -> bool {
        self.0.is_idle().await.unwrap_or(false)
    }

    async fn saved(&self) -> Option<HashSet<String>> {
        match self.0.entries().await {
            Ok(entries) => Some(saved_ids(&entries)),
            Err(err) => {
                pi_ext_sdk::debug!("cannot read the session entries: {err}");
                None
            }
        }
    }

    async fn send(&self, task: &Task) -> Result<(), SendError> {
        let message = CustomMessage::new(NOTIFICATION, notification_text(task))
            .display(true)
            .details(task)
            .deliver_as(DeliverAs::FollowUp)
            .trigger_turn(task.trigger_on_completion);
        match self.0.send_message(message).await {
            Ok(()) => {}
            Err(err) if err == Error::closed() => return Err(SendError::Closed),
            Err(err) => return Err(SendError::Refused(err.to_string())),
        }
        if task.trigger_on_completion {
            let reason = format!("task {} finished", task.id);
            let wake = json!({ "source": "background-tasks", "reason": reason });
            let _ = self.0.emit(WAKE_CHANNEL, wake).await;
        }
        Ok(())
    }
}

/// The task ids (`details.id`) of the notification entries in `entries`.
pub fn saved_ids(entries: &[Value]) -> HashSet<String> {
    entries
        .iter()
        .filter(|entry| entry["type"] == "custom_message" && entry["customType"] == NOTIFICATION)
        .filter_map(|entry| entry["details"]["id"].as_str().map(str::to_owned))
        .collect()
}

/// This process's notification state, by task id.
#[derive(Default)]
pub struct Notifier {
    tracks: HashMap<String, Track>,
    /// No check of pi before this.
    next_check: Option<Instant>,
}

struct Track {
    /// This process's last send.
    sent: Option<Instant>,
    sends: u32,
    /// The notification was lost: send it again.
    resend: bool,
    retry_at: Option<Instant>,
    backoff: Duration,
    /// Since when pi is seen idle while the session lacks the notification.
    idle_since: Option<Instant>,
    /// The session may have gained the notification since the last read: there was none since
    /// the send, or pi ran since.
    unread: bool,
}

impl Default for Track {
    fn default() -> Self {
        Track {
            sent: None,
            sends: 0,
            resend: false,
            retry_at: None,
            backoff: RETRY_FIRST,
            idle_since: None,
            unread: true,
        }
    }
}

impl Notifier {
    /// Sends the notifications that `pending` owes and confirms or resends the sent ones.
    /// `pending` holds the finished tasks that still owe one ([`Registry::poll`]).
    pub(crate) async fn tick(
        &mut self,
        pi: &impl Pi,
        registry: &Registry,
        pending: &[Task],
        now: Instant,
    ) {
        self.tracks
            .retain(|id, _| pending.iter().any(|task| task.id == *id));
        let mut sent = Vec::new();
        for task in pending {
            let track = self.tracks.entry(task.id.clone()).or_default();
            if task.notice_sent_at.is_some() && !track.resend {
                sent.push(task);
            } else if track.retry_at.is_none_or(|at| now >= at) {
                self.send(pi, registry, task, now).await;
            }
        }
        if !sent.is_empty() {
            self.check(pi, registry, &sent, now).await;
        }
    }

    /// Records the send in task.json, then sends.
    async fn send(&mut self, pi: &impl Pi, registry: &Registry, task: &Task, now: Instant) {
        let sends = self.tracks.get(&task.id).map_or(0, |track| track.sends);
        if sends >= MAX_SENDS {
            pi_ext_sdk::warn!(
                "the notification of task {} is not in the session after {sends} sends; not sending it again",
                task.id
            );
            registry.mark_notified(&task.id);
            self.tracks.remove(&task.id);
            return;
        }
        let previous = task.notice_sent_at;
        let Some(task) = registry.set_notice_sent(&task.id, Some(task::now_ms())) else {
            return;
        };
        let result = pi.send(&task).await;
        let track = self.tracks.entry(task.id.clone()).or_default();
        match result {
            Ok(()) => {
                *track = Track {
                    sent: Some(now),
                    sends: track.sends + 1,
                    ..Track::default()
                };
            }
            // pi may hold it, so the next process checks the session before it sends again.
            Err(SendError::Closed) => track.resend = false,
            Err(SendError::Refused(err)) => {
                pi_ext_sdk::debug!(
                    "pi did not take the notification of task {}: {err}",
                    task.id
                );
                registry.set_notice_sent(&task.id, previous);
                track.retry_at = Some(now + track.backoff);
                track.backoff = (track.backoff * 2).min(RETRY_MAX);
            }
        }
    }

    /// Confirms sent notifications that are in the session, and marks lost ones for a resend.
    async fn check(&mut self, pi: &impl Pi, registry: &Registry, sent: &[&Task], now: Instant) {
        if self.next_check.is_some_and(|at| now < at) {
            return;
        }
        // A notification this process just sent needs a moment to reach the session.
        let ready: Vec<&Task> = sent
            .iter()
            .copied()
            .filter(|task| {
                let sent = self.tracks.get(&task.id).and_then(|track| track.sent);
                sent.is_none_or(|at| now.saturating_duration_since(at) >= CHECK_EVERY)
            })
            .collect();
        if ready.is_empty() {
            return;
        }
        self.next_check = Some(now + CHECK_EVERY);
        let idle = pi.is_idle().await;
        let mut read = false;
        for task in &ready {
            let track = self.tracks.entry(task.id.clone()).or_default();
            if idle {
                let since = *track.idle_since.get_or_insert(now);
                read |= track.unread || now.saturating_duration_since(since) >= LOST_AFTER;
            } else {
                track.idle_since = None;
                track.unread = true;
            }
        }
        if !read {
            return;
        }
        let Some(saved) = pi.saved().await else {
            return;
        };
        for task in ready {
            if saved.contains(&task.id) {
                registry.mark_notified(&task.id);
                self.tracks.remove(&task.id);
                continue;
            }
            let Some(track) = self.tracks.get_mut(&task.id) else {
                continue;
            };
            track.unread = false;
            let lost = track
                .idle_since
                .is_some_and(|since| now.saturating_duration_since(since) >= LOST_AFTER);
            if lost {
                pi_ext_sdk::warn!(
                    "the notification of task {} is not in the session; sending it again",
                    task.id
                );
                track.resend = true;
                track.idle_since = None;
            }
        }
    }
}
