//! The integration checks executor: one run of the trunk's declared
//! `.holt/checks.toml` against a worktree's in-memory squash merge with the
//! trunk, checked out into a throwaway worktree. Deliberately NOT part of
//! `BackgroundProcesses` — every entry there belongs to a chat and queues
//! exit notices; a checks run belongs to a repository and reports through
//! `WatchChecks`. The spawn, process-group, log, and kill plumbing IS
//! shared with it (`background::spawn_group_process` and friends).
//!
//! Results inform the merge bar; nothing here ever gates a merge.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use holt_rpc::checks::{CheckRow, CheckRun};
use tokio::sync::watch;

use crate::git::Git;

/// One declared check from the trunk's `.holt/checks.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckSpec {
    pub(crate) name: String,
    pub(crate) run: String,
    pub(crate) timeout: Duration,
}

const DEFAULT_TIMEOUT_SECS: u64 = 1800;

/// Parse the trunk's `.holt/checks.toml`. Unknown fields are errors (a
/// misspelled key must not silently do nothing); duplicates likewise.
pub(crate) fn parse_checks_config(bytes: &[u8]) -> Result<Vec<CheckSpec>, String> {
    let parsed: toml::Value = toml::from_str(&String::from_utf8_lossy(bytes))
        .map_err(|error| format!(".holt/checks.toml is not valid TOML: {error}"))?;
    let tables = parsed
        .get("check")
        .and_then(|value| value.as_array())
        .ok_or_else(|| ".holt/checks.toml declares no [[check]]".to_string())?;
    let root_keys: Vec<&str> = parsed
        .as_table()
        .map(|table| table.keys().map(String::as_str).collect())
        .unwrap_or_default();
    if let Some(unknown) = root_keys.iter().find(|key| **key != "check") {
        return Err(format!(
            ".holt/checks.toml: unknown field {unknown:?} (only [[check]] is allowed)"
        ));
    }
    let mut specs = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for table in tables {
        let Some(table) = table.as_table() else {
            return Err(".holt/checks.toml: every [[check]] must be a table".to_string());
        };
        if let Some(unknown) = table
            .keys()
            .find(|key| !matches!(key.as_str(), "name" | "run" | "timeout_secs"))
        {
            return Err(format!(
                ".holt/checks.toml: unknown check field {unknown:?} (name, run, timeout_secs)"
            ));
        }
        let name = table
            .get("name")
            .and_then(|value| value.as_str())
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| ".holt/checks.toml: every check needs a name".to_string())?;
        if seen.contains(&name) {
            return Err(format!(".holt/checks.toml: duplicate check name {name:?}"));
        }
        seen.push(name);
        let run = table
            .get("run")
            .and_then(|value| value.as_str())
            .filter(|run| !run.trim().is_empty())
            .ok_or_else(|| format!(".holt/checks.toml: check {name:?} needs a run command"))?;
        let timeout = table
            .get("timeout_secs")
            .map(|value| {
                value
                    .as_integer()
                    .filter(|secs| *secs > 0)
                    .map(|secs| Duration::from_secs(secs as u64))
                    .ok_or_else(|| {
                        format!(".holt/checks.toml: check {name:?} timeout_secs must be a positive integer")
                    })
            })
            .transpose()?
            .unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        specs.push(CheckSpec {
            name: name.to_string(),
            run: run.to_string(),
            timeout,
        });
    }
    Ok(specs)
}

/// One live run: its cancellation and the process of the check currently
/// executing (for StopChecks).
struct RunHandle {
    run_id: String,
    stop: tokio_util::sync::CancellationToken,
    /// The running check's name and process. The name rides along so a
    /// log subscription for one check can never stream another's output.
    current: Mutex<Option<(String, Arc<crate::background::Process>)>>,
}

/// The per-(checkout, worktree) latest result, published to watchers.
struct WatchSlot {
    tx: watch::Sender<Option<CheckRun>>,
}

/// The engine-owned checks service: one live run per (repo, worktree) at a
/// time, the latest result persisted per (checkout key, worktree).
#[derive(Default)]
pub struct IntegrationChecks {
    runs: Mutex<HashMap<(String, String), Arc<RunHandle>>>,
    by_run_id: Mutex<HashMap<String, (String, String)>>,
    slots: Mutex<HashMap<(String, String), Arc<WatchSlot>>>,
}

