//! PTY ownership and bounded replay, independent of agent tool execution.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{Read, Write},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use holt_proto::{TerminalEvent, TerminalSession};
use holt_rpc::{RpcError, RpcReply, terminals::TerminalStatus};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::watch;

const REPLAY_BYTES: usize = 1024 * 1024;
const MAX_WRITE_BYTES: usize = 256 * 1024;

#[derive(Default)]
struct Registry {
    sessions: HashMap<String, Arc<Session>>,
    removed_chats: HashSet<String>,
    stopped: bool,
}

#[derive(Default)]
pub(crate) struct Terminals {
    registry: Mutex<Registry>,
}

struct Session {
    chat_id: String,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    jobs_cleaned: AtomicBool,
    replay: Mutex<Replay>,
    changed: watch::Sender<u64>,
}

#[derive(Default)]
struct Replay {
    seq: u64,
    bytes: usize,
    events: VecDeque<(TerminalEvent, usize)>,
    exited: bool,
}

fn event_seq(event: &TerminalEvent) -> u64 {
    match event {
        TerminalEvent::Data { seq, .. }
        | TerminalEvent::Exit { seq, .. }
        | TerminalEvent::Gap { seq } => *seq,
    }
}

impl Replay {
    fn push(&mut self, data: &[u8]) {
        self.seq += 1;
        self.bytes += data.len();
        self.events.push_back((
            TerminalEvent::Data {
                seq: self.seq,
                data: STANDARD.encode(data),
            },
            data.len(),
        ));
        while self.bytes > REPLAY_BYTES || self.events.len() > 4096 {
            if let Some((_, len)) = self.events.pop_front() {
                self.bytes -= len;
            }
        }
    }

    fn next(&self, after: u64) -> Option<TerminalEvent> {
        let first = self.events.front().map(|(event, _)| event_seq(event))?;
        if after.saturating_add(1) < first {
            return Some(TerminalEvent::Gap { seq: first - 1 });
        }
        self.events
            .iter()
            .find(|(e, _)| event_seq(e) > after)
            .map(|(e, _)| e.clone())
    }
}

fn size(cols: u16, rows: u16) -> Result<PtySize, RpcError> {
    if cols == 0 || rows == 0 || cols > 1000 || rows > 1000 {
        return Err(RpcError::BadParams(
            "terminal size must be between 1 and 1000 cells per axis".into(),
        ));
    }
    Ok(PtySize {
        cols,
        rows,
        pixel_width: 0,
        pixel_height: 0,
    })
}

fn failed(error: impl std::fmt::Display) -> RpcError {
    RpcError::Failed(error.to_string())
}

