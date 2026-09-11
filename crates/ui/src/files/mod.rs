//! The File sidebar: the Space-owned file editing state (ADR-0020), the
//! far-right directory tree, and the file contents tabs that share the right
//! pane's surface strip with Diff/Terminal/Subagent views.
//!
//! Ownership split (ADR-0020): file tabs, their contents, and tree expansion
//! belong to a **Space** and are shared by its Chats; Terminal/Diff/Subagent
//! views stay Chat-owned. The strip in `shell::right_pane` renders the
//! Space's file tabs ahead of the selected Chat's private views.

pub mod editor;
pub mod image_surface;
pub mod preview;
pub mod tree;
pub mod viewer;

use std::collections::HashMap;

use gpui::{Entity, actions};

use crate::state::AppState;
use viewer::FileViewer;

actions!(files, [SaveFile, FindInFile, TogglePreview]);

/// One open file tab. Lives in [`FileTabs`] keyed by the owning Space —
/// never per Chat (ADR-0020).
pub struct FileTab {
    pub id: u64,
    /// The path the tree opened — the entry's own path, possibly a symlink.
    pub path: String,
    /// The engine-resolved canonical path once the first read answered.
    /// Alias-aware tab identity: two opens through the same inside-root
    /// symlink select one tab.
    pub resolved: Option<String>,
    /// Preview tabs are replaceable by the next preview open; double-click
    /// (and, from ticket 02 on, editing) pins a tab for good.
    pub pinned: bool,
    pub viewer: Entity<FileViewer>,
}

impl FileTab {
    /// The strip chip's title: the entry's own name, not the target's.
    pub fn title(&self) -> &str {
        self.path
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(self.path.as_str())
    }
}

/// The Space-owned file editing state: the open tabs in strip order, plus
/// which one the Space's strip selects. Chats in the Space share this.
#[derive(Default)]
pub struct FileTabs {
    pub tabs: Vec<FileTab>,
}

impl FileTabs {
    pub fn find(&self, id: u64) -> Option<&FileTab> {
        self.tabs.iter().find(|tab| tab.id == id)
    }

    pub fn position(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    /// The tab whose file `path`/`resolved` refers to — duplicate-open
    /// detection. Matches the exact entry path (always), the caller's
    /// resolved hint against the tab's resolved target (alias through
    /// alias), and the tab's resolved target against the opened path
    /// (canonical path opened directly hitting an alias's tab).
    pub fn find_by_path(&self, path: &str, resolved: Option<&str>) -> Option<u64> {
        self.tabs
            .iter()
            .find(|tab| {
                let alias_match = resolved
                    .is_some_and(|hint| tab.resolved.as_deref() == Some(hint))
                    || tab.resolved.as_deref() == Some(path);
                alias_match || tab.path == path
            })
            .map(|tab| tab.id)
    }

    /// Which tab id the strip should select: `explicit` when the Space has a
    /// live pick, else the last tab.
    pub fn selected(&self, explicit: Option<u64>) -> Option<u64> {
        match explicit {
            Some(id) if self.find(id).is_some() => Some(id),
            _ => self.tabs.last().map(|tab| tab.id),
        }
    }
}

/// Per-Space file state on the shell. Keys are space ids (or `cwd:`-prefixed
/// working directories for chats that predate their space).
#[derive(Default)]
pub struct FileStateMap {
    map: HashMap<String, FileTabs>,
    /// The file tab the strip selects, per space (in-memory; persisted
    /// navigation arrives with ticket 05).
    active: HashMap<String, u64>,
}

impl FileStateMap {
    pub fn get(&mut self, space: &str) -> &mut FileTabs {
        self.map.entry(space.to_string()).or_default()
    }

    /// Read-only lookup for render paths.
    pub fn space(&self, space: &str) -> Option<&FileTabs> {
        self.map.get(space)
    }

    /// Every (space, tabs) pair, arbitrary order — census walks.
    pub fn spaces(&self) -> impl Iterator<Item = (&String, &FileTabs)> {
        self.map.iter()
    }

