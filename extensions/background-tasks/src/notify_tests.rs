//! The notifier against a scripted pi, on a clock the tests move.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::notify::{CHECK_EVERY, LOST_AFTER, MAX_SENDS, Notifier, Pi, SendError, saved_ids};
use crate::registry::Registry;
use crate::task::{self, Status, Task, session_dir};
use crate::tests::{Temp, sample};

/// A pi that answers from fields: idle or not, the notifications its session holds, and
/// whether it saves what it takes (else the message waits in a queue that a stop loses).
#[derive(Default)]
struct FakePi {
    idle: Cell<bool>,
    saves: Cell<bool>,
    session: RefCell<HashSet<String>>,
    sent: RefCell<Vec<String>>,
    reads: Cell<u32>,
    idle_checks: Cell<u32>,
    fail_next: RefCell<Option<SendError>>,
}

impl FakePi {
    fn new(idle: bool, saves: bool) -> FakePi {
        let pi = FakePi::default();
        pi.idle.set(idle);
        pi.saves.set(saves);
        pi
    }

    fn sent(&self) -> Vec<String> {
        self.sent.borrow().clone()
    }

    fn save(&self, id: &str) {
        self.session.borrow_mut().insert(id.to_owned());
    }
}

impl Pi for FakePi {
    async fn is_idle(&self) -> bool {
        self.idle_checks.set(self.idle_checks.get() + 1);
        self.idle.get()
    }

    async fn saved(&self) -> Option<HashSet<String>> {
        self.reads.set(self.reads.get() + 1);
        Some(self.session.borrow().clone())
    }

    async fn send(&self, task: &Task) -> Result<(), SendError> {
        if let Some(err) = self.fail_next.borrow_mut().take() {
            return Err(err);
        }
        self.sent.borrow_mut().push(task.id.clone());
        if self.saves.get() {
            self.save(&task.id);
        }
        Ok(())
    }
}

/// A finished task that notifies, saved in `sessions`.
fn finished(sessions: &Path, id: &str, notice_sent_at: Option<u64>) -> Task {
    let mut task = sample(id, Status::Completed);
    task.dir = sessions.join(id);
    task.ended_at = Some(2_000);
    task.exit_code = Some(0);
    task.notice_sent_at = notice_sent_at;
    task::create_private_dir(&task.dir).unwrap();
    task.save().unwrap();
    task
}

fn bound(sessions: &Path) -> Registry {
    let registry = Registry::default();
    registry.bind(sessions.to_path_buf());
    registry
}

/// One monitor tick at `at`.
async fn tick(notifier: &mut Notifier, pi: &FakePi, registry: &Registry, at: Instant) {
    let pending = registry.poll(false).await;
    notifier.tick(pi, registry, &pending, at).await;
}

/// Ticks every 500 ms from `from` to `to` (both included).
async fn ticks(
    notifier: &mut Notifier,
    pi: &FakePi,
    registry: &Registry,
    from: Instant,
    to: Instant,
) {
    let mut at = from;
    while at <= to {
        tick(notifier, pi, registry, at).await;
        at += Duration::from_millis(500);
    }
}

