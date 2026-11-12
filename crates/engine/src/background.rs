//! The registry of agent-spawned background processes
//! (ADR-background-processes-are-chat-owned).
//!
//! Each process is one agent command (`spawn_background`) running detached
//! from its Turn: pipes (no PTY — it is never interactive), its own process
//! group, a bounded output log, and a lifetime the user owns from the
//! Processes panel. The group, not the shell leader, is the unit of
//! lifetime: `npm run dev &` leaves the server alive after the leader
//! exits, and the session stays running until the last group member dies.
//! While a group runs it is recorded under `shell-groups/`, so a hard kill
//! of Holt is swept at the next launch — the same orphan reaping the
//! foreground `bash` tool uses.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use holt_proto::TerminalEvent;
use holt_rpc::{RpcError, RpcReply, background::BackgroundProcessStatus};
use tokio::sync::watch;

use crate::output_log::OutputLog;

/// SIGTERM grace before the SIGKILL escalation — the same window the
/// foreground `bash` tool's timeout path uses.
const KILL_GRACE: std::time::Duration = std::time::Duration::from_millis(250);
/// How often the supervisor polls a leader-less group before it concludes
/// the process is done.
const GROUP_POLL: std::time::Duration = std::time::Duration::from_millis(200);
/// Exit notices held per chat for the next Turn's context; oldest drop past
/// this so a runaway spawner cannot grow the queue unbounded.
const MAX_NOTICES_PER_CHAT: usize = 20;

fn failed(message: impl Into<String>) -> RpcError {
    RpcError::Failed(message.into())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One spawned command: its identity, its bounded log, and its lifetime
/// state. Two pipe readers append concurrently; the supervisor finishes it
/// exactly once, when the whole group is gone.
struct Process {
    id: String,
    chat_id: String,
    command: String,
    cwd: String,
    pid: Option<u32>,
    started_at: u64,
    state: Mutex<ProcessState>,
    /// Wakes log subscribers (`SubscribeBackgroundProcess`).
    changed: watch::Sender<u64>,
}

struct ProcessState {
    log: OutputLog,
    exited: bool,
    /// Set when this exit has been claimed, before `exited` is visible.
    /// Kill racing the supervisor must not queue a second notice.
    exit_claimed: bool,
    exit_code: Option<i32>,
    exited_at: Option<u64>,
    /// Unix: the process-group id (the leader's pid — spawned with
    /// `process_group(0)`); `None` once nothing is left to signal.
    /// Windows: the Job Object whose close kills the tree.
    #[cfg(unix)]
    group: Option<u32>,
    #[cfg(windows)]
    job: Option<crate::tools::ShellJob>,
    /// The `shell-groups/` record path, forgotten when the group dies.
    record: Option<PathBuf>,
}

impl Process {
    fn running(&self) -> bool {
        !self.state.lock().unwrap_or_else(|e| e.into_inner()).exited
    }

    fn status(&self) -> BackgroundProcessStatus {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        BackgroundProcessStatus {
            process_id: self.id.clone(),
            chat_id: self.chat_id.clone(),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            pid: self.pid,
            running: !state.exited,
            started_at: self.started_at,
            exit_code: state.exit_code,
            exited_at: state.exited_at,
            kind: Default::default(),
            agents: None,
            tokens: None,
        }
    }

    /// The last `max_lines` lines (at most `max_bytes`) of combined output —
    /// what `process_status` and the exit notices report.
    fn tail(&self, max_lines: usize, max_bytes: usize) -> String {
        let chunks: Vec<String> = {
            use base64::Engine as _;
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // Walk events newest-first until the budgets are covered.
            let mut chunks: Vec<String> = Vec::new();
            let mut bytes = 0usize;
            let mut lines = 0usize;
            for event in state.log.events_iter().rev() {
                let TerminalEvent::Data { data, .. } = event else {
                    continue;
                };
                let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(data) else {
                    continue;
                };
                let chunk = String::from_utf8_lossy(&raw).into_owned();
                bytes += chunk.len();
                lines += chunk.matches('\n').count();
                chunks.push(chunk);
                if lines >= max_lines || bytes >= max_bytes {
                    break;
                }
            }
            chunks.reverse();
            chunks
        };
        let joined: String = chunks.concat();
        let lines: Vec<&str> = joined.lines().collect();
        let tail = if lines.len() > max_lines {
            lines[lines.len() - max_lines..].join("\n")
        } else {
            joined
        };
        tail.chars().take(max_bytes).collect()
    }

    /// Post `signal` to the whole group. Returns whether the group was
    /// signalled (false: already gone, or nothing to signal). Signal 0 is
    /// the existence probe the supervisor polls with.
    #[cfg(unix)]
    fn signal(&self, signal: libc::c_int) -> bool {
        let group = self.state.lock().unwrap_or_else(|e| e.into_inner()).group;
        let Some(group) = group else {
            return false;
        };
        // SAFETY: the child was spawned with process_group(0), so its pid
        // identifies only this process's group; `kill` only posts a signal.
        if unsafe { libc::kill(-(group as libc::pid_t), signal) } == 0 {
            return true;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            tracing::warn!(pid = group, %error, "could not signal background process group");
        }
        false
    }

    /// Close the Job Object handle — its KILL_ON_JOB_CLOSE limit takes the
    /// whole tree with it.
    #[cfg(windows)]
    fn close_job(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .job
            .take();
    }

    /// Claim this exit once, before it is visible. A second observation
    /// (kill racing the supervisor) returns false and must not queue
    /// another notice.
    fn claim_exit(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.exited || state.exit_claimed {
            return false;
        }
        state.exit_claimed = true;
        true
    }

    /// Make the exit visible: the log's `Exit` event, `running: false`, and
    /// the shell-groups record to forget. The caller queues the notice,
    /// tail included, before this — a Turn that sees the exit must not be
    /// able to drain an empty placeholder.
    fn publish_exit(&self, code: i32) -> Option<PathBuf> {
        let seq = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.exited {
                return None;
            }
            state.exited = true;
            state.exit_code = Some(code);
            state.exited_at = Some(now_ms());
            #[cfg(unix)]
            {
                state.group = None;
            }
            #[cfg(windows)]
            {
                state.job = None;
            }
            state.log.push_exit(code)
        };
        self.changed.send_replace(seq);
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record
            .take()
    }
}

