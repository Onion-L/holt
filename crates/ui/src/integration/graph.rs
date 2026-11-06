//! Row order for the integration page's worktree list. Trunk commits stay
//! in the layout so a worktree sorts under its fork (missing ones float
//! first); the page renders only the worktree rows. Swim lanes are computed
//! for the tests and are not painted.

use std::collections::HashMap;

use holt_proto::{GitHistoryCommit, IntegrationSnapshot};

/// One list row: a trunk commit (by index into `trunk_commits`) or a
/// worktree's head (by index into `worktrees`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RowKind {
    Trunk(usize),
    Worktree(usize),
}

/// One branch's drawn span: the swim-lane it occupies, the rows its visual
/// ends sit on, and its style flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BranchSpan {
    pub name: String,
    /// 1-based; lane 0 is the trunk itself.
    pub lane: usize,
    /// The span's top row: the merge point when it merges back, else the
    /// head row.
    pub top_row: usize,
    /// The row the worktree's head dot sits on.
    pub tip_row: usize,
    /// The row the branch forks off the trunk.
    pub fork_row: usize,
    /// Merged branches draw dashed.
    pub dashed: bool,
    /// A visible merge point exists (merged with its `mergeSha` on a shown
    /// trunk row) — the branch returns to the trunk.
    pub merge_back: bool,
    /// The fork lies below the shown trunk window: the branch attaches to
    /// the bottom of the line.
    pub earlier: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeLayout {
    pub rows: Vec<RowKind>,
    pub spans: Vec<BranchSpan>,
    /// Lanes beyond the trunk (0 when every branch is also the trunk line).
    pub lane_count: usize,
}

/// A row's sort key: timestamp (descending in the final order), a tie-break
/// where trunk rows precede worktree rows, and the worktree sequence for
/// stability between equal keys.
type RowKey = (i64, bool, usize);

fn epoch_secs(authored_at: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(authored_at)
        .map(|time| time.timestamp())
        .unwrap_or(0)
}

fn commit_epoch(commit: &GitHistoryCommit) -> i64 {
    epoch_secs(&commit.authored_at)
}

