//! The registry of live agent shell process groups (ADR-turns-resume-from-an-intent-journal).
//!
//! Every `bash` command runs in its own process group, killed when its
//! execution ends — but a hard kill of Holt runs no destructor, and the
//! group keeps running as an orphan, still changing the workspace a resumed
//! Turn is about to work in. Each group is recorded under
//! `<data dir>/shell-groups/` while it runs; launch kills whatever is still
//! recorded before any Turn resumes.

use std::path::{Path, PathBuf};

pub(crate) fn registry_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("shell-groups")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Record {
    pgid: i32,
    /// The group leader's start time, so a PID the OS reused after the
    /// orphan died is never signalled.
    started: Option<(i64, i64)>,
}

/// Record a just-spawned group; returns the record to forget once the group
/// is gone. Best-effort: a failed write only loses the orphan sweep.
pub(crate) fn record(dir: &Path, pgid: u32) -> Option<PathBuf> {
    let pgid = pgid as i32;
    let record = Record {
        pgid,
        started: start_time(pgid),
    };
    let path = dir.join(format!("{pgid}.json"));
    let written = std::fs::create_dir_all(dir).and_then(|()| {
        std::fs::write(
            &path,
            serde_json::to_vec(&record).map_err(std::io::Error::other)?,
        )
    });
    match written {
        Ok(()) => Some(path),
        Err(error) => {
            tracing::warn!(target: "holt::tools", %error, "could not record a shell process group");
            None
        }
    }
}

pub(crate) fn forget(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Kill every recorded group that outlived the process that started it,
/// then clear the registry. Runs at launch, before any Turn resumes.
pub(crate) fn reap(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(record) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
            && is_ours(&record)
        {
            // SAFETY: signalling a process group by id; `is_ours` checked
            // that the id still names the recorded group.
            if unsafe { libc::kill(-record.pgid, libc::SIGKILL) } == 0 {
                tracing::info!(target: "holt::tools", pgid = record.pgid, "killed an orphaned shell process group");
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Whether the recorded group still exists and is the one recorded. A PID
/// is never reused while a group with that id has members, so a group whose
/// leader is gone is still ours; a live leader must match its start time.
fn is_ours(record: &Record) -> bool {
    if record.pgid <= 1 {
        return false;
    }
    // SAFETY: signal 0 only probes for existence.
    if unsafe { libc::kill(-record.pgid, 0) } != 0 {
        return false;
    }
    match (start_time(record.pgid), record.started) {
        (Some(now), Some(then)) => now == then,
        _ => true,
    }
}

#[cfg(target_os = "macos")]
fn start_time(pid: i32) -> Option<(i64, i64)> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a writable buffer of exactly `size` bytes.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (read == size).then_some((info.pbi_start_tvsec as i64, info.pbi_start_tvusec as i64))
}

#[cfg(not(target_os = "macos"))]
fn start_time(_pid: i32) -> Option<(i64, i64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt as _;

    #[test]
    fn an_orphaned_group_is_killed_and_the_registry_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        let recorded = record(dir.path(), child.id()).unwrap();
        assert!(recorded.exists());
        reap(dir.path());
        let status = child.wait().unwrap();
        assert!(!status.success(), "the group was killed");
        assert!(!recorded.exists());
    }

    #[test]
    fn a_reused_leader_with_another_start_time_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as i32;
        let path = dir.path().join(format!("{pgid}.json"));
        std::fs::write(
            &path,
            serde_json::to_vec(&Record {
                pgid,
                started: Some((1, 2)),
            })
            .unwrap(),
        )
        .unwrap();
        reap(dir.path());
        assert!(child.try_wait().unwrap().is_none(), "still running");
        assert!(!path.exists());
        child.kill().unwrap();
        let _ = child.wait();
    }
}