#[derive(Clone)]
pub(crate) struct ExitNotice {
    pub(crate) command: String,
    pub(crate) exit_code: i32,
    pub(crate) tail: String,
}

#[derive(Default)]
struct Registry {
    processes: HashMap<String, Arc<Process>>,
    removed_chats: HashSet<String>,
    stopped: bool,
    notices: HashMap<String, VecDeque<ExitNotice>>,
}

/// The engine-owned background process registry. Spawned from the
/// `spawn_background` tool, listed and watched over the RPC surface, and
/// torn down with the engine.
#[derive(Default)]
pub struct BackgroundProcesses {
    registry: Mutex<Registry>,
    /// Bumped on every structural change (spawn, exit, removal); list
    /// watchers re-snapshot on change.
    version: watch::Sender<u64>,
}

/// Pump one output pipe into the process's log until EOF (the pipe closes
/// when the last group member holding it dies) or the process is finished
/// and nobody is listening anymore.
fn spawn_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    pipe: Option<R>,
    weak: std::sync::Weak<Process>,
) -> Option<tokio::task::JoinHandle<()>> {
    use tokio::io::AsyncReadExt as _;
    let mut pipe = pipe?;
    Some(tokio::spawn(async move {
        let mut buffer = [0; 16384];
        loop {
            let len = match pipe.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(len) => len,
            };
            let Some(process) = weak.upgrade() else {
                return;
            };
            let seq = {
                let mut state = process.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.exited {
                    return;
                }
                state.log.push(&buffer[..len]);
                state.log.seq()
            };
            process.changed.send_replace(seq);
        }
    }))
}