/// Lay the snapshot out. Rows run newest-first: trunk commits by walk order,
/// worktree heads by their head commit's date — except merged worktrees with
/// a visible merge point, which sit directly under that commit, and rows
/// with no date to sort by (missing directories, unborn heads), which float
/// to the top where they ask for attention.
pub(crate) fn layout(snapshot: &IntegrationSnapshot) -> TreeLayout {
    // (key, row) pairs; the final order sorts by key descending.
    let mut keyed: Vec<(RowKey, RowKind)> =
        Vec::with_capacity(snapshot.trunk_commits.len() + snapshot.worktrees.len());
    for (index, commit) in snapshot.trunk_commits.iter().enumerate() {
        keyed.push(((commit_epoch(commit), true, 0), RowKind::Trunk(index)));
    }
    let trunk_row_of: HashMap<String, usize> = snapshot
        .trunk_commits
        .iter()
        .enumerate()
        .map(|(index, commit)| (commit.sha.clone(), index))
        .collect();

    for (index, worktree) in snapshot.worktrees.iter().enumerate() {
        // Missing directories and unreadable heads have nothing to sort by.
        let mut key = (i64::MAX, false, index);
        if let (false, Some(head_sha)) = (worktree.missing, worktree.head_sha.as_deref()) {
            if worktree.merged
                && let Some(merge_row) =
                    trunk_row_of.get(worktree.merge_sha.as_deref().unwrap_or(""))
            {
                // Directly under the merge commit, regardless of the head's
                // own date: the (ts, false) key sorts just after the merge
                // row's (ts, true).
                key = (
                    commit_epoch(&snapshot.trunk_commits[*merge_row]),
                    false,
                    index,
                );
            } else if let Some(head_row) = trunk_row_of.get(head_sha) {
                key = (
                    commit_epoch(&snapshot.trunk_commits[*head_row]),
                    false,
                    index,
                );
            } else if let Some(head) = worktree.commits.first() {
                key = (commit_epoch(head), false, index);
            }
        }
        keyed.push((key, RowKind::Worktree(index)));
    }

    // Descending by timestamp; ties keep trunk rows first (a worktree whose
    // head IS a trunk commit reads under that commit), then stable order.
    keyed.sort_by_key(|(key, _)| std::cmp::Reverse(*key));

    let rows: Vec<RowKind> = keyed.into_iter().map(|(_, row)| row).collect();
    let row_of = |kind: &RowKind| -> usize {
        rows.iter()
            .position(|row| row == kind)
            .expect("every kind was just placed")
    };
    let mut trunk_rows: HashMap<String, usize> = HashMap::new();
    for (index, commit) in snapshot.trunk_commits.iter().enumerate() {
        trunk_rows.insert(commit.sha.clone(), row_of(&RowKind::Trunk(index)));
    }
    let mut worktree_rows: HashMap<String, usize> = HashMap::new();
    for (index, worktree) in snapshot.worktrees.iter().enumerate() {
        worktree_rows.insert(worktree.name.clone(), row_of(&RowKind::Worktree(index)));
    }
    let row_count = rows.len();

    // Swim lanes: a branch occupies its lane from its top row down to its
    // fork row; time-overlapping branches stack outwards. Sorted by top row
    // so an earlier branch's claim is visible to the next.
    struct SpanPending {
        name: String,
        top_row: usize,
        tip_row: usize,
        fork_row: usize,
        dashed: bool,
        merge_back: bool,
        earlier: bool,
    }
    let mut pending: Vec<SpanPending> = Vec::new();
    for worktree in &snapshot.worktrees {
        if worktree.missing || worktree.head_sha.is_none() {
            continue;
        }
        let Some(tip_row) = worktree_rows.get(&worktree.name).copied() else {
            continue;
        };
        let merge_row = worktree
            .merged
            .then(|| {
                worktree
                    .merge_sha
                    .as_deref()
                    .and_then(|sha| trunk_rows.get(sha).copied())
            })
            .flatten();
        let (top_row, merge_back) = match merge_row {
            Some(row) => (row, true),
            None => (tip_row, false),
        };
        let (fork_row, earlier) = match worktree
            .fork_sha
            .as_deref()
            .and_then(|sha| trunk_rows.get(sha).copied())
        {
            Some(row) => (row, false),
            // The fork predates the shown trunk window: attach to the
            // bottom of the line.
            None => (row_count.saturating_sub(1), true),
        };
        pending.push(SpanPending {
            name: worktree.name.clone(),
            top_row,
            tip_row,
            fork_row,
            dashed: worktree.merged,
            merge_back,
            earlier,
        });
    }
    pending.sort_by_key(|span| span.top_row);

    let mut lane_end: Vec<usize> = Vec::new();
    let mut spans = Vec::with_capacity(pending.len());
    for span in pending {
        let lane = match lane_end.iter().position(|end| *end < span.top_row) {
            Some(lane) => lane,
            None => {
                lane_end.push(0);
                lane_end.len() - 1
            }
        };
        lane_end[lane] = span.fork_row;
        spans.push(BranchSpan {
            name: span.name,
            lane: lane + 1,
            top_row: span.top_row,
            tip_row: span.tip_row,
            fork_row: span.fork_row,
            dashed: span.dashed,
            merge_back: span.merge_back,
            earlier: span.earlier,
        });
    }

    TreeLayout {
        lane_count: lane_end.len(),
        rows,
        spans,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_proto::{IntegrationMainCheckout, IntegrationTrunk, IntegrationWorktree};

    fn commit(sha: &str, at_minutes: i64) -> GitHistoryCommit {
        GitHistoryCommit {
            sha: sha.to_string(),
            parent_shas: Vec::new(),
            subject: format!("subject {sha}"),
            author_name: "a".to_string(),
            author_email: "e".to_string(),
            authored_at: chrono::DateTime::from_timestamp(1_000_000 + at_minutes * 60, 0)
                .unwrap()
                .to_rfc3339(),
            refs: Vec::new(),
        }
    }

    fn worktree(name: &str, head: Option<&str>, fork: Option<&str>) -> IntegrationWorktree {
        IntegrationWorktree {
            name: name.to_string(),
            path: format!("/wt/{name}"),
            branch: Some(name.to_string()),
            head_sha: head.map(str::to_string),
            fork_sha: fork.map(str::to_string),
            ahead: 1,
            behind: 0,
            dirty: false,
            dirty_files: 0,
            merged: false,
            merge_sha: None,
            missing: false,
            chat_id: None,
            commits: Vec::new(),
            truncated: false,
        }
    }

    fn snapshot(
        trunk_commits: Vec<GitHistoryCommit>,
        worktrees: Vec<IntegrationWorktree>,
    ) -> IntegrationSnapshot {
        IntegrationSnapshot {
            repo_path: "/repo".to_string(),
            trunk: IntegrationTrunk {
                name: "main".to_string(),
                head_sha: trunk_commits[0].sha.clone(),
            },
            trunk_commits,
            trunk_truncated: false,
            main_checkout: IntegrationMainCheckout {
                path: "/repo".to_string(),
                branch: Some("main".to_string()),
                dirty: false,
            },
            worktrees,
        }
    }

    /// Row kinds in order, as `(T0 | W<name>)` strings for compact asserts.
    fn row_kinds(layout: &TreeLayout, snapshot: &IntegrationSnapshot) -> Vec<String> {
        layout
            .rows
            .iter()
            .map(|row| match row {
                RowKind::Trunk(index) => format!("T{}", snapshot.trunk_commits[*index].sha),
                RowKind::Worktree(index) => format!("W{}", snapshot.worktrees[*index].name),
            })
            .collect()
    }

    #[test]
    fn worktree_heads_sort_by_date_between_trunk_commits() {
        // Trunk: c3 (newest), c2, c1. Branches: b-new (newer than c3),
        // b-old (between c2 and c1).
        let trunk = vec![commit("c3", 30), commit("c2", 20), commit("c1", 10)];
        let b_new = IntegrationWorktree {
            commits: vec![commit("b-new-head", 40)],
            ..worktree("b-new", Some("b-new-head"), Some("c3"))
        };
        let b_old = IntegrationWorktree {
            commits: vec![commit("b-old-head", 15)],
            ..worktree("b-old", Some("b-old-head"), Some("c1"))
        };
        let snap = snapshot(trunk, vec![b_new, b_old]);
        let layout = layout(&snap);
        assert_eq!(
            row_kinds(&layout, &snap),
            ["Wb-new", "Tc3", "Tc2", "Wb-old", "Tc1"]
        );
        // Distinct fork rows at overlapping times: separate lanes.
        let lane = |name: &str| {
            layout
                .spans
                .iter()
                .find(|span| span.name == name)
                .unwrap()
                .lane
        };
        assert_eq!(lane("b-new"), 1);
        assert_eq!(lane("b-old"), 1, "forks differ, one lane suffices");
    }

    #[test]
    fn merged_worktrees_sit_under_their_merge_commit() {
        let trunk = vec![commit("c3", 30), commit("c2", 20), commit("c1", 10)];
        let merged = IntegrationWorktree {
            merged: true,
            merge_sha: Some("c2".to_string()),
            commits: vec![commit("m-head", 12)],
            ..worktree("merged-wt", Some("m-head"), Some("c1"))
        };
        let snap = snapshot(trunk, vec![merged]);
        let layout = layout(&snap);
        assert_eq!(
            row_kinds(&layout, &snap),
            ["Tc3", "Tc2", "Wmerged-wt", "Tc1"]
        );
        let span = &layout.spans[0];
        assert!(span.dashed);
        assert!(span.merge_back);
        assert_eq!(span.top_row, 1, "the span leaves the trunk at the merge");
        assert_eq!(span.fork_row, 3, "and returns at the fork");
    }

    #[test]
    fn overlapping_branches_take_separate_lanes() {
        // Two branches forking from the same commit with close heads:
        // time-overlapping, so they must not share a lane.
        let trunk = vec![commit("c2", 20), commit("c1", 10)];
        let first = IntegrationWorktree {
            commits: vec![commit("h1", 30)],
            ..worktree("first", Some("h1"), Some("c1"))
        };
        let second = IntegrationWorktree {
            commits: vec![commit("h2", 29)],
            ..worktree("second", Some("h2"), Some("c1"))
        };
        let snap = snapshot(trunk, vec![first, second]);
        let layout = layout(&snap);
        let lanes: Vec<(String, usize)> = layout
            .spans
            .iter()
            .map(|span| (span.name.clone(), span.lane))
            .collect();
        assert_eq!(lanes, [("first".into(), 1), ("second".into(), 2)]);
        assert_eq!(layout.lane_count, 2);
    }

    #[test]
    fn a_fork_below_the_window_attaches_to_the_bottom() {
        let trunk = vec![commit("c2", 20), commit("c1", 10)];
        let old = IntegrationWorktree {
            commits: vec![commit("old-head", 25)],
            ..worktree("old", Some("old-head"), Some("root-commit"))
        };
        let snap = snapshot(trunk, vec![old]);
        let layout = layout(&snap);
        let span = &layout.spans[0];
        assert!(span.earlier);
        assert_eq!(span.fork_row, layout.rows.len() - 1);
    }

    #[test]
    fn missing_worktrees_float_to_the_top_without_a_span() {
        let trunk = vec![commit("c2", 20), commit("c1", 10)];
        let gone = IntegrationWorktree {
            missing: true,
            ..worktree("gone", None, None)
        };
        let snap = snapshot(trunk, vec![gone]);
        let layout = layout(&snap);
        assert_eq!(row_kinds(&layout, &snap), ["Wgone", "Tc2", "Tc1"]);
        assert!(layout.spans.is_empty());
    }

    #[test]
    fn a_head_on_the_trunk_sorts_under_its_commit() {
        // A branch with no commits of its own: its head IS a trunk commit.
        let trunk = vec![commit("c2", 20), commit("c1", 10)];
        let fresh = worktree("fresh", Some("c1"), Some("c1"));
        let snap = snapshot(trunk, vec![fresh]);
        let layout = layout(&snap);
        assert_eq!(row_kinds(&layout, &snap), ["Tc2", "Tc1", "Wfresh"]);
    }
}
