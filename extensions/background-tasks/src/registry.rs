use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::identity::{self, Owner};
use crate::launch;
use crate::process;
use crate::task::{self, EXIT_FILE, OUTPUT_FILE, Status, Task, default_name, resolve_in};

const KILL_GRACE: Duration = Duration::from_secs(3);
const SHUTDOWN_GRACE: Duration = Duration::from_millis(700);

#[derive(Debug, Clone, Default)]
pub struct StartOptions {
    pub name: Option<String>,
    pub command: String,
    pub description: Option<String>,
    pub is_agent: bool,
    pub timeout_seconds: Option<u64>,
    pub notify_on_completion: bool,
    pub trigger_on_completion: bool,
    pub survive_reload: bool,
}

#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
    pub published: crate::active_work::Published,
    pub monitor_started: AtomicBool,
    widget: Mutex<WidgetMode>,
    /// The binary of this extension, whose `--launch` starts tasks outside pi's process tree;
    /// `None` starts them in this process.
    launcher: Option<PathBuf>,
}

/// Whether `session_shutdown` with `reason` keeps the running tasks with `surviveReload`: on
/// `reload`, and on `quit` in `rpc` mode (pi-rust's ACP mode binds extensions in `rpc` mode too)
/// when the session has a file, so a client such as Paseo can start pi again on it. Any other
/// quit and every session switch (`new`, `resume`, `fork`) stops all running tasks.
pub fn keeps_survivors(reason: &str, mode: Option<&str>, resumable: bool) -> bool {
    reason == "reload" || (reason == "quit" && mode == Some("rpc") && resumable)
}

/// How the widget below the editor shows running tasks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WidgetMode {
    /// One row per task, or one summary line above `ui::COLLAPSE_AT` tasks.
    #[default]
    Auto,
    Collapsed,
    Expanded,
}

#[derive(Default)]
struct Inner {
    dir: Option<PathBuf>,
    tasks: Vec<Task>,
}