impl BackgroundProcesses {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Spawn one detached command. `shell` is the resolved agent shell (the
    /// same resolution the `bash` tool uses); `shell_groups` is the
    /// crash-reap registry directory, recorded while the group runs.
    pub fn spawn(
        self: &Arc<Self>,
        chat_id: String,
        command: String,
        cwd: &str,
        shell: &str,
        shell_groups: Option<PathBuf>,
    ) -> Result<BackgroundProcessStatus, RpcError> {
        let command = command.trim().to_string();
        if command.is_empty() {
            return Err(RpcError::BadParams("command is required".into()));
        }
        {
            let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            if registry.stopped {
                return Err(failed("background processes are stopped"));
            }
            if registry.removed_chats.contains(&chat_id) {
                return Err(failed("background process owner is closed"));
            }
        }
        // Command parity with the foreground `bash` tool (`Shell::exec`):
        // the same login shell, the same flags, the same process group — a
        // background command must not behave differently from a foreground
        // one beyond the wait.
        let mut cmd = tokio::process::Command::new(shell);
        cmd.args(crate::shell_env::command_args(shell))
            .arg(&command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(windows)]
        cmd.creation_flags(crate::shell_env::CREATE_NO_WINDOW);
        let child = cmd
            .spawn()
            .map_err(|error| failed(format!("could not spawn {command:?}: {error}")))?;
        let pid = child.id();
        // Unix only: the crash-reap registry (Windows Job Objects cover the
        // hard-kill case at the OS level).
        #[cfg(unix)]
        let record = shell_groups
            .as_deref()
            .zip(pid)
            .and_then(|(dir, id)| crate::shell_groups::record(dir, id));
        #[cfg(not(unix))]
        let record = None;
        #[cfg(windows)]
        let job = crate::tools::ShellJob::assign(&child);
        let (changed, _) = watch::channel(0);
        let process = Arc::new(Process {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id,
            command,
            cwd: cwd.to_string(),
            pid,
            started_at: now_ms(),
            state: Mutex::new(ProcessState {
                log: OutputLog::default(),
                exited: false,
                exit_claimed: false,
                exit_code: None,
                exited_at: None,
                #[cfg(unix)]
                group: pid,
                #[cfg(windows)]
                job: Some(job),
                record,
            }),
            changed,
        });
        let status = process.status();
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .processes
            .insert(process.id.clone(), process.clone());
        self.bump();
        self.supervise(process, child);
        Ok(status)
    }

    /// Own the process to its end: pump both pipes into the log, wait the
    /// leader, then wait the group empty — a leader-less group (`cmd &`)
    /// is still running, and its members still hold the pipes open.
    fn supervise(self: &Arc<Self>, process: Arc<Process>, mut child: tokio::process::Child) {
        let weak = Arc::downgrade(&process);
        let stdout = spawn_reader(child.stdout.take(), weak.clone());
        let stderr = spawn_reader(child.stderr.take(), weak);
        let registry = Arc::downgrade(self);
        tokio::spawn(async move {
            let code = match child.wait().await {
                Ok(status) => status.code().unwrap_or(-1),
                Err(_) => -1,
            };
            // The leader is gone; the group may not be. Poll until the last
            // member exits — only then is the session finished.
            loop {
                #[cfg(unix)]
                if !process.signal(0) {
                    break;
                }
                #[cfg(windows)]
                {
                    let active = process
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .job
                        .as_ref()
                        .is_some_and(|job| job.active_processes() > 0);
                    if !active {
                        break;
                    }
                }
                tokio::time::sleep(GROUP_POLL).await;
            }
            // Both pipes must reach EOF before the exit event and notice
            // snapshot the log; otherwise the final lines are lost from tail.
            for reader in [stdout, stderr].into_iter().flatten() {
                let _ = reader.await;
            }
            match registry.upgrade() {
                Some(registry) => registry.record_exit(&process, code),
                // The engine is gone; the exit event still reaches any
                // outstanding subscriber holding the process.
                None => {
                    let _ = process.publish_exit(code);
                }
            }
        });
    }

    /// The supervisor's finish path: queue the chat's exit notice, publish
    /// the exit, forget the shell-groups record, and wake watchers. The
    /// notice, tail included, is queued BEFORE `running` flips. A Turn
    /// that observes the exit then drains the lines. The previous order
    /// queued an empty placeholder, flipped `running` inside `finish`, and
    /// filled the tail afterwards — `take_exit_notices` could drain the
    /// placeholder and the real tail was dropped on the empty queue
    /// (Ubuntu CI, `output_exit_stream_and_notice_round_trip`).
    fn record_exit(self: &Arc<Self>, process: &Arc<Process>, code: i32) {
        if !process.claim_exit() {
            return;
        }
        let notice = ExitNotice {
            command: process.command.clone(),
            exit_code: code,
            tail: process.tail(8, 4096),
        };
        {
            let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            if !registry.removed_chats.contains(&process.chat_id) {
                let notices = registry.notices.entry(process.chat_id.clone()).or_default();
                notices.push_back(notice);
                while notices.len() > MAX_NOTICES_PER_CHAT {
                    notices.pop_front();
                }
            }
        }
        if let Some(record) = process.publish_exit(code) {
            #[cfg(unix)]
            crate::shell_groups::forget(&record);
        }
        self.bump();
    }

    /// Drain the chat's exit notices — the next Turn's context block. Only
    /// the main-chat Turn start calls this: a notice is delivered once.
    pub fn take_exit_notices(&self, chat_id: &str) -> Vec<ExitNotice> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .notices
            .remove(chat_id)
            .unwrap_or_default()
            .into_iter()
            .collect()
    }