impl IntegrationChecks {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Drop leftover checkouts from a previous engine life (a hard kill
    /// leaves them behind; their runs do not resume).
    pub fn clean_checkouts(data_dir: &Path) {
        let dir = data_dir.join("integration/checkouts");
        if dir.is_dir()
            && let Err(error) = std::fs::remove_dir_all(&dir)
        {
            tracing::warn!(dir = %dir.display(), %error, "could not clean integration checkouts");
        }
    }

    fn slot(&self, key: (String, String)) -> Arc<WatchSlot> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots
            .entry(key)
            .or_insert_with(|| {
                let (tx, _) = watch::channel(None);
                Arc::new(WatchSlot { tx })
            })
            .clone()
    }

    /// The latest run for a (checkout, worktree), read from the persisted
    /// record when no slot has a live one.
    pub fn watch(
        &self,
        data_dir: &Path,
        checkout_key: &str,
        worktree: &str,
    ) -> watch::Receiver<Option<CheckRun>> {
        let slot = self.slot((checkout_key.to_string(), worktree.to_string()));
        if slot.tx.borrow().is_none() {
            let persisted = load_run(data_dir, checkout_key, worktree);
            // Only seed when still empty: a concurrent run may have
            // published in between.
            slot.tx.send_if_modified(|value| {
                if value.is_none() {
                    *value = persisted;
                    true
                } else {
                    false
                }
            });
        }
        slot.tx.subscribe()
    }

