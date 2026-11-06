//! The integration page's right column: one worktree's header, tabs
//! (overview / commits / changes), and the embedded diff panes — pinned
//! [`Changes`] views over the worktree's checkout, reusing the Changes
//! pane's rendering wholesale.
//!
//! `Detail` is plain state owned by the page (not an entity): the listeners
//! its render builds are page listeners (`cx.listener`), so tab switches and
//! commit opens mutate the page's own copy while chat/terminal actions
//! bubble as page events to the shell.

use gpui::{AnyElement, Context, Entity, SharedString, div, prelude::*, px};
use holt_proto::{GitHistoryCommit, IntegrationSnapshot, IntegrationWorktree};

use crate::changes::{Changes, CheckoutPin};
use crate::i18n::t;
use crate::icons::{self, icon};
use crate::state::AppState;
use crate::theme::Theme;

use super::{IntegrationEvent, IntegrationPage, WorktreeStatus};

/// Which detail tab is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetailTab {
    Overview,
    Commits,
    Changes,
    /// 审查与检查 (reviews arrive in P3; the checks card lives here and on
    /// the overview).
    Review,
}

/// The selected worktree's detail view. Rebuilt when the selection or the
/// trunk moves, kept (and invalidated) across refreshes so tab and fold
/// state survive.
pub(crate) struct Detail {
    pub(crate) worktree: String,
    pub(crate) trunk: String,
    pub(crate) tab: DetailTab,
    /// The branch-scope diff pane (`merge-base(trunk, head) → workdir`),
    /// created on first visit to the Changes tab.
    pub(crate) changes: Option<Entity<Changes>>,
    /// A pinned commit diff while one commit is open from the Commits tab.
    pub(crate) commit: Option<(GitHistoryCommit, Entity<Changes>)>,
}

impl Detail {
    /// The detail for `worktree` against `trunk`; `None` when there is
    /// nothing to show. A key-matching `previous` is kept as is.
    pub(crate) fn rebuild(
        previous: Option<Detail>,
        worktree: &str,
        trunk: String,
    ) -> Option<Detail> {
        if worktree.is_empty() {
            return None;
        }
        if let Some(previous) = previous
            && previous.worktree == worktree
            && previous.trunk == trunk
        {
            return Some(previous);
        }
        Some(Detail {
            worktree: worktree.to_string(),
            trunk,
            tab: DetailTab::Overview,
            changes: None,
            commit: None,
        })
    }

    /// The pinned panes' checkout: the worktree path, with the owning chat's
    /// id when one exists (the branch/commit RPCs ignore it; the fetch key
    /// folds it in).
    pub(crate) fn pin(&self, worktree: &IntegrationWorktree) -> CheckoutPin {
        CheckoutPin {
            cwd: worktree.path.clone(),
            chat_id: worktree
                .chat_id
                .clone()
                .unwrap_or_else(|| format!("integration-{}", worktree.name)),
        }
    }

    pub(crate) fn set_tab(&mut self, tab: DetailTab) {
        if self.tab != tab {
            self.tab = tab;
            // Leaving a commit view resets to the commit list.
            if tab == DetailTab::Commits {
                self.commit = None;
            }
        }
    }