    /// Drop a space's records entirely (space removal): tabs, viewers (held
    /// by the tab entries), and the strip's selection. Called only after the
    /// draft-close decision succeeded.
    pub fn purge_space(&mut self, space: &str) {
        self.map.remove(space);
        self.active.remove(space);
    }

    pub fn active(&self, space: &str) -> Option<u64> {
        self.active.get(space).copied()
    }

    pub fn set_active(&mut self, space: &str, id: u64) {
        self.active.insert(space.to_string(), id);
    }

    pub fn clear_active(&mut self, space: &str) {
        self.active.remove(space);
    }

    /// Drop a closed tab and heal the space's selection.
    pub fn remove(&mut self, space: &str, id: u64) -> Option<Entity<FileViewer>> {
        let index = self.get(space).position(id)?;
        let viewer = self.get(space).tabs.remove(index).viewer;
        if self.active(space) == Some(id) {
            let tabs = &self.get(space).tabs;
            let next = tabs
                .get(index.min(tabs.len().saturating_sub(1)))
                .map(|next| next.id);
            match next {
                Some(next) => self.set_active(space, next),
                None => self.clear_active(space),
            }
        }
        Some(viewer)
    }

    /// The space key for the shell's current selection: the selected chat's
    /// space, the chat's own cwd when it has none, the selected space on the
    /// new-chat canvas, `None` when there is nothing to browse.
    pub fn space_key(state: &AppState) -> Option<String> {
        if let Some(chat) = state.selected_chat_row() {
            Some(
                chat.space_id
                    .clone()
                    .or_else(|| chat.cwd.clone().map(|cwd| format!("cwd:{cwd}")))?,
            )
        } else {
            state.selected_space.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use gpui::{App, AppContext as _, TestAppContext};
    use holt_proto::{Chat, Space};

    fn chat(id: &str, space_id: Option<&str>, cwd: Option<&str>) -> Chat {
        Chat {
            id: id.into(),
            device_id: "device".into(),
            title: None,
            title_source: holt_proto::TitleSource::Automatic,
            title_task_started: false,
            archived: false,
            cwd: cwd.map(str::to_string),
            branch: None,
            checkout_id: None,
            source_context: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: chrono::Utc::now(),
            space_id: space_id.map(str::to_string),
            last_seen_at: None,
            room_gen: None,
            compact_before_next_turn: false,
            plan_mode: None,
            approved_plan_path: None,
        }
    }

    fn space(id: &str) -> Space {
        Space {
            id: id.into(),
            device_id: "device".into(),
            path: format!("/tmp/{id}"),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn tab(cx: &mut App, id: u64, path: &str, resolved: Option<&str>, pinned: bool) -> FileTab {
        let state = cx.new(|_| AppState::new());
        let viewer = cx.new(|cx| {
            viewer::FileViewer::new(
                state,
                path.to_string(),
                viewer::FileScope {
                    chat_id: None,
                    space_id: Some("space-1".into()),
                },
                cx,
            )
        });
        FileTab {
            id,
            path: path.to_string(),
            resolved: resolved.map(str::to_string),
            pinned,
            viewer,
        }
    }

    #[gpui::test]
    fn space_key_follows_chat_space_then_cwd_then_canvas(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let state = cx.new(|_| AppState::new());
            state.update(cx, |state, _| {
                // Nothing selected: no key, the tree's empty state.
                assert_eq!(FileStateMap::space_key(state), None);

                // Canvas: the selected space.
                state.selected_space = Some("space-1".into());
                assert_eq!(FileStateMap::space_key(state).as_deref(), Some("space-1"));

                // A selected chat implies its space.
                state.chats = vec![chat("chat-1", Some("space-1"), None)];
                state.selected_chat = Some("chat-1".into());
                assert_eq!(FileStateMap::space_key(state).as_deref(), Some("space-1"));

                // Two chats in one space share the key (ADR-0020).
                state.chats.push(chat("chat-2", Some("space-1"), None));
                state.selected_chat = Some("chat-2".into());
                assert_eq!(FileStateMap::space_key(state).as_deref(), Some("space-1"));

                // A spaceless chat falls back to its own cwd.
                state.chats.push(chat("chat-3", None, Some("/tmp/solo")));
                state.selected_chat = Some("chat-3".into());
                assert_eq!(
                    FileStateMap::space_key(state).as_deref(),
                    Some("cwd:/tmp/solo")
                );

                // A spaceless chat without a cwd has nothing to browse.
                state.chats.push(chat("chat-4", None, None));
                state.selected_chat = Some("chat-4".into());
                assert_eq!(FileStateMap::space_key(state), None);
            });
        });
    }

    #[gpui::test]
    fn tab_titles_use_the_entry_name(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let first = tab(cx, 1, "/tmp/space-1/src/main.rs", None, false);
            assert_eq!(first.title(), "main.rs");
            // A trailing separator never yields an empty title.
            let dir = tab(cx, 2, "/tmp/space-1/src/", None, false);
            assert_eq!(dir.title(), "src");
        });
    }

