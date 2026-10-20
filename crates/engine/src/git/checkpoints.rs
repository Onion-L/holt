//! Step checkpoint content (ADR-step-checkpoints-in-a-chat-owned-object-store):
//! the exact before/after bytes of the paths one tool call changed, written
//! as blobs into the chat's own bare object store. The user's repository is
//! only read — HEAD blobs for files that were clean before the call.

use std::collections::HashMap;
use std::path::Path;

use git2::{Oid, Repository};
use serde::{Deserialize, Serialize};

/// Larger files are recorded by path only.
const MAX_CHECKPOINT_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// One side of a changed path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum CheckpointSide {
    /// The file did not exist.
    Absent,
    /// The exact bytes, by the store's opaque blob id.
    Blob { id: String },
    /// Over the size cap, or not a regular file: no content kept.
    Omitted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CheckpointFile {
    /// Repo-root-relative, the capture vocabulary.
    pub path: String,
    pub before: CheckpointSide,
    pub after: CheckpointSide,
}

/// What a call's step is measured against, kept on disk while the call
/// runs so a call a crash cut off can still be measured at the next load.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum CallStart {
    /// write/edit: the target's side.
    Path {
        path: String,
        before: CheckpointSide,
    },
    /// bash: HEAD, and the stored side of every uncommitted path.
    Window {
        head: Option<String>,
        dirty: HashMap<String, CheckpointSide>,
    },
}

/// A bash command's window start: the uncommitted section map attribution
/// compares, plus — when the Turn checkpoints — the call's start.
pub(crate) struct WindowStart {
    pub sections: HashMap<String, String>,
    /// `None` when checkpointing is off or storing a before side failed —
    /// the window then records no step rather than a wrong before side.
    pub start: Option<CallStart>,
}

impl WindowStart {
    pub(crate) fn empty() -> Self {
        Self {
            sections: HashMap::new(),
            start: None,
        }
    }
}

/// The chat's object store, created on first use.
pub(super) fn open_store(dir: &Path) -> Result<Repository, git2::Error> {
    Repository::open_bare(dir).or_else(|_| Repository::init_bare(dir))
}

fn head_oid(repo: &Repository) -> Option<Oid> {
    repo.head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok())
        .map(|commit| commit.id())
}

pub(super) fn window_start(
    repo: &Repository,
    sections: HashMap<String, String>,
    store: Option<&Path>,
) -> WindowStart {
    let dirty = store.and_then(|store| {
        let workdir = repo.workdir()?;
        let store = open_store(store).ok()?;
        sections
            .keys()
            .map(|path| Ok((path.clone(), workdir_side(&store, workdir, path)?)))
            .collect::<Result<HashMap<_, _>, git2::Error>>()
            .ok()
    });
    WindowStart {
        start: dirty.map(|dirty| CallStart::Window {
            head: head_oid(repo).map(|oid| oid.to_string()),
            dirty,
        }),
        sections,
    }
}

/// The files a bash window changed among `paths`, or `None` when the
/// window has no trustworthy before side.
pub(super) fn window_files(
    repo: &Repository,
    store: &Path,
    window: &WindowStart,
    paths: &[String],
) -> Result<Option<Vec<CheckpointFile>>, git2::Error> {
    let Some(CallStart::Window { head, dirty }) = &window.start else {
        return Ok(None);
    };
    changed_files(repo, &open_store(store)?, head.as_deref(), dirty, paths)
}