fn login_shell() -> String {
    #[cfg(unix)]
    {
        // Reentrant lookup: GUI launches need not inherit SHELL from a login shell.
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut storage = vec![0u8; 16384];
        let mut result = std::ptr::null_mut();
        unsafe {
            if libc::getpwuid_r(
                libc::getuid(),
                entry.as_mut_ptr(),
                storage.as_mut_ptr().cast(),
                storage.len(),
                &mut result,
            ) == 0
                && !result.is_null()
                && !(*result).pw_shell.is_null()
            {
                let shell = std::ffi::CStr::from_ptr((*result).pw_shell).to_string_lossy();
                if !shell.is_empty() {
                    return shell.into_owned();
                }
            }
        }
    }
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

impl Terminals {
    pub fn open(
        &self,
        chat_id: String,
        cwd: &str,
        cols: u16,
        rows: u16,
    ) -> Result<TerminalSession, RpcError> {
        let mut command = CommandBuilder::new(login_shell());
        command.arg("-l");
        command.arg("-i");
        command.cwd(cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "Holt");
        self.spawn(chat_id, cwd, cols, rows, command)
    }

    fn spawn(
        &self,
        chat_id: String,
        cwd: &str,
        cols: u16,
        rows: u16,
        command: CommandBuilder,
    ) -> Result<TerminalSession, RpcError> {
        let dimensions = size(cols, rows)?;
        if !Path::new(cwd).is_dir() {
            return Err(failed(format!("terminal directory does not exist: {cwd}")));
        }
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        if registry.stopped || registry.removed_chats.contains(&chat_id) {
            return Err(failed("terminal owner is closed"));
        }
        let shell = command.get_argv()[0].to_string_lossy().into_owned();
        let pair = native_pty_system().openpty(dimensions).map_err(failed)?;
        let reader = pair.master.try_clone_reader().map_err(failed)?;
        let writer = pair.master.take_writer().map_err(failed)?;
        let child = pair.slave.spawn_command(command).map_err(failed)?;
        drop(pair.slave);
        let (changed, _) = watch::channel(0);
        let session = Arc::new(Session {
            chat_id,
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            jobs_cleaned: AtomicBool::new(false),
            replay: Mutex::new(Replay::default()),
            changed,
        });
        let id = uuid::Uuid::new_v4().to_string();
        let weak = Arc::downgrade(&session);
        let spawned = std::thread::Builder::new()
            .name("holt-pty".into())
            .spawn(move || {
                let mut reader = reader;
                let mut buffer = [0; 16384];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(len) => {
                            let Some(session) = weak.upgrade() else {
                                return;
                            };
                            let mut replay =
                                session.replay.lock().unwrap_or_else(|e| e.into_inner());
                            if replay.exited {
                                return;
                            }
                            replay.push(&buffer[..len]);
                            session.changed.send_replace(replay.seq);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                if let Some(session) = weak.upgrade() {
                    // EOF follows the final output; publish Exit only after draining it.
                    let code = loop {
                        let status = {
                            let mut child = session.child.lock().unwrap_or_else(|e| e.into_inner());
                            let status = child.try_wait();
                            if matches!(status, Ok(Some(_))) {
                                session.cleanup_jobs(child.as_ref());
                            }
                            status
                        };
                        match status {
                            Ok(Some(status)) => break status.exit_code() as i32,
                            Err(_) => break -1,
                            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                        }
                    };
                    session.finish(code);
                }
            });
        if let Err(error) = spawned {
            session.close();
            return Err(failed(error));
        }
        let weak = Arc::downgrade(&session);
        if let Err(error) = std::thread::Builder::new()
            .name("holt-pty-exit".into())
            .spawn(move || {
                loop {
                    let Some(session) = weak.upgrade() else {
                        return;
                    };
                    let exited = {
                        let mut child = session.child.lock().unwrap_or_else(|e| e.into_inner());
                        match child.try_wait() {
                            Ok(Some(_)) => {
                                session.cleanup_jobs(child.as_ref());
                                true
                            }
                            Err(_) => true,
                            Ok(None) => false,
                        }
                    };
                    if exited {
                        return;
                    }
                    drop(session);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            })
        {
            session.close();
            return Err(failed(error));
        }
        registry.sessions.insert(id.clone(), session);
        Ok(TerminalSession {
            id,
            cwd: cwd.into(),
            shell,
        })
    }

    fn session(&self, id: &str) -> Result<Arc<Session>, RpcError> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| failed("terminal no longer exists"))
    }

    pub fn subscribe(&self, id: &str, after: u64) -> Result<RpcReply, RpcError> {
        let session = self.session(id)?;
        let receiver = session.changed.subscribe();
        Ok(RpcReply::Stream(Box::pin(futures::stream::unfold(
            (session, receiver, after),
            |(session, mut receiver, mut after)| async move {
                loop {
                    receiver.borrow_and_update();
                    let (next, exited) = {
                        let replay = session.replay.lock().unwrap_or_else(|e| e.into_inner());
                        (replay.next(after), replay.exited)
                    };
                    if let Some(event) = next {
                        after = event_seq(&event);
                        return Some((
                            serde_json::to_value(event).expect("terminal event serializes"),
                            (session, receiver, after),
                        ));
                    }
                    if exited || receiver.changed().await.is_err() {
                        return None;
                    }
                }
            },
        ))))
    }

    pub fn write(&self, id: &str, encoded: &str) -> Result<(), RpcError> {
        if encoded.len() > MAX_WRITE_BYTES.div_ceil(3) * 4 {
            return Err(RpcError::BadParams(
                "terminal input chunk exceeds 256 KiB".into(),
            ));
        }
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|e| RpcError::BadParams(e.to_string()))?;
        let session = self.session(id)?;
        if session
            .replay
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .exited
        {
            return Err(failed("terminal has exited"));
        }
        session
            .writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(&bytes)
            .map_err(failed)
    }

    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), RpcError> {
        self.session(id)?
            .master
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resize(size(cols, rows)?)
            .map_err(failed)
    }

    pub fn list(&self) -> Vec<TerminalStatus> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .iter()
            .map(|(id, session)| TerminalStatus {
                terminal_id: id.clone(),
                chat_id: session.chat_id.clone(),
                running: !session
                    .replay
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .exited,
            })
            .collect()
    }

    pub fn close(&self, id: &str) {
        let session = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .remove(id);
        if let Some(session) = session {
            session.close();
        }
    }

    pub fn close_chat(&self, chat: &str) {
        let sessions = {
            let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            registry.removed_chats.insert(chat.into());
            let ids: Vec<_> = registry
                .sessions
                .iter()
                .filter(|(_, s)| s.chat_id == chat)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| registry.sessions.remove(&id))
                .collect::<Vec<_>>()
        };
        for session in sessions {
            session.close();
        }
    }

    pub fn close_all(&self, shutdown: bool) {
        let sessions = {
            let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            registry.stopped |= shutdown;
            std::mem::take(&mut registry.sessions)
        };
        for (_, session) in sessions {
            session.close();
        }
    }
}