fn on_disk(task: &Task) -> Task {
    let bytes = std::fs::read(task.dir.join(task::TASK_FILE)).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn secs(n: f64) -> Duration {
    Duration::from_secs_f64(n)
}

#[tokio::test]
async fn a_sent_notification_counts_once_it_is_in_the_session() {
    let temp = Temp::new();
    let sessions = session_dir(&temp.0, Some("s"));
    let task = finished(&sessions, "aaaa0001", None);
    let registry = bound(&sessions);
    // pi runs a turn, so the message waits in its queue.
    let pi = FakePi::new(false, false);
    let mut notifier = Notifier::default();
    let t0 = Instant::now();
    tick(&mut notifier, &pi, &registry, t0).await;
    assert_eq!(pi.sent(), ["aaaa0001"]);
    let sent = on_disk(&task);
    assert!(sent.notice_sent_at.is_some() && !sent.notified, "{sent:?}");

    tick(&mut notifier, &pi, &registry, t0 + secs(1.0)).await;
    assert_eq!(pi.idle_checks.get(), 0, "too soon after the send");
    tick(&mut notifier, &pi, &registry, t0 + CHECK_EVERY).await;
    assert_eq!((pi.idle_checks.get(), pi.reads.get()), (1, 0));
    tick(&mut notifier, &pi, &registry, t0 + secs(4.0)).await;
    assert_eq!(pi.idle_checks.get(), 1, "at most one check per CHECK_EVERY");

    // The turn ends and pi saves the queued message.
    pi.idle.set(true);
    pi.save("aaaa0001");
    tick(&mut notifier, &pi, &registry, t0 + secs(6.0)).await;
    assert_eq!((pi.idle_checks.get(), pi.reads.get()), (2, 1));
    assert!(on_disk(&task).notified);
    ticks(
        &mut notifier,
        &pi,
        &registry,
        t0 + secs(6.5),
        t0 + secs(40.0),
    )
    .await;
    assert_eq!(pi.sent().len(), 1);
    assert_eq!((pi.idle_checks.get(), pi.reads.get()), (2, 1));
}

#[tokio::test]
async fn a_lost_notification_is_sent_again_after_pi_was_idle_without_it() {
    let temp = Temp::new();
    let sessions = session_dir(&temp.0, Some("s"));
    let task = finished(&sessions, "bbbb0001", None);
    let registry = bound(&sessions);
    let pi = FakePi::new(true, false);
    let mut notifier = Notifier::default();
    let t0 = Instant::now();
    tick(&mut notifier, &pi, &registry, t0).await;
    tick(&mut notifier, &pi, &registry, t0 + CHECK_EVERY).await;
    assert_eq!(
        pi.reads.get(),
        1,
        "the first check after the send reads the session"
    );
    // pi stays idle without the message: no read until LOST_AFTER, then one more and a resend.
    ticks(
        &mut notifier,
        &pi,
        &registry,
        t0 + secs(3.5),
        t0 + CHECK_EVERY + LOST_AFTER - secs(0.5),
    )
    .await;
    assert_eq!((pi.reads.get(), pi.sent().len()), (1, 1));
    pi.saves.set(true);
    ticks(
        &mut notifier,
        &pi,
        &registry,
        t0 + CHECK_EVERY + LOST_AFTER,
        t0 + secs(17.0),
    )
    .await;
    assert_eq!(pi.reads.get(), 2);
    assert_eq!(pi.sent(), ["bbbb0001", "bbbb0001"]);
    assert!(!on_disk(&task).notified);
    ticks(
        &mut notifier,
        &pi,
        &registry,
        t0 + secs(17.5),
        t0 + secs(60.0),
    )
    .await;
    assert!(on_disk(&task).notified);
    assert_eq!((pi.reads.get(), pi.sent().len()), (3, 2));
}

#[tokio::test]
async fn a_notification_of_another_process_is_not_sent_again_when_the_session_has_it() {
    let temp = Temp::new();
    let sessions = session_dir(&temp.0, Some("s"));
    let earlier = Some(task::now_ms() - 5_000);
    let saved = finished(&sessions, "cccc0001", earlier);
    let lost = finished(&sessions, "cccc0002", earlier);
    let registry = bound(&sessions);
    let pi = FakePi::new(false, true);
    pi.save("cccc0001");
    let mut notifier = Notifier::default();
    let t0 = Instant::now();
    // While pi runs a turn, the message may still wait in its queue: no read, no send.
    ticks(&mut notifier, &pi, &registry, t0, t0 + secs(30.0)).await;
    assert_eq!(pi.reads.get(), 0);
    assert!(pi.sent().is_empty());

    // The busy checks ran every CHECK_EVERY from t0, so the next one is due at t0 + 33 s.
    pi.idle.set(true);
    let t1 = t0 + secs(33.0);
    tick(&mut notifier, &pi, &registry, t1 - secs(0.5)).await;
    assert_eq!(pi.reads.get(), 0);
    tick(&mut notifier, &pi, &registry, t1).await;
    assert_eq!(pi.reads.get(), 1);
    assert!(on_disk(&saved).notified);
    assert!(!on_disk(&lost).notified);
    assert!(pi.sent().is_empty());
    ticks(&mut notifier, &pi, &registry, t1, t1 + secs(20.0)).await;
    assert_eq!(pi.sent(), ["cccc0002"], "only the missing one, once");
    assert!(on_disk(&lost).notified);
    assert!(pi.reads.get() <= 4, "{} reads", pi.reads.get());
}

#[tokio::test]
async fn the_session_is_read_rarely_and_once_for_all_tasks() {
    let temp = Temp::new();
    let sessions = session_dir(&temp.0, Some("s"));
    let earlier = Some(task::now_ms() - 5_000);
    for n in 0..5 {
        finished(&sessions, &format!("dddd000{n}"), earlier);
    }
    let registry = bound(&sessions);
    let pi = FakePi::new(true, false);
    let mut notifier = Notifier::default();
    let t0 = Instant::now();
    ticks(&mut notifier, &pi, &registry, t0, t0 + secs(30.0)).await;
    // 61 ticks. Idle checks at 0, 3, 6, 9, 12 s, then (after the resend at 12.5 s waits
    // CHECK_EVERY) at 15.5, 18.5, 21.5, 24.5, 27.5 s. Reads, each for all five tasks: the first
    // check, LOST_AFTER later (resend), the first check after the resend, and LOST_AFTER later.
    assert_eq!(pi.idle_checks.get(), 10);
    assert_eq!(pi.reads.get(), 4);
    assert_eq!(pi.sent().len(), 10);
}

#[tokio::test]
async fn a_refused_send_is_retried_and_a_closed_connection_keeps_the_record() {
    let temp = Temp::new();
    let sessions = session_dir(&temp.0, Some("s"));
    let task = finished(&sessions, "eeee0001", None);
    let registry = bound(&sessions);
    let pi = FakePi::new(false, true);
    let mut notifier = Notifier::default();
    let t0 = Instant::now();
    *pi.fail_next.borrow_mut() = Some(SendError::Refused("not now".into()));
    tick(&mut notifier, &pi, &registry, t0).await;
    assert!(pi.sent().is_empty());
    assert_eq!(on_disk(&task).notice_sent_at, None, "pi does not hold it");
    tick(&mut notifier, &pi, &registry, t0 + secs(0.5)).await;
    assert!(pi.sent().is_empty(), "the retry waits");
    tick(&mut notifier, &pi, &registry, t0 + secs(1.0)).await;
    assert_eq!(pi.sent(), ["eeee0001"]);

    let closed = finished(&sessions, "eeee0002", None);
    let registry = bound(&sessions);
    let pi = FakePi::new(false, false);
    let mut notifier = Notifier::default();
    *pi.fail_next.borrow_mut() = Some(SendError::Closed);
    ticks(&mut notifier, &pi, &registry, t0, t0 + secs(30.0)).await;
    assert!(pi.sent().is_empty());
    let record = on_disk(&closed);
    assert!(record.notice_sent_at.is_some() && !record.notified);

    // The next process finds the message in the session and sends nothing.
    let pi = FakePi::new(true, true);
    pi.save("eeee0002");
    let mut notifier = Notifier::default();
    tick(&mut notifier, &pi, &bound(&sessions), t0).await;
    assert!(pi.sent().is_empty());
    assert!(on_disk(&closed).notified);
}

#[tokio::test]
async fn a_notification_that_never_reaches_the_session_stops_after_max_sends() {
    let temp = Temp::new();
    let sessions = session_dir(&temp.0, Some("s"));
    let task = finished(&sessions, "ffff0001", None);
    let registry = bound(&sessions);
    let pi = FakePi::new(true, false);
    let mut notifier = Notifier::default();
    let t0 = Instant::now();
    ticks(&mut notifier, &pi, &registry, t0, t0 + secs(90.0)).await;
    assert_eq!(pi.sent().len(), MAX_SENDS as usize);
    assert!(on_disk(&task).notified);
}

#[tokio::test]
async fn old_task_records_keep_their_notified_flag() {
    let temp = Temp::new();
    let sessions = session_dir(&temp.0, Some("s"));
    for (id, notified) in [("0ld00001", true), ("0ld00002", false)] {
        let dir = sessions.join(id);
        task::create_private_dir(&dir).unwrap();
        // task.json as versions before `noticeSentAt` wrote it.
        let json = serde_json::json!({
            "id": id, "name": "old", "command": "true", "cwd": "/", "isAgent": false,
            "pid": 1, "dir": dir, "startedAt": 1000, "endedAt": 2000, "timeoutSeconds": null,
            "notifyOnCompletion": true, "triggerOnCompletion": true, "surviveReload": true,
            "status": "completed", "exitCode": 0, "notified": notified
        });
        std::fs::write(dir.join(task::TASK_FILE), json.to_string()).unwrap();
    }
    let registry = bound(&sessions);
    assert_eq!(registry.tasks().len(), 2);
    let pi = FakePi::new(true, true);
    let mut notifier = Notifier::default();
    let t0 = Instant::now();
    ticks(&mut notifier, &pi, &registry, t0, t0 + secs(10.0)).await;
    assert_eq!(pi.sent(), ["0ld00002"]);
    assert_eq!(pi.reads.get(), 1);
    assert!(registry.resolve("0ld00002").unwrap().notified);
}

#[test]
fn saved_ids_reads_the_notification_entries() {
    let entries = vec![
        serde_json::json!({"type":"custom_message","customType":"background-task-notification","details":{"id":"a1"}}),
        serde_json::json!({"type":"custom_message","customType":"subagent-notify","details":{"id":"b1"}}),
        serde_json::json!({"type":"message","customType":"background-task-notification","details":{"id":"c1"}}),
        serde_json::json!({"type":"custom_message","customType":"background-task-notification","details":{}}),
    ];
    assert_eq!(saved_ids(&entries), HashSet::from(["a1".to_owned()]));
}
