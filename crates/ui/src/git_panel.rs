//! The right-pane Git panel: the checkout's commit graph. It hosts the
//! same `GitHistory` entity the Changes pane's History scope uses,
//! instantiated separately so the two coexist; a commit row click
//! re-emits [`GitPanelEvent::OpenCommit`] for the surface strip to route
//! into the same pinned-commit diff tab the Changes pane opens. (Formerly
//! a Status/History pair with staging and commit writes — the status view
//! and the write trio are gone; ADR-0022 is historical.)

use gpui::prelude::*;
use gpui::{Context, Entity, Subscription, div};
use holt_proto::GitHistoryCommit;

use crate::history::{GitHistory, GitHistoryEvent};
use crate::state::AppState;

/// Events the host (the right pane's surface strip) listens for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitPanelEvent {
    /// A commit row was clicked — open it as its own pinned Changes diff
    /// tab (the same routing as the Changes pane's History scope).
    OpenCommit(GitHistoryCommit),
}

impl gpui::EventEmitter<GitPanelEvent> for GitPanel {}

/// The right-pane Git surface: hosts the checkout's commit graph and
/// routes its row clicks to the host.
pub struct GitPanel {
    /// The commit-graph entity — the same one the Changes pane's History
    /// scope hosts, instantiated separately so both coexist. It owns its
    /// data, loading, and rendering.
    history: Entity<GitHistory>,
    /// Holds the history entity's event subscription for the panel's
    /// lifetime.
    _history_events: Subscription,
}

impl GitPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let history = cx.new(|cx| GitHistory::new(state.clone(), cx));
        let _history_events = cx.subscribe(&history, |_this: &mut Self, _, event, cx| {
            if let GitHistoryEvent::OpenCommit(commit) = event {
                cx.emit(GitPanelEvent::OpenCommit(commit.clone()));
            }
        });
        Self {
            history,
            _history_events,
        }
    }

    /// The panel became the visible surface — the shell's activation hook.
    /// Visibility is History's refresh trigger.
    pub fn ensure_visible(&mut self, cx: &mut Context<Self>) {
        self.history
            .update(cx, |history, cx| history.ensure_current(cx));
    }
}

impl Render for GitPanel {
    fn render(&mut self, _window: &mut gpui::Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex_1()
            .min_h_0()
            .child(self.history.clone())
    }
}