    /// TERM → grace → KILL the process's group. The supervisor observes the
    /// exit and finishes the session; the entry stays listed with its exit
    /// code until the user clears it.
    pub fn kill(&self, id: &str) -> Result<(), RpcError> {
        let process = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .processes
            .get(id)
            .cloned()
            .ok_or_else(|| failed("background process no longer exists"))?;
        if !process.running() {
            return Err(failed("background process has already exited"));
        }
        #[cfg(unix)]
        {
            let process = process.clone();
            tokio::spawn(async move {
                if process.signal(libc::SIGTERM) {
                    tokio::time::sleep(KILL_GRACE).await;
                    if process.running() {
                        process.signal(libc::SIGKILL);
                    }
                } else {
                    process.signal(libc::SIGKILL);
                }
            });
        }
        #[cfg(windows)]
        process.close_job();
        Ok(())
    }

    /// Remove one exited entry from the list (its output goes with it).
    pub fn remove(&self, id: &str) -> Result<(), RpcError> {
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let running = registry.processes.get(id).is_some_and(|p| p.running());
        if running {
            return Err(failed(
                "background process is still running — kill it first",
            ));
        }
        if registry.processes.remove(id).is_some() {
            drop(registry);
            self.bump();
        }
        Ok(())
    }

    /// Remove every exited entry of one chat.
    pub fn clear_exited(&self, chat_id: &str) -> usize {
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let before = registry.processes.len();
        registry
            .processes
            .retain(|_, p| p.chat_id != chat_id || p.running());
        let removed = before - registry.processes.len();
        drop(registry);
        if removed > 0 {
            self.bump();
        }
        removed
    }

