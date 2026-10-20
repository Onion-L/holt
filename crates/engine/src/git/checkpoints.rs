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

/// A bash command's window start: the uncommitted section map attribution
/// compares, plus — when the Turn checkpoints — the before side of every
/// uncommitted path, already stored, and HEAD for the clean ones.
pub(crate) struct WindowStart {
    pub sections: HashMap<String, String>,
    head: Option<Oid>,
    /// `None` when checkpointing is off or storing a before side failed —
    /// the window then records no step rather than a wrong before side.
    dirty: Option<HashMap<String, CheckpointSide>>,
}

impl WindowStart {
    pub(crate) fn empty() -> Self {
        Self {
            sections: HashMap::new(),
            head: None,
            dirty: None,
        }
    }
}

/// The chat's object store, created on first use.
pub(super) fn open_store(dir: &Path) -> Result<Repository, git2::Error> {
    Repository::open_bare(dir).or_else(|_| Repository::init_bare(dir))
}

pub(super) fn window_start(
    repo: &Repository,
    sections: HashMap<String, String>,
    store: Option<&Path>,
) -> WindowStart {
    let head = repo
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok())
        .map(|commit| commit.id());
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
        sections,
        head,
        dirty,
    }
}

/// The files a bash window changed, or `None` when the window has no
/// trustworthy before side.
pub(super) fn window_files(
    repo: &Repository,
    store: &Path,
    window: &WindowStart,
    paths: &[String],
) -> Result<Option<Vec<CheckpointFile>>, git2::Error> {
    let Some(dirty) = &window.dirty else {
        return Ok(None);
    };
    let Some(workdir) = repo.workdir() else {
        return Ok(None);
    };
    let store = open_store(store)?;
    let head = match window.head {
        Some(oid) => Some(repo.find_commit(oid)?.tree()?),
        None => None,
    };
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let before = match dirty.get(path) {
            Some(side) => side.clone(),
            None => head_side(repo, &store, head.as_ref(), path)?,
        };
        let after = workdir_side(&store, workdir, path)?;
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
