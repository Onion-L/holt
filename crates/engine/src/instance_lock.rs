//! Single-instance lock — an exclusive advisory `flock` on `{data_dir}/engine.lock`
//! held for the engine's lifetime. The per-chat persistence locks are
//! in-process mutexes: they order one engine's appends, but two engines on
//! one data dir would race the append-only JSONL logs and the atomic
//! whole-file replaces with nothing cross-process to stop them, so the
//! second instance must fail fast with a clear error instead of corrupting
//! state.
//!
//! The lock is taken before any store opens, so a second process fails fast
//! instead of racing the first.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::EngineError;

/// Held lock on the data dir. Dropping it (engine shutdown / process exit)
/// releases the advisory lock; a crash releases it too (kernel-owned).
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Acquire the exclusive lock, non-blocking. Errors with a descriptive
    /// message (including the holder's pid when readable) if another engine
    /// already owns this data dir.
    pub fn acquire(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join("engine.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // Bounded EWOULDBLOCK retries: a fork→exec window in ANY process
            // that inherited the previous holder's fd (git scans, provider
            // spawns — fds are duplicated between fork and CLOEXEC-at-exec)
            // keeps the flock alive for a few milliseconds after release. A
            // real second engine holds it forever; transient artifacts clear
            // well within the budget.
            let mut retries = 40u32; // × 25ms = 1s budget
            loop {
                let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if rc == 0 {
                    break;
                }
                let errno = std::io::Error::last_os_error();
                match errno.raw_os_error() {
                    Some(libc::EINTR) => continue, // signal-interrupted: retry
                    Some(libc::EWOULDBLOCK) if retries > 0 => {
                        retries -= 1;
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    Some(libc::EWOULDBLOCK) => return Err(already_running(data_dir, &path)),
                    // Anything else (ENOLCK, filesystem without flock, …) is an
                    // environment problem, not a second engine — surface it as-is.
                    _ => return Err(EngineError::Io(errno)),
                }
            }
        }

        #[cfg(windows)]
        {
            // std's whole-file try_lock maps to LockFileEx — the same
            // kernel-owned, crash-released advisory lock flock gives unix.
            match file.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(already_running(data_dir, &path));
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(EngineError::Io(error)),
            }
        }

        // Best-effort pid stamp for the contention error message above.
        let _ = file.set_len(0);
        let _ = write!(file, "{}", std::process::id());
        let _ = file.flush();
        Ok(Self { _file: file })
    }
}

/// The contention error, with the holder's pid when the stamp is readable.
fn already_running(data_dir: &Path, path: &Path) -> EngineError {
    let holder = std::fs::read_to_string(path).unwrap_or_default();
    let holder = holder.trim();
    EngineError::Other(format!(
        "another holt engine is already running on {} (pid {}); \
         stop it or use a different data dir (HOLT_DATA_DIR)",
        data_dir.display(),
        if holder.is_empty() { "unknown" } else { holder },
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_while_held_then_succeeds_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        let lock = InstanceLock::acquire(dir.path()).expect("first acquire");
        let err = InstanceLock::acquire(dir.path()).expect_err("second acquire must fail");
        let msg = err.to_string();
        assert!(msg.contains("already running"), "unexpected error: {msg}");
        assert!(
            msg.contains(&std::process::id().to_string()),
            "holder pid missing from error: {msg}"
        );
        drop(lock);
        InstanceLock::acquire(dir.path()).expect("acquire after release");
    }
}