    #[gpui::test]
    fn duplicate_opens_match_by_resolved_alias_or_exact_path(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut map = FileStateMap::default();
            map.get("space-1").tabs.push(tab(
                cx,
                1,
                "/tmp/space-1/real.rs",
                Some("/tmp/space-1/real.rs"),
                false,
            ));
            map.get("space-1")
                .tabs
                .push(tab(cx, 2, "/tmp/space-1/other.rs", None, true));

            // An alias open whose resolved target matches tab 1 selects it —
            // even though the entry path differs.
            assert_eq!(
                map.space("space-1")
                    .unwrap()
                    .find_by_path("/tmp/space-1/alias.rs", Some("/tmp/space-1/real.rs")),
                Some(1)
            );
            // Exact unresolved path matches before the read answers.
            assert_eq!(
                map.space("space-1")
                    .unwrap()
                    .find_by_path("/tmp/space-1/other.rs", None),
                Some(2)
            );
            // No match for a distinct file.
            assert_eq!(
                map.space("space-1")
                    .unwrap()
                    .find_by_path("/tmp/space-1/third.rs", None),
                None
            );
        });
    }

    #[gpui::test]
    fn removing_a_tab_heals_the_space_selection(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut map = FileStateMap::default();
            map.get("space-1")
                .tabs
                .push(tab(cx, 1, "/tmp/space-1/a.rs", None, false));
            map.get("space-1")
                .tabs
                .push(tab(cx, 2, "/tmp/space-1/b.rs", None, false));
            map.get("space-1")
                .tabs
                .push(tab(cx, 3, "/tmp/space-1/c.rs", None, false));
            map.set_active("space-1", 2);

            // Closing the selected tab lands on its right neighbor.
            assert!(map.remove("space-1", 2).is_some());
            assert_eq!(map.active("space-1"), Some(3));

            // Closing the LAST tab clears the selection entirely.
            map.set_active("space-1", 3);
            assert!(map.remove("space-1", 3).is_some());
            assert_eq!(map.active("space-1"), Some(1));
            assert!(map.remove("space-1", 1).is_some());
            assert_eq!(map.active("space-1"), None);
            assert!(map.space("space-1").unwrap().tabs.is_empty());

            // Spaces are independent.
            map.get("space-2")
                .tabs
                .push(tab(cx, 9, "/tmp/space-2/x.rs", None, false));
            assert_eq!(map.space("space-2").unwrap().tabs.len(), 1);
        });
    }

    #[gpui::test]
    fn selected_falls_back_to_the_last_tab(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut map = FileStateMap::default();
            map.get("space-1")
                .tabs
                .push(tab(cx, 1, "/tmp/space-1/a.rs", None, false));
            map.get("space-1")
                .tabs
                .push(tab(cx, 2, "/tmp/space-1/b.rs", None, false));
            // A stale explicit pick falls back rather than dead-ending.
            assert_eq!(map.space("space-1").unwrap().selected(Some(99)), Some(2));
            assert_eq!(map.space("space-1").unwrap().selected(Some(1)), Some(1));
            assert_eq!(map.space("space-1").unwrap().selected(None), Some(2));
        });
    }

    #[test]
    fn space_rows_exist_for_the_reference_space_helper() {
        // The Space construction used above must stay complete: proto field
        // drift breaks every test in this module loudly.
        let space = space("probe");
        assert!(space.path.ends_with("probe"));
    }
}