    /// The currently executing check's process, for `SubscribeCheckLog`.
    /// Live only, and only for the check actually running: a named check
    /// that is not live reports `None` (the persisted `logTail` is what
    /// remains for finished checks).
    pub fn live_check_process(
        &self,
        run_id: &str,
        check_name: &str,
    ) -> Option<Arc<crate::background::Process>> {
        let key = self
            .by_run_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run_id)
            .cloned()?;
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .and_then(|handle| {
                let current = handle.current.lock().unwrap_or_else(|e| e.into_inner());
                let (name, process) = current.as_ref()?;
                (name == check_name).then(|| process.clone())
            })
    }

    /// Stop one run by id (StopChecks). Unknown ids, finished ids, and ids
    /// a re-run superseded are no-ops — the id must still name the live run
    /// under its key.
    pub fn stop(&self, run_id: &str) {
        let key = self
            .by_run_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run_id)
            .cloned();
        let Some(key) = key else {
            return;
        };
        if let Some(handle) = self
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
            && handle.run_id == run_id
        {
            handle.stop.cancel();
            if let Some(process) = handle
                .current
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|(_, process)| process.clone())
            {
                process.kill_with_grace();
            }
        }
    }

    /// Start a run: compute the merge, check it out, run the checks
    /// serially, clean up, and persist. A previous run for the same
    /// (repo, worktree) is stopped first. The reply is the run id — the
    /// outcome streams through `WatchChecks`.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        self: &Arc<Self>,
        git: Git,
        data_dir: PathBuf,
        shell: String,
        shell_groups: Option<PathBuf>,
        repo_path: String,
        worktree: String,
        trunk: String,
    ) -> Result<String, String> {
        let checkout_key = git.checkout_key(&repo_path).await?;
        let key = (checkout_key.clone(), worktree.clone());

        // Read the config from the trunk BEFORE stopping anything — an
        // invalid or missing config fails the call without touching the
        // previous run.
        let config = git
            .checks_config_blob(&repo_path, trunk.clone())
            .await?
            .ok_or_else(|| "the trunk has no .holt/checks.toml".to_string())?;
        let specs = parse_checks_config(&config)?;

        // One live run per (repo, worktree): stop the old one, take its
        // remembered id with it, and seat the replacement — one critical
        // section, so two racing RunChecks cannot both miss the previous.
        let run_id = uuid::Uuid::new_v4().to_string();
        let handle = Arc::new(RunHandle {
            run_id: run_id.clone(),
            stop: tokio_util::sync::CancellationToken::new(),
            current: Mutex::new(None),
        });
        {
            let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
            let previous = runs.insert(key.clone(), handle.clone());
            if let Some(previous) = previous {
                previous.stop.cancel();
                if let Some(process) = previous
                    .current
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|(_, process)| process.clone())
                {
                    process.kill_with_grace();
                }
                let mut by_id = self.by_run_id.lock().unwrap_or_else(|e| e.into_inner());
                by_id.remove(&previous.run_id);
                by_id.insert(run_id.clone(), key.clone());
            } else {
                let mut by_id = self.by_run_id.lock().unwrap_or_else(|e| e.into_inner());
                by_id.insert(run_id.clone(), key.clone());
            }
        }
        let slot = self.slot(key.clone());

        let executor = self.clone();
        let slot_tx = slot.tx.clone();
        let worktree_name = worktree.clone();
        let handle_for_task = handle.clone();
        tokio::spawn(async move {
            executor
                .execute(
                    git,
                    data_dir,
                    shell,
                    shell_groups,
                    repo_path,
                    worktree_name,
                    trunk,
                    checkout_key,
                    specs,
                    handle_for_task,
                    slot_tx,
                )
                .await;
        });
        Ok(run_id)
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        git: Git,
        data_dir: PathBuf,
        shell: String,
        shell_groups: Option<PathBuf>,
        repo_path: String,
        worktree: String,
        trunk: String,
        checkout_key: String,
        specs: Vec<CheckSpec>,
        handle: Arc<RunHandle>,
        slot: watch::Sender<Option<CheckRun>>,
    ) {
        let key = (checkout_key.clone(), worktree.clone());
        // A superseded run lands here too: a re-run took the key while this
        // task was still unwinding. Its record must not clobber the
        // replacement — no persist, no publish, no removal — so every
        // finish checks it still owns the key.
        let finish_run = |this: &Self, run: CheckRun| {
            let still_current = this
                .runs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key)
                .is_some_and(|live| live.run_id == handle.run_id);
            if !still_current {
                return;
            }
            persist_run(&data_dir, &checkout_key, &worktree, &run);
            slot.send_replace(Some(run));
            let mut runs = this.runs.lock().unwrap_or_else(|e| e.into_inner());
            runs.remove(&key);
            drop(runs);
            let mut by_id = this.by_run_id.lock().unwrap_or_else(|e| e.into_inner());
            by_id.remove(&handle.run_id);
        };

        // The merge the run executes against. A conflict (or refusal) ends
        // the run before any command runs.
        let merge = match git
            .checks_merge_tree(&repo_path, worktree.clone(), trunk.clone())
            .await
        {
            Ok(merge) => merge,
            Err(error) => {
                finish_run(
                    self,
                    CheckRun {
                        run_id: handle.run_id.clone(),
                        trunk: trunk.clone(),
                        trunk_head: String::new(),
                        branch_head: String::new(),
                        stale: false,
                        started_at: now_ms(),
                        finished_at: Some(now_ms()),
                        checks: specs
                            .into_iter()
                            .map(|spec| CheckRow {
                                name: spec.name,
                                status: "pending".into(),
                                exit_code: None,
                                duration_ms: None,
                                log_tail: String::new(),
                            })
                            .collect(),
                        error: Some(error),
                    },
                );
                return;
            }
        };

        let started = now_ms();
        let mut run = CheckRun {
            run_id: handle.run_id.clone(),
            trunk: trunk.clone(),
            trunk_head: merge.trunk_head.clone(),
            branch_head: merge.branch_head.clone(),
            stale: false,
            started_at: started,
            finished_at: None,
            checks: specs
                .iter()
                .map(|spec| CheckRow {
                    name: spec.name.clone(),
                    status: "pending".into(),
                    exit_code: None,
                    duration_ms: None,
                    log_tail: String::new(),
                })
                .collect(),
            error: None,
        };

        // The throwaway checkout.
        let checkout_dir = data_dir.join("integration/checkouts").join(&handle.run_id);
        let checked_out = git
            .checkout_detached_tree(
                &repo_path,
                &handle.run_id,
                &merge.tree,
                &merge.trunk_head,
                &checkout_dir.display().to_string(),
            )
            .await;
        if let Err(error) = checked_out {
            run.error = Some(error);
            run.finished_at = Some(now_ms());
            finish_run(self, run);
            return;
        }

        // Serial execution; every row transition publishes a frame.
        for (index, spec) in specs.iter().enumerate() {
            if handle.stop.is_cancelled() {
                for row in run.checks[index..].iter_mut() {
                    if row.status == "pending" {
                        row.status = "stopped".into();
                    }
                }
                break;
            }
            run.checks[index].status = "running".into();
            slot.send_replace(Some(run.clone()));
            let began = std::time::Instant::now();

            let spawned = crate::background::spawn_group_process(
                "",
                &spec.run,
                &checkout_dir.display().to_string(),
                &shell,
                shell_groups.clone(),
                &[("CI", "true")],
            );
            let (process, child) = match spawned {
                Ok(pair) => pair,
                Err(error) => {
                    run.checks[index].status = "error".into();
                    run.checks[index].log_tail = error;
                    run.checks[index].duration_ms = Some(began.elapsed().as_millis() as u64);
                    slot.send_replace(Some(run.clone()));
                    continue;
                }
            };
            *handle.current.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((spec.name.clone(), process.clone()));

            let (done_tx, done_rx) = tokio::sync::oneshot::channel::<i32>();
            crate::background::supervise_group(process.clone(), child, move |process, code| {
                if let Some(record) = process.finish(code).flatten() {
                    #[cfg(unix)]
                    crate::shell_groups::forget(&record);
                }
                let _ = done_tx.send(code);
            });

            let stopped = handle.stop.clone();
            let timeout = spec.timeout;
            enum Outcome {
                Exited(i32),
                TimedOut,
                Stopped,
            }
            let mut done_rx = done_rx;
            let mut outcome = tokio::select! {
                code = &mut done_rx => Outcome::Exited(code.unwrap_or(-1)),
                _ = tokio::time::sleep(timeout) => Outcome::TimedOut,
                _ = stopped.cancelled() => Outcome::Stopped,
            };
            let timed_out = matches!(outcome, Outcome::TimedOut);
            if !matches!(outcome, Outcome::Exited(_)) {
                // Timed out, or stopped — and the stop may have landed
                // before this process reached the run's `current` slot, so
                // nothing else is guaranteed to kill it. Take the exit when
                // it lands.
                process.kill_with_grace();
                outcome = Outcome::Exited(done_rx.await.unwrap_or(-1));
            }
            let duration = began.elapsed().as_millis() as u64;
            let row = &mut run.checks[index];
            row.duration_ms = Some(duration);
            row.log_tail = process.tail(20, 4096);
            match outcome {
                Outcome::Exited(0) if !timed_out && !stopped.is_cancelled() => {
                    row.status = "passed".into()
                }
                Outcome::Exited(code) => {
                    row.status = if timed_out {
                        "timeout"
                    } else if stopped.is_cancelled() {
                        "stopped"
                    } else {
                        "failed"
                    }
                    .into();
                    row.exit_code = Some(code);
                }
                Outcome::TimedOut | Outcome::Stopped => {
                    unreachable!("folded into Exited above")
                }
            }
            *handle.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
            slot.send_replace(Some(run.clone()));
            if run.checks[index].status == "stopped" {
                for pending in run.checks[index + 1..].iter_mut() {
                    pending.status = "stopped".into();
                }
                break;
            }
        }

        run.finished_at = Some(now_ms());
        let _ = git.remove_check_worktree(&repo_path, &handle.run_id).await;
        finish_run(self, run);
    }

    /// Mark every unfinished run stopped (engine shutdown).
    pub fn shutdown(&self) {
        let handles: Vec<Arc<RunHandle>> = self
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for handle in handles {
            handle.stop.cancel();
            if let Some(process) = handle
                .current
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|(_, process)| process.clone())
            {
                process.kill_with_grace();
            }
        }
    }

    /// Forget a repository's persisted results (its Space deletion).
    pub fn forget_checkout(&self, data_dir: &Path, checkout_key: &str) {
        let dir = data_dir.join("integration/checks").join(checkout_key);
        if dir.is_dir()
            && let Err(error) = std::fs::remove_dir_all(&dir)
        {
            tracing::warn!(dir = %dir.display(), %error, "could not remove the checks results");
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn run_path(data_dir: &Path, checkout_key: &str, worktree: &str) -> PathBuf {
    data_dir
        .join("integration/checks")
        .join(checkout_key)
        .join(format!("{worktree}.json"))
}

/// Persist the latest run (tmp + rename); a damaged file reads as absent.
fn persist_run(data_dir: &Path, checkout_key: &str, worktree: &str, run: &CheckRun) {
    let path = run_path(data_dir, checkout_key, worktree);
    let Some(parent) = path.parent() else {
        return;
    };
    if let Err(error) = std::fs::create_dir_all(parent) {
        tracing::warn!(dir = %parent.display(), %error, "could not create the checks store");
        return;
    }
    let Ok(bytes) = serde_json::to_vec(run) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .is_err()
    {
        tracing::warn!(path = %path.display(), "could not persist the checks run");
    }
}

fn load_run(data_dir: &Path, checkout_key: &str, worktree: &str) -> Option<CheckRun> {
    let bytes = std::fs::read(run_path(data_dir, checkout_key, worktree)).ok()?;
    serde_json::from_slice(&bytes).ok()
}