/// What a call changed between its start and now — the measure of a call
/// a crash cut off, so its delta is not known: every path uncommitted at
/// either end, and every path a HEAD move changed. Edits made by anything
/// else since the start are included; the caller words it that way.
pub(super) fn files_since(
    repo: &Repository,
    store: &Path,
    start: &CallStart,
    sections_now: &HashMap<String, String>,
) -> Result<Option<Vec<CheckpointFile>>, git2::Error> {
    let store = open_store(store)?;
    match start {
        CallStart::Path { path, before } => {
            let Some(workdir) = repo.workdir() else {
                return Ok(None);
            };
            let after = workdir_side(&store, workdir, path)?;
            Ok(Some(if after == *before {
                Vec::new()
            } else {
                vec![CheckpointFile {
                    path: path.clone(),
                    before: before.clone(),
                    after,
                }]
            }))
        }
        CallStart::Window { head, dirty } => {
            let mut paths: Vec<String> = dirty.keys().chain(sections_now.keys()).cloned().collect();
            let head_then = head.as_deref().and_then(|id| Oid::from_str(id).ok());
            let head_now = head_oid(repo);
            if head_then != head_now {
                let tree = |oid: Option<Oid>| -> Result<Option<git2::Tree<'_>>, git2::Error> {
                    oid.map(|oid| repo.find_commit(oid)?.tree()).transpose()
                };
                let diff = repo.diff_tree_to_tree(
                    tree(head_then)?.as_ref(),
                    tree(head_now)?.as_ref(),
                    None,
                )?;
                paths.extend(diff.deltas().filter_map(|delta| {
                    delta
                        .new_file()
                        .path()
                        .or_else(|| delta.old_file().path())
                        .map(|path| path.to_string_lossy().into_owned())
                }));
            }
            paths.sort();
            paths.dedup();
            changed_files(repo, &store, head.as_deref(), dirty, &paths)
        }
    }
}

fn changed_files(
    repo: &Repository,
    store: &Repository,
    head: Option<&str>,
    dirty: &HashMap<String, CheckpointSide>,
    paths: &[String],
) -> Result<Option<Vec<CheckpointFile>>, git2::Error> {
    let Some(workdir) = repo.workdir() else {
        return Ok(None);
    };
    let head = match head {
        Some(id) => Some(repo.find_commit(Oid::from_str(id)?)?.tree()?),
        None => None,
    };
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let before = match dirty.get(path) {
            Some(side) => side.clone(),
            None => head_side(repo, store, head.as_ref(), path)?,
        };
        let after = workdir_side(store, workdir, path)?;
        if before != after {
            files.push(CheckpointFile {
                path: path.clone(),
                before,
                after,
            });
        }
    }
    Ok(Some(files))
}

/// One working-tree path's current side, stored.
pub(super) fn path_side(
    repo: &Repository,
    store: &Path,
    path: &str,
) -> Result<Option<CheckpointSide>, git2::Error> {
    let Some(workdir) = repo.workdir() else {
        return Ok(None);
    };
    Ok(Some(workdir_side(&open_store(store)?, workdir, path)?))
}

fn workdir_side(
    store: &Repository,
    workdir: &Path,
    path: &str,
) -> Result<CheckpointSide, git2::Error> {
    let full = workdir.join(path);
    let Ok(metadata) = std::fs::symlink_metadata(&full) else {
        return Ok(CheckpointSide::Absent);
    };
    if !metadata.is_file() || metadata.len() > MAX_CHECKPOINT_FILE_BYTES {
        return Ok(CheckpointSide::Omitted);
    }
    match std::fs::read(&full) {
        Ok(bytes) => Ok(CheckpointSide::Blob {
            id: store.blob(&bytes)?.to_string(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CheckpointSide::Absent),
        Err(_) => Ok(CheckpointSide::Omitted),
    }
}

/// A clean path's before side: its HEAD blob, copied into the store.
fn head_side(
    repo: &Repository,
    store: &Repository,
    head: Option<&git2::Tree>,
    path: &str,
) -> Result<CheckpointSide, git2::Error> {
    let Some(entry) = head.and_then(|tree| tree.get_path(Path::new(path)).ok()) else {
        return Ok(CheckpointSide::Absent);
    };
    let Ok(blob) = repo.find_blob(entry.id()) else {
        // A submodule commit or a tree: nothing a file restore could write.
        return Ok(CheckpointSide::Omitted);
    };
    if entry.filemode() == i32::from(git2::FileMode::Link)
        || blob.size() as u64 > MAX_CHECKPOINT_FILE_BYTES
    {
        return Ok(CheckpointSide::Omitted);
    }
    Ok(CheckpointSide::Blob {
        id: store.blob(blob.content())?.to_string(),
    })
}