impl Registry {
    /// A registry whose tasks start through `launcher --launch`.
    pub fn with_launcher(launcher: PathBuf) -> Registry {
        Registry {
            launcher: Some(launcher),
            ..Registry::default()
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Binds the registry to a session directory once and loads the tasks stored there.
    pub fn bind(&self, dir: PathBuf) {
        let mut inner = self.lock();
        if inner.dir.is_none() {
            inner.tasks = task::load_all(&dir);
            inner.dir = Some(dir);
        }
    }

    pub fn widget_mode(&self) -> WidgetMode {
        *self
            .widget
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn set_widget_mode(&self, mode: WidgetMode) {
        *self
            .widget
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = mode;
    }

    pub fn is_bound(&self) -> bool {
        self.lock().dir.is_some()
    }

    pub fn tasks(&self) -> Vec<Task> {
        self.lock().tasks.clone()
    }

    pub fn running(&self) -> Vec<Task> {
        self.lock()
            .tasks
            .iter()
            .filter(|t| t.status.is_running())
            .cloned()
            .collect()
    }

    pub async fn start(&self, options: StartOptions, cwd: &str) -> Result<Task, String> {
        let base = self.lock().dir.clone().ok_or("no session directory yet")?;
        let id = task::new_id();
        let dir = base.join(&id);
        task::create_private_dir(&base)
            .and_then(|()| task::create_private_dir(&dir))
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let name = options
            .name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| default_name(&options.command));
        let mut task = Task {
            id,
            name,
            command: options.command,
            description: options.description,
            cwd: cwd.to_owned(),
            is_agent: options.is_agent,
            pid: None,
            process_start: None,
            dir,
            started_at: task::now_ms(),
            ended_at: None,
            timeout_seconds: options.timeout_seconds.filter(|s| *s > 0),
            notify_on_completion: options.notify_on_completion,
            trigger_on_completion: options.notify_on_completion && options.trigger_on_completion,
            survive_reload: options.survive_reload,
            status: Status::Running,
            exit_code: None,
            notified: false,
            notice_sent_at: None,
            error: None,
        };
        let detach = task.survive_reload;
        match self
            .spawn(&task.command, Path::new(cwd), &task.dir, detach)
            .await
        {
            Ok((pid, process_start)) => {
                task.pid = Some(pid);
                task.process_start = process_start;
            }
            Err(err) => {
                let _ = std::fs::remove_dir_all(&task.dir);
                return Err(format!("cannot start `{}`: {err}", task.command));
            }
        }
        task.save()
            .map_err(|e| format!("cannot save task state: {e}"))?;
        self.lock().tasks.push(task.clone());
        Ok(task)
    }

    /// Starts the wrapper of `command` through the launcher when `detach` (a task that survives
    /// reloads and pi restarts), else in this process, so a kill of pi's process tree ends it.
    /// Without a launcher, or when it did not start the task, it starts here. Returns the pid and
    /// its start time.
    async fn spawn(
        &self,
        command: &str,
        cwd: &Path,
        dir: &Path,
        detach: bool,
    ) -> Result<(u32, Option<String>), String> {
        if let Some(launcher) = self.launcher.as_ref().filter(|_| detach) {
            match launch::start(launcher, dir, cwd, command).await {
                Ok(launched) => {
                    let pid = launched.pid;
                    let start = launched
                        .process_start
                        .or_else(|| identity::identity(pid).ok().flatten());
                    return Ok((pid, start));
                }
                Err(launch::Failure::Unknown(err)) => return Err(err),
                Err(launch::Failure::NotStarted(err)) => pi_ext_sdk::warn!(
                    "the task launcher did not start `{command}`, so it runs in this process and ends with pi: {err}"
                ),
            }
        }
        let pid = process::spawn(command, cwd, &dir.join(OUTPUT_FILE), &dir.join(EXIT_FILE))
            .map_err(|err| err.to_string())?;
        Ok((pid, identity::identity(pid).ok().flatten()))
    }

    /// Finds a task by id or by an unambiguous id prefix.
    pub fn resolve(&self, query: &str) -> Result<Task, String> {
        resolve_in(&self.lock().tasks, query).cloned()
    }

    /// Applies `update` to the task `id` and saves it; returns the updated task.
    fn update(&self, id: &str, update: impl FnOnce(&mut Task) -> bool) -> Option<Task> {
        let mut inner = self.lock();
        let task = inner.tasks.iter_mut().find(|t| t.id == id)?;
        if !update(task) {
            return None;
        }
        if let Err(err) = task.save() {
            pi_ext_sdk::warn!("cannot save task {}: {err}", task.id);
        }
        Some(task.clone())
    }

    /// Stops a running task: TERM to its process group, KILL after a grace period.
    pub async fn stop(&self, query: &str, reason: &str) -> Result<Task, String> {
        let found = self.resolve(query)?;
        let id = found.id.clone();
        if found.status.is_running() && found.pid.is_some() && self.settle_unowned(&found) {
            let now = self.resolve(&id)?;
            let detail = now.error.map(|e| format!(": {e}")).unwrap_or_default();
            return Err(format!("task {id} is {}{detail}", now.status.label()));
        }
        let task = self
            .update(&id, |t| {
                let stopped = t.finish(Status::Killed, None);
                if stopped {
                    t.notified = true;
                    t.error = Some(reason.to_owned());
                }
                stopped
            })
            .ok_or_else(|| format!("task {id} is not running"))?;
        let Some(pid) = task.pid else {
            return Ok(task);
        };
        if let Err(err) = process::terminate(pid, task.process_start.as_deref(), KILL_GRACE).await {
            self.update(&id, |t| {
                t.error = Some(err.clone());
                true
            });
            return Err(err);
        }
        Ok(task)
    }

    /// Checks running tasks once and returns the finished tasks that still owe a notification.
    pub async fn poll(&self, check_alive: bool) -> Vec<Task> {
        let now = task::now_ms();
        for task in self.running() {
            if let Some(code) = task.read_exit_code() {
                let status = if code == 0 {
                    Status::Completed
                } else {
                    Status::Failed
                };
                self.update(&task.id, |t| t.finish(status, Some(code)));
            } else {
                let timed_out = task.timed_out(now);
                if !timed_out && !check_alive {
                    continue;
                }
                if !self.settle_unowned(&task) && timed_out {
                    self.time_out(&task);
                }
            }
        }
        self.lock()
            .tasks
            .iter()
            .filter(|t| !t.status.is_running() && t.notify_on_completion && !t.notified)
            .cloned()
            .collect()
    }

    fn time_out(&self, task: &Task) {
        let updated = self.update(&task.id, |t| {
            let done = t.finish(Status::TimedOut, None);
            t.error = Some(format!(
                "timed out after {}s",
                t.timeout_seconds.unwrap_or(0)
            ));
            done
        });
        if let (Some(_), Some(pid)) = (updated, task.pid) {
            let identity = task.process_start.clone();
            tokio::spawn(async move {
                if let Err(err) = process::terminate(pid, identity.as_deref(), KILL_GRACE).await {
                    pi_ext_sdk::warn!("{err}");
                }
            });
        }
    }

    fn mark_ended(&self, task: &Task) {
        let code = task.read_exit_code();
        self.update(&task.id, |t| match code {
            Some(0) => t.finish(Status::Completed, Some(0)),
            Some(c) => t.finish(Status::Failed, Some(c)),
            None => {
                t.error = Some("process ended without an exit status".into());
                t.finish(Status::Lost, None)
            }
        });
    }

    /// Records the end of a running task whose pid is gone or not its own; true when it did.
    fn settle_unowned(&self, task: &Task) -> bool {
        match owner_of(task) {
            Owner::Same => false,
            Owner::Gone => {
                self.mark_ended(task);
                true
            }
            Owner::Other => {
                self.mark_reused(task);
                true
            }
        }
    }

    fn mark_reused(&self, task: &Task) {
        let reason = process::unverified_message(task.pid.unwrap_or_default());
        self.update(&task.id, |t| {
            t.error = Some(reason);
            t.finish(Status::Lost, None)
        });
    }

    /// The session holds the task's notification, or none is owed any more.
    pub fn mark_notified(&self, id: &str) {
        self.update(id, |t| {
            t.notified = true;
            true
        });
    }

    /// Records when the task's notification was sent; `None` undoes a send that pi refused.
    pub fn set_notice_sent(&self, id: &str, at: Option<u64>) -> Option<Task> {
        self.update(id, |t| {
            t.notice_sent_at = at;
            true
        })
    }

    /// Removes finished tasks and their output; returns how many it removed.
    pub fn clear_finished(&self) -> usize {
        let mut inner = self.lock();
        let before = inner.tasks.len();
        inner.tasks.retain(|t| {
            let keep = t.status.is_running();
            if !keep {
                let _ = std::fs::remove_dir_all(&t.dir);
            }
            keep
        });
        before - inner.tasks.len()
    }

    /// `session_shutdown`: stops the running tasks, except those with `surviveReload` when
    /// `keep` ([`keeps_survivors`]). Returns how many it stopped.
    pub async fn shutdown(&self, reason: &str, keep: bool) -> usize {
        let mut victims = Vec::new();
        for task in self.running() {
            if keep && task.survive_reload {
                continue;
            }
            if task.pid.is_some() && self.settle_unowned(&task) {
                continue;
            }
            let stopped = self.update(&task.id, |t| {
                t.notified = true;
                t.error = Some(format!("stopped on session {reason}"));
                t.finish(Status::Killed, None)
            });
            victims.extend(stopped);
        }
        let mut stops = tokio::task::JoinSet::new();
        for task in &victims {
            if let Some(pid) = task.pid {
                let identity = task.process_start.clone();
                stops.spawn(async move {
                    process::terminate(pid, identity.as_deref(), SHUTDOWN_GRACE).await
                });
            }
        }
        while let Some(result) = stops.join_next().await {
            if let Ok(Err(err)) = result {
                pi_ext_sdk::warn!("{err}");
            }
        }
        victims.len()
    }

    pub fn monitor_once(&self) -> bool {
        !self.monitor_started.swap(true, Ordering::SeqCst)
    }
}

fn owner_of(task: &Task) -> Owner {
    match task.pid {
        Some(pid) => identity::owner(pid, task.process_start.as_deref()),
        None => Owner::Gone,
    }
}