    /// End a chat: kill and forget its processes, drop its notices, and
    /// refuse any later spawn for it (a delete-then-spawn race cannot
    /// resurrect a process into a deleted chat).
    pub fn close_chat(&self, chat_id: &str) {
        let processes = {
            let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            registry.removed_chats.insert(chat_id.into());
            registry.notices.remove(chat_id);
            let ids: Vec<_> = registry
                .processes
                .iter()
                .filter(|(_, p)| p.chat_id == chat_id)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| registry.processes.remove(&id))
                .collect::<Vec<_>>()
        };
        for process in processes {
            self.terminate(&process);
        }
        self.bump();
    }

    /// End everything. `shutdown` additionally stops the registry (engine
    /// drop): later spawns refuse.
    pub fn close_all(&self, shutdown: bool) {
        let processes = {
            let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            registry.stopped |= shutdown;
            registry.notices.clear();
            std::mem::take(&mut registry.processes)
        };
        for (_, process) in processes {
            self.terminate(&process);
        }
        self.bump();
    }

    /// Kill one process now, bypassing the registry list — the chat-close
    /// and engine-drop paths, where the entry is already gone but the
    /// group must still die.
    fn terminate(&self, process: &Arc<Process>) {
        #[cfg(unix)]
        {
            process.signal(libc::SIGTERM);
            process.signal(libc::SIGKILL);
            let record = process
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record
                .take();
            if let Some(record) = record {
                crate::shell_groups::forget(&record);
            }
        }
        #[cfg(not(unix))]
        let _ = process;
        #[cfg(windows)]
        process.close_job();
    }

    /// Every process, running and exited-retained, oldest first.
    pub fn list(&self) -> Vec<BackgroundProcessStatus> {
        let mut statuses: Vec<_> = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .processes
            .values()
            .map(|p| p.status())
            .collect();
        statuses.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.process_id.cmp(&b.process_id))
        });
        statuses
    }

    /// The last lines of one process's output — the `process_status`
    /// detail read. `None` when the id is unknown.
    pub fn tail(&self, id: &str, max_lines: usize, max_bytes: usize) -> Option<String> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .processes
            .get(id)
            .map(|process| process.tail(max_lines, max_bytes))
    }

    pub fn subscribe_list(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    /// The log stream from `after`: replay, then follow live until the
    /// process's exit event.
    pub fn subscribe_output(&self, id: &str, after: u64) -> Result<RpcReply, RpcError> {
        let process = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .processes
            .get(id)
            .cloned()
            .ok_or_else(|| failed("background process no longer exists"))?;
        let receiver = process.changed.subscribe();
        let stream = futures::stream::unfold(
            (process, receiver, after),
            |(process, mut receiver, mut after)| async move {
                loop {
                    receiver.borrow_and_update();
                    let (next, exited) = {
                        let state = process.state.lock().unwrap_or_else(|e| e.into_inner());
                        (state.log.next(after), state.exited)
                    };
                    if let Some(event) = next {
                        after = match &event {
                            TerminalEvent::Data { seq, .. }
                            | TerminalEvent::Exit { seq, .. }
                            | TerminalEvent::Gap { seq } => *seq,
                        };
                        return Some((
                            serde_json::to_value(event)
                                .expect("background process event serializes"),
                            (process, receiver, after),
                        ));
                    }
                    if exited || receiver.changed().await.is_err() {
                        return None;
                    }
                }
            },
        );
        Ok(RpcReply::Stream(Box::pin(stream)))
    }

    fn bump(&self) {
        self.version.send_modify(|v| *v = v.wrapping_add(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::time::{Duration, Instant};

    fn spawn(
        background: &Arc<BackgroundProcesses>,
        command: &str,
    ) -> Result<BackgroundProcessStatus, RpcError> {
        background.spawn("chat".into(), command.into(), "/tmp", "/bin/sh", None)
    }

    async fn collect(reply: RpcReply) -> Vec<TerminalEvent> {
        let RpcReply::Stream(mut stream) = reply else {
            panic!("stream")
        };
        let mut events = Vec::new();
        while let Some(value) = tokio::time::timeout(Duration::from_secs(15), stream.next())
            .await
            .unwrap()
        {
            events.push(serde_json::from_value(value).unwrap());
        }
        events
    }

    /// Poll the listed statuses until `expected` holds for the row, or the
    /// deadline passes — the observable is reached, never assumed at a tick
    /// (the 2026-10-23 RETROS rule, applied to processes).
    fn poll_status(
        background: &Arc<BackgroundProcesses>,
        id: &str,
        expected: impl Fn(&BackgroundProcessStatus) -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = background
                .list()
                .into_iter()
                .find(|status| status.process_id == id)
                && expected(&status)
            {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn an_empty_command_is_refused() {
        let background = BackgroundProcesses::new();
        assert!(spawn(&background, "   ").is_err());
        assert!(background.subscribe_output("missing", 0).is_err());
        assert!(background.kill("missing").is_err());
    }

    // Real-process tests drive /bin/sh — Windows would need cmd spellings
    // and the process-group semantics have no counterpart there.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn output_exit_stream_and_notice_round_trip() {
        use base64::Engine as _;
        let background = BackgroundProcesses::new();
        let status = spawn(&background, "printf 'line-a\nline-b\n'; exit 3").unwrap();
        let events = collect(background.subscribe_output(&status.process_id, 0).unwrap()).await;
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                TerminalEvent::Data { data, .. } => base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .ok()
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
                _ => None,
            })
            .collect();
        assert!(text.contains("line-a") && text.contains("line-b"), "{text}");
        assert!(matches!(
            events.last(),
            Some(TerminalEvent::Exit { exit_code: 3, .. })
        ));
        // The exited entry stays listed with its code until cleared.
        assert!(poll_status(&background, &status.process_id, |s| {
            !s.running && s.exit_code == Some(3)
        }));
        // The notice carries the command, the code, and the tail.
        let notices = background.take_exit_notices("chat");
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].exit_code, 3);
        assert!(notices[0].command.contains("printf"));
        assert!(notices[0].tail.contains("line-b"), "{}", notices[0].tail);
        assert!(background.take_exit_notices("chat").is_empty());
        // Remove accepts the exited entry; clearing sweeps it.
        background.remove(&status.process_id).unwrap();
        assert!(background.list().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_terminates_a_running_group_but_keeps_it_listed() {
        let background = BackgroundProcesses::new();
        let status = spawn(&background, "sleep 60; true").unwrap();
        assert!(poll_status(&background, &status.process_id, |s| s.running));
        background.kill(&status.process_id).unwrap();
        assert!(poll_status(&background, &status.process_id, |s| !s.running));
        // Killed but still listed — the user clears it.
        assert!(
            background
                .list()
                .iter()
                .any(|s| s.process_id == status.process_id)
        );
        // A running process cannot be removed by hand.
        let second = spawn(&background, "sleep 60; true").unwrap();
        assert!(poll_status(&background, &second.process_id, |s| s.running));
        assert!(background.remove(&second.process_id).is_err());
        // close_all ends and de-lists everything at once.
        background.close_all(false);
        assert!(background.list().is_empty());
        // A stopped registry (engine drop) refuses new spawns.
        background.close_all(true);
        assert!(spawn(&background, "true").is_err());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_leader_less_group_stays_running_until_its_last_member_dies() {
        let background = BackgroundProcesses::new();
        // The shell exits immediately; the spawned sleeper owns the group
        // — the session is still running (and still capturing output).
        let status = spawn(&background, "sleep 5 & printf gone; exit 0").unwrap();
        assert!(poll_status(&background, &status.process_id, |s| s.running));
        background.kill(&status.process_id).unwrap();
        assert!(poll_status(&background, &status.process_id, |s| !s.running));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_chat_kills_and_prevents_a_late_spawn() {
        let background = BackgroundProcesses::new();
        let status = spawn(&background, "sleep 60; true").unwrap();
        assert!(poll_status(&background, &status.process_id, |s| s.running));
        background.close_chat("chat");
        assert!(background.list().is_empty());
        assert!(
            poll_status(&background, &status.process_id, |s| !s.running)
                || background
                    .list()
                    .iter()
                    .all(|s| s.process_id != status.process_id)
        );
        assert!(spawn(&background, "true").is_err());
        // A closed chat's exits never queue notices.
        assert!(background.take_exit_notices("chat").is_empty());
    }
}