impl Session {
    // Both reader and exit watcher can observe shell exit. Clean jobs once,
    // under the child lock, before publishing Exit or allowing later closes.
    fn cleanup_jobs(&self, child: &(dyn Child + Send + Sync)) {
        if !self.jobs_cleaned.swap(true, Ordering::SeqCst) {
            #[cfg(unix)]
            if let Some(pid) = child.process_id() {
                kill_session_jobs(pid as i32);
            }
        }
    }

    fn finish(&self, code: i32) {
        let mut replay = self.replay.lock().unwrap_or_else(|e| e.into_inner());
        if replay.exited {
            return;
        }
        replay.exited = true;
        replay.seq += 1;
        let seq = replay.seq;
        replay.events.push_back((
            TerminalEvent::Exit {
                seq,
                exit_code: code,
                signal: None,
            },
            0,
        ));
        self.changed.send_replace(seq);
    }

    fn close(&self) {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        self.cleanup_jobs(child.as_ref());
        if child.try_wait().ok().flatten().is_none() {
            #[cfg(unix)]
            {
                let master = self.master.lock().unwrap_or_else(|e| e.into_inner());
                let foreground = master.process_group_leader();
                let shell = child.process_id().map(|pid| pid as i32);
                // Job-control programs have their own foreground group.
                // Kill that group as well as the shell's group before reaping.
                for group in [foreground, shell]
                    .into_iter()
                    .flatten()
                    .filter(|pid| *pid > 1)
                {
                    unsafe {
                        libc::kill(-group, libc::SIGKILL);
                    }
                }
            }
            let _ = child.kill();
        }
        let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
        self.finish(code);
    }
}

impl Drop for Terminals {
    fn drop(&mut self) {
        self.close_all(true);
    }
}