    /// Refetch the embedded panes after a snapshot refresh.
    pub(crate) fn invalidate(&self, cx: &mut Context<IntegrationPage>) {
        if let Some(changes) = &self.changes {
            changes.update(cx, |changes, cx| changes.invalidate(cx));
        }
        if let Some((_, changes)) = &self.commit {
            changes.update(cx, |changes, cx| changes.invalidate(cx));
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &mut self,
        worktree: &IntegrationWorktree,
        title: SharedString,
        status: WorktreeStatus,
        _snapshot: &IntegrationSnapshot,
        state: &Entity<AppState>,
        checks_card: AnyElement,
        review_card: AnyElement,
        theme: &Theme,
        cx: &mut Context<IntegrationPage>,
    ) -> AnyElement {
        let trunk = self.trunk.clone();
        let header = self.render_header(worktree, title, status, &trunk, theme, cx);
        let tabs = self.render_tabs(worktree, theme, cx);
        let body = match self.tab {
            DetailTab::Overview => self.render_overview(worktree, state, checks_card, theme, cx),
            DetailTab::Commits => self.render_commits(worktree, theme, cx),
            DetailTab::Changes => self.render_changes(worktree, state, cx),
            // The checks card and the review card come from the page (it
            // owns the watches).
            DetailTab::Review => div()
                .id("integration-review-scroll")
                .flex()
                .flex_col()
                .min_h_0()
                .overflow_y_scroll()
                .child(checks_card)
                .child(review_card)
                .into_any_element(),
        };
        let selector_name = self.worktree.clone();
        div()
            .id("integration-detail")
            .debug_selector(move || format!("integration-detail-{selector_name}"))
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(tabs)
            .child(div().flex_1().min_h_0().overflow_hidden().child(body))
            .into_any_element()
    }

    fn render_header(
        &mut self,
        worktree: &IntegrationWorktree,
        title: SharedString,
        status: WorktreeStatus,
        trunk: &str,
        theme: &Theme,
        cx: &mut Context<IntegrationPage>,
    ) -> AnyElement {
        let branch = worktree
            .branch
            .clone()
            .unwrap_or_else(|| t!("integration.detail.detached").into_owned());
        let mut meta = div()
            .mt(px(6.0))
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(12.0))
            .text_size(px(11.5))
            .text_color(theme.text_muted)
            .child(IntegrationPage::status_chip(status, trunk, theme))
            .child(
                div()
                    .flex_none()
                    .font_family(theme.font_mono.clone())
                    .text_color(theme.text_dim)
                    .child(format!("{branch} → {trunk}")),
            );
        if let Some(chat_id) = worktree.chat_id.clone() {
            let path = worktree.path.clone();
            let terminal_chat = chat_id.clone();
            meta = meta
                .child(
                    div()
                        .id("integration-open-chat")
                        .cursor_pointer()
                        .text_color(theme.text_muted)
                        .hover(|style| style.text_color(theme.text))
                        .underline()
                        .child(t!("integration.detail.open_chat"))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(IntegrationEvent::OpenChat(chat_id.clone()));
                        })),
                )
                .child(
                    div()
                        .id("integration-open-terminal")
                        .cursor_pointer()
                        .text_color(theme.text_muted)
                        .hover(|style| style.text_color(theme.text))
                        .underline()
                        .child(t!("integration.detail.open_terminal"))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(IntegrationEvent::OpenTerminal {
                                chat_id: terminal_chat.clone(),
                                path: path.clone(),
                            });
                        })),
                );
        }
        div()
            .flex_none()
            .pb(px(12.0))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(16.0))
                    .text_color(theme.text)
                    .child(title),
            )
            .child(meta)
            .into_any_element()
    }

    fn render_tabs(
        &self,
        worktree: &IntegrationWorktree,
        theme: &Theme,
        cx: &mut Context<IntegrationPage>,
    ) -> AnyElement {
        use DetailTab::{Changes, Commits, Overview, Review};
        let tab = self.tab;
        let entries: [(DetailTab, SharedString, Option<usize>); 4] = [
            (Overview, t!("integration.detail.tab_overview").into(), None),
            (
                Commits,
                t!("integration.detail.tab_commits").into(),
                Some(worktree.commits.len()),
            ),
            (Changes, t!("integration.detail.tab_changes").into(), None),
            (Review, t!("integration.detail.tab_review").into(), None),
        ];
        div()
            .flex_none()
            .flex()
            .items_center()
            .p(px(3.0))
            .rounded(px(10.0))
            .bg(crate::theme::wash(0.06))
            .children(entries.into_iter().map(|(entry, label, count)| {
                let active = entry == tab;
                div()
                    .id(SharedString::from(format!(
                        "integration-detail-tab-{entry:?}"
                    )))
                    .h(px(24.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .rounded(px(7.0))
                    .cursor_pointer()
                    .text_size(px(11.5))
                    .when(active, |el| {
                        el.bg(theme.surface_raised)
                            .shadow_sm()
                            .text_color(theme.text)
                    })
                    .when(!active, |el| {
                        el.hover(|style| style.text_color(theme.text))
                            .text_color(theme.text_muted)
                    })
                    .on_click(cx.listener(move |page, _, _, cx| {
                        if let Some(detail) = &mut page.detail {
                            detail.set_tab(entry);
                        }
                        cx.notify();
                    }))
                    .child(label)
                    .when_some(count, |el, count| {
                        el.child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(count.to_string())),
                        )
                    })
            }))
            .into_any_element()
    }

    fn render_overview(
        &mut self,
        worktree: &IntegrationWorktree,
        state: &Entity<AppState>,
        checks_card: AnyElement,
        theme: &Theme,
        cx: &mut Context<IntegrationPage>,
    ) -> AnyElement {
        let app = state.read(cx);
        let (chat_value, orphan) = match worktree.chat_id.as_deref() {
            Some(chat_id) => {
                let title = app
                    .chat_row(chat_id)
                    .and_then(|chat| chat.title.clone())
                    .unwrap_or_else(|| t!("integration.detail.untitled_chat").into_owned());
                (title, false)
            }
            None => (t!("integration.detail.no_chat").into_owned(), {
                let data_dir = app.data_dir.clone().unwrap_or_default();
                super::worktree_is_orphan(worktree, &data_dir)
            }),
        };
        let ahead_behind = format!(
            "{} · {}",
            t!("integration.detail.ahead", n = worktree.ahead),
            t!("integration.detail.behind", n = worktree.behind),
        );
        let rows: Vec<(SharedString, String)> = vec![
            (
                t!("integration.detail.row_branch").into(),
                worktree
                    .branch
                    .clone()
                    .unwrap_or_else(|| t!("integration.detail.detached").into_owned()),
            ),
            (
                t!("integration.detail.row_path").into(),
                worktree.path.clone(),
            ),
            (t!("integration.detail.row_vs_trunk").into(), ahead_behind),
            (t!("integration.detail.row_chat").into(), chat_value),
        ]
        .into_iter()
        .chain(
            worktree
                .merged
                .then(|| worktree.merge_sha.clone())
                .flatten()
                .map(|sha| {
                    (
                        t!("integration.detail.row_merged_as").into(),
                        sha.chars().take(12).collect(),
                    )
                }),
        )
        .collect();

        div()
            .id("integration-overview")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(theme.border.opacity(0.6))
                    .p(px(16.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .children(rows.into_iter().map(|(label, value)| {
                        div()
                            .flex()
                            .gap(px(14.0))
                            .text_size(px(12.0))
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(96.0))
                                    .text_color(theme.text_faint)
                                    .child(label),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .truncate()
                                    .font_family(theme.font_mono.clone())
                                    .text_size(px(11.5))
                                    .text_color(theme.text_muted)
                                    .child(value),
                            )
                    }))
                    .when(orphan, |el| {
                        el.child(
                            div()
                                .text_size(px(11.5))
                                .text_color(theme.warning)
                                .child(t!("integration.detail.orphan_hint")),
                        )
                    })
                    .when(worktree.missing, |el| {
                        el.child(
                            div()
                                .text_size(px(11.5))
                                .text_color(theme.danger)
                                .child(t!("integration.detail.missing_hint")),
                        )
                    }),
            )
            .child(checks_card)
            .into_any_element()
    }

    fn render_commits(
        &self,
        worktree: &IntegrationWorktree,
        theme: &Theme,
        cx: &mut Context<IntegrationPage>,
    ) -> AnyElement {
        // A pinned commit diff replaces the list until dismissed.
        if let Some((commit, changes)) = &self.commit {
            let sha: String = commit.sha.chars().take(7).collect();
            let subject = if commit.subject.is_empty() {
                t!("integration.detail.no_subject").into_owned()
            } else {
                commit.subject.clone()
            };
            return div()
                .size_full()
                .flex()
                .flex_col()
                .min_h_0()
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .pb(px(8.0))
                        .child(
                            div()
                                .id("integration-commit-back")
                                .h(px(24.0))
                                .px(px(8.0))
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .rounded(px(6.0))
                                .cursor_pointer()
                                .text_size(px(11.5))
                                .text_color(theme.text_muted)
                                .hover(|style| {
                                    style.bg(crate::theme::wash(0.08)).text_color(theme.text)
                                })
                                .child(icon(icons::ARROW_LEFT).size(px(12.0)))
                                .child(t!("integration.detail.all_commits"))
                                .on_click(cx.listener(|page, _, _, cx| {
                                    if let Some(detail) = &mut page.detail {
                                        detail.commit = None;
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .flex_none()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(11.0))
                                .text_color(theme.text_dim)
                                .child(sha),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .text_size(px(12.0))
                                .text_color(theme.text)
                                .child(subject),
                        ),
                )
                .child(div().flex_1().min_h_0().child(changes.clone()))
                .into_any_element();
        }

        let now = chrono::Utc::now();
        let rows: Vec<AnyElement> = worktree
            .commits
            .iter()
            .map(|commit| {
                let sha: String = commit.sha.chars().take(7).collect();
                let subject = if commit.subject.is_empty() {
                    t!("integration.detail.no_subject").into_owned()
                } else {
                    commit.subject.clone()
                };
                let age = chrono::DateTime::parse_from_rfc3339(&commit.authored_at)
                    .ok()
                    .map(|time| {
                        crate::state::format_time_ago(time.with_timezone(&chrono::Utc), now)
                    })
                    .unwrap_or_default();
                let open = commit.clone();
                div()
                    .id(SharedString::from(format!(
                        "integration-commit-row-{}",
                        commit.sha
                    )))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .px(px(12.0))
                    .h(px(36.0))
                    .flex_none()
                    .cursor_pointer()
                    .border_b_1()
                    .border_color(crate::theme::hairline(0.04))
                    .hover(|style| style.bg(crate::theme::ink(0.025)))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.open_commit_diff(open.clone(), cx);
                    }))
                    .child(
                        div()
                            .flex_none()
                            .w(px(48.0))
                            .font_family(theme.font_mono.clone())
                            .text_size(px(10.5))
                            .text_color(theme.text_dim)
                            .child(sha),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(theme.text)
                            .child(subject),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.5))
                            .text_color(theme.text_faint)
                            .child(age),
                    )
                    .into_any_element()
            })
            .collect();
        div()
            .id("integration-commits")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .when(worktree.commits.is_empty(), |el| {
                el.child(
                    div()
                        .p(px(24.0))
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child(t!(
                            "integration.detail.no_commits",
                            trunk = self.trunk.as_str()
                        )),
                )
            })
            .when(worktree.truncated, |el| {
                el.child(
                    div()
                        .p(px(12.0))
                        .text_size(px(10.5))
                        .text_color(theme.text_faint)
                        .child(t!("integration.detail.truncated")),
                )
            })
            .children(rows)
            .into_any_element()
    }

    fn render_changes(
        &mut self,
        worktree: &IntegrationWorktree,
        state: &Entity<AppState>,
        cx: &mut Context<IntegrationPage>,
    ) -> AnyElement {
        if self.changes.is_none() {
            let pin = self.pin(worktree);
            let trunk = self.trunk.clone();
            let state = state.clone();
            self.changes = Some(cx.new(|cx| Changes::pinned(state, pin, None, Some(trunk), cx)));
        }
        let changes = self.changes.clone().expect("just created");
        div().size_full().child(changes).into_any_element()
    }
}
