//! Fuzzy workspace path search behind the `SearchFiles` RPC: the composer
//! walks the resolved root for `@`-mention candidates. Files and
//! directories are candidates, matched on their full workspace-relative
//! path (so `cmp/send` finds `composer/send.rs`); contents never cross
//! this boundary.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use holt_proto::FileSearchMatch;
use ignore::{WalkBuilder, WalkState};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Matches returned at most — the palette pages a bounded list.
const RESULT_LIMIT: usize = 50;

/// Walked-entry ceiling: huge ignored trees (`node_modules`, `target`) stay
/// responsive because the walk quits once this many entries were seen. The
/// PARALLEL walk visits siblings concurrently, so shallow entries (a repo's
/// own files) are reached long before the cap can land — a depth-first
/// single-threaded walk could grind the cap away inside one deep ignored
/// directory and never reach the project files at all.
const WALK_ENTRY_CAP: usize = 500_000;

/// The best fuzzy path matches under `root`, best score first, ties broken
/// by path ascending so repeat queries order identically. An empty or
/// whitespace-only query matches nothing and never touches the disk.
///
/// Blocking (filesystem walk); callers on an async runtime must offload it
/// (`spawn_blocking`).
pub(crate) fn search(root: &Path, query: &str) -> Vec<FileSearchMatch> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let scored: Mutex<Vec<(u32, String, bool)>> = Mutex::new(Vec::new());
    let walked = AtomicUsize::new(0);
    let mut walker = WalkBuilder::new(root);
    // The mention contract searches hidden files and does not let ignore
    // files suppress candidates, but repository metadata is never
    // searched — `.git` in ANY form: the directory (the grep tool's guard)
    // or the gitdir-pointer file a worktree/submodule carries.
    walker
        .hidden(false)
        .standard_filters(false)
        .filter_entry(|entry| entry.file_name() != OsStr::new(".git"));
    walker.build_parallel().run(|| {
        let pattern = pattern.clone();
        let scored = &scored;
        let walked = &walked;
        let mut matcher = Matcher::new(Config::DEFAULT);
        Box::new(move |entry| {
            if walked.fetch_add(1, Ordering::Relaxed) >= WALK_ENTRY_CAP {
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if entry.depth() == 0 {
                return WalkState::Continue;
            }
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            let Ok(relative) = entry.path().strip_prefix(root) else {
                return WalkState::Continue;
            };
            let path = relative.to_string_lossy().replace('\\', "/");
            let mut buf = Vec::new();
            if let Some(score) = pattern.score(Utf32Str::new(&path, &mut buf), &mut matcher) {
                scored
                    .lock()
                    .expect("scored poisoned")
                    .push((score, path, is_dir));
            }
            WalkState::Continue
        })
    });
    let mut scored = scored.into_inner().expect("scored poisoned");
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.truncate(RESULT_LIMIT);
    scored
        .into_iter()
        .map(|(_, path, is_dir)| FileSearchMatch { path, is_dir })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(root: &Path) {
        std::fs::create_dir_all(root.join("composer")).unwrap();
        std::fs::write(root.join("composer/send.rs"), "").unwrap();
        std::fs::write(root.join(".hidden"), "").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.log\n").unwrap();
        std::fs::write(root.join("ignored.log"), "").unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/config"), "").unwrap();
    }

    fn paths(matches: &[FileSearchMatch]) -> Vec<&str> {
        matches.iter().map(|hit| hit.path.as_str()).collect()
    }

    #[test]
    fn an_empty_query_never_walks() {
        let dir = tempfile::TempDir::new().unwrap();
        tree(dir.path());
        assert!(search(dir.path(), "").is_empty());
        assert!(search(dir.path(), "  ").is_empty());
    }

    #[test]
    fn hidden_and_gitignored_entries_match_but_dot_git_does_not() {
        let dir = tempfile::TempDir::new().unwrap();
        tree(dir.path());
        assert!(paths(&search(dir.path(), "hidden")).contains(&".hidden"));
        assert!(paths(&search(dir.path(), "ignored")).contains(&"ignored.log"));
        assert!(
            search(dir.path(), "config")
                .iter()
                .all(|hit| !hit.path.starts_with(".git"))
        );
    }

    #[test]
    fn ordering_is_score_descending_then_path_ascending() {
        let dir = tempfile::TempDir::new().unwrap();
        tree(dir.path());
        let matches = search(dir.path(), "send");
        assert_eq!(paths(&matches).first(), Some(&"composer/send.rs"));
        let repeat = search(dir.path(), "send");
        assert_eq!(matches, repeat);
    }
}