#[cfg(unix)]
fn kill_session_jobs(session: i32) {
    #[cfg(target_os = "macos")]
    let pids = unsafe {
        let count = libc::proc_listallpids(std::ptr::null_mut(), 0).max(0) as usize;
        let mut pids = vec![0i32; count + 1024];
        let read = libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<i32>()) as i32,
        );
        pids.truncate(read.max(0) as usize);
        pids
    };
    #[cfg(target_os = "linux")]
    let pids: Vec<i32> = std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_string_lossy().parse().ok())
        .collect();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let pids: Vec<i32> = Vec::new();
    for pid in pids {
        // Job-control background groups share the shell's session, but may
        // not be either its foreground group or its process group.
        if pid > 1 && pid != session && unsafe { libc::getsid(pid) } == session {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn shell(terminals: &Terminals, dir: &Path, script: &str) -> TerminalSession {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", script]);
        cmd.cwd(dir);
        terminals
            .spawn("chat".into(), dir.to_str().unwrap(), 80, 24, cmd)
            .unwrap()
    }

    async fn collect(reply: RpcReply) -> Vec<TerminalEvent> {
        let RpcReply::Stream(mut stream) = reply else {
            panic!("stream")
        };
        let mut events = Vec::new();
        while let Some(value) =
            tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
                .await
                .unwrap()
        {
            events.push(serde_json::from_value(value).unwrap());
        }
        events
    }

    #[tokio::test]
    async fn real_pty_input_resize_output_exit_and_replay() {
        let terminals = Terminals::default();
        let dir = tempfile::tempdir().unwrap();
        let session = shell(
            &terminals,
            dir.path(),
            "printf READY; read line; stty size; printf 'GOT:%s' \"$line\"; exit 7",
        );
        terminals.resize(&session.id, 93, 31).unwrap();
        terminals
            .write(&session.id, &STANDARD.encode(b"hello\n"))
            .unwrap();
        let events = collect(terminals.subscribe(&session.id, 0).unwrap()).await;
        let output: Vec<u8> = events
            .iter()
            .filter_map(|e| match e {
                TerminalEvent::Data { data, .. } => Some(STANDARD.decode(data).unwrap()),
                _ => None,
            })
            .flatten()
            .collect();
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("31 93"), "{text}");
        assert!(text.contains("GOT:hello"), "{text}");
        assert!(matches!(
            events.last(),
            Some(TerminalEvent::Exit { exit_code: 7, .. })
        ));
        let after = event_seq(&events[0]);
        let replayed = collect(terminals.subscribe(&session.id, after).unwrap()).await;
        assert_eq!(replayed, events[1..]);
        assert!(!terminals.list()[0].running);
        terminals.close(&session.id);
        assert!(terminals.list().is_empty());
    }

    #[tokio::test]
    async fn close_chat_ends_running_process_and_prevents_late_open() {
        let terminals = Terminals::default();
        let dir = tempfile::tempdir().unwrap();
        let session = shell(&terminals, dir.path(), "sleep 60");
        let reply = terminals.subscribe(&session.id, 0).unwrap();
        terminals.close_chat("chat");
        let events = collect(reply).await;
        assert!(matches!(events.last(), Some(TerminalEvent::Exit { .. })));
        assert!(terminals.list().is_empty());
        assert!(
            terminals
                .open("chat".into(), dir.path().to_str().unwrap(), 80, 24)
                .is_err()
        );
    }

    #[tokio::test]
    async fn terminal_exit_cleans_background_jobs_with_redirected_output() {
        let terminals = Terminals::default();
        let dir = tempfile::tempdir().unwrap();
        let session = shell(
            &terminals,
            dir.path(),
            "trap '' HUP; (sleep 1; touch survived) </dev/null >/dev/null 2>&1 & printf done; exit 0",
        );
        let events = collect(terminals.subscribe(&session.id, 0).unwrap()).await;
        assert!(matches!(
            events.last(),
            Some(TerminalEvent::Exit { exit_code: 0, .. })
        ));
        tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
        assert!(
            !dir.path().join("survived").exists(),
            "background job survived shell exit"
        );
    }

    #[test]
    fn replay_eviction_is_explicit_and_storage_is_bounded() {
        let mut replay = Replay::default();
        for _ in 0..200 {
            replay.push(&[b'x'; 16384]);
        }
        assert!(replay.bytes <= REPLAY_BYTES);
        let gap = replay.next(0).unwrap();
        assert!(matches!(gap, TerminalEvent::Gap { .. }));
        assert!(matches!(
            replay.next(event_seq(&gap)),
            Some(TerminalEvent::Data { .. })
        ));
    }

    #[test]
    fn bad_sizes_directories_and_input_are_rejected() {
        let terminals = Terminals::default();
        assert!(terminals.open("chat".into(), "/", 0, 24).is_err());
        assert!(
            terminals
                .open("chat".into(), "/holt-missing-terminal-test", 80, 24)
                .is_err()
        );
        assert!(terminals.write("missing", "@@").is_err());
        terminals.close("missing");
        terminals.close_all(true);
        assert!(terminals.open("chat".into(), "/", 80, 24).is_err());
    }
}
