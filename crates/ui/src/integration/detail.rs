//! The integration page's detail: one worktree's title, a summary
//! (status, review, branch, changes, checks), and the embedded diff
//! panes. Pinned [`Changes`] views reuse the Changes pane wholesale.
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
    /// Status, review, branch, checks. The page opens here.
    Summary,
    /// The branch diff against the trunk.
    Changes,
    Commits,
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
    /// A file to scroll the Changes pane to once it exists (a review
    /// finding's click).
    pub(crate) pending_reveal: Option<String>,
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
            tab: DetailTab::Summary,
            changes: None,
            commit: None,
            pending_reveal: None,
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

    /// Open the Changes tab scrolled to `path`.
    pub(crate) fn reveal_in_changes(&mut self, path: String) {
        self.set_tab(DetailTab::Changes);
        self.pending_reveal = Some(path);
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
        review_line: SharedString,
        theme: &Theme,
        cx: &mut Context<IntegrationPage>,
    ) -> AnyElement {
        let orphan = worktree.chat_id.is_none() && {
            let data_dir = state.read(cx).data_dir.clone().unwrap_or_default();
            super::worktree_is_orphan(worktree, &data_dir)
        };
        // Start the branch diff with the detail, not on the Code click.
        // The capture is the slow part; the tab should open onto it.
        self.ensure_changes(worktree, state, cx);
        let header = self.render_header(worktree, title, orphan, theme, cx);
        let tabs = self.render_tabs(worktree, theme, cx);
        let body = match self.tab {
            DetailTab::Summary => self.render_summary(
                worktree,
                status,
                review_line,
                checks_card,
                review_card,
                theme,
            ),
            DetailTab::Commits => self.render_commits(worktree, theme, cx),
            DetailTab::Changes => self.render_changes(cx),
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
            .child(div().flex_1().min_h_0().overflow_hidden().child(body))
            .child(tabs)
            .into_any_element()
    }

    fn render_header(
        &mut self,
        worktree: &IntegrationWorktree,
        title: SharedString,
        orphan: bool,
        theme: &Theme,
        cx: &mut Context<IntegrationPage>,
    ) -> AnyElement {
        let age = worktree
            .commits
            .first()
            .and_then(|commit| chrono::DateTime::parse_from_rfc3339(&commit.authored_at).ok())
            .map(|time| {
                crate::state::format_time_ago(time.with_timezone(&chrono::Utc), chrono::Utc::now())
            })
            .unwrap_or_default();
        let mut meta = div()
            .mt(px(6.0))
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(12.0))
            .text_size(px(12.0))
            .text_color(theme.text_faint)
            .when(!age.is_empty(), |el| el.child(age));
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
            .pt(px(4.0))
            .pb(px(10.0))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(20.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(title),
            )
            .child(meta)
            .when(worktree.missing, |el| {
                el.child(
                    div()
                        .mt(px(6.0))
                        .text_size(px(12.0))
                        .text_color(theme.danger)
                        .child(t!("integration.detail.missing_hint")),
                )
            })
            .when(orphan, |el| {
                el.child(
                    div()
                        .mt(px(6.0))
                        .text_size(px(12.0))
                        .text_color(theme.warning)
                        .child(t!("integration.detail.orphan_hint")),
                )
            })
            .into_any_element()
    }

    fn render_tabs(
        &self,
        worktree: &IntegrationWorktree,
        theme: &Theme,
        cx: &mut Context<IntegrationPage>,
    ) -> AnyElement {
        use DetailTab::{Changes, Commits, Summary};
        let tab = self.tab;
        let entries: [(DetailTab, SharedString, Option<usize>); 3] = [
            (Summary, t!("integration.detail.tab_summary").into(), None),
            (Changes, t!("integration.detail.tab_code").into(), None),
            (
                Commits,
                t!("integration.detail.tab_activity").into(),
                Some(worktree.commits.len()),
            ),
        ];
        div()
            .flex_none()
            .flex()
            .justify_center()
            .py(px(10.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .p(px(2.0))
                    .rounded(px(9.0))
                    .bg(crate::theme::wash(0.06))
                    .children(entries.into_iter().map(|(entry, label, count)| {
                        let active = entry == tab;
                        div()
                            .id(SharedString::from(format!(
                                "integration-detail-tab-{entry:?}"
                            )))
                            .h(px(28.0))
                            .px(px(12.0))
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .rounded(px(7.0))
                            .cursor_pointer()
                            .text_size(px(12.5))
                            .when(active, |el| {
                                el.bg(theme.surface_raised).text_color(theme.text)
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
                                        .text_size(px(11.0))
                                        .text_color(theme.text_faint)
                                        .child(SharedString::from(count.to_string())),
                                )
                            })
                    })),
            )
            .into_any_element()
    }

    fn render_summary(
        &self,
        worktree: &IntegrationWorktree,
        status: WorktreeStatus,
        review_line: SharedString,
        checks_card: AnyElement,
        review_card: AnyElement,
        theme: &Theme,
    ) -> AnyElement {
        div()
            .id("integration-summary")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .child(self.property_card(worktree, status, review_line, theme))
            .child(checks_card)
            .child(review_card)
            .into_any_element()
    }

    fn property_card(
        &self,
        worktree: &IntegrationWorktree,
        status: WorktreeStatus,
        review_line: SharedString,
        theme: &Theme,
    ) -> AnyElement {
        let trunk = self.trunk.clone();
        let branch = worktree
            .branch
            .clone()
            .unwrap_or_else(|| t!("integration.detail.detached").into_owned());
        let changes = t!(
            "integration.detail.changes_line",
            commits = worktree.commits.len(),
            ahead = worktree.ahead,
            behind = worktree.behind
        );
        let row = |label: SharedString, value: AnyElement| {
            div()
                .h(px(36.0))
                .flex()
                .items_center()
                .gap(px(16.0))
                .child(
                    div()
                        .flex_none()
                        .w(px(88.0))
                        .text_size(px(12.5))
                        .text_color(theme.text_faint)
                        .child(label),
                )
                .child(value)
        };
        div()
            .flex_none()
            .rounded(px(12.0))
            .border_1()
            .border_color(theme.border.opacity(0.55))
            .px(px(16.0))
            .py(px(4.0))
            .flex()
            .flex_col()
            .child(row(
                t!("integration.detail.prop_status").into(),
                IntegrationPage::status_badge(status, &trunk, theme),
            ))
            .child(row(
                t!("integration.detail.prop_review").into(),
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.5))
                    .text_color(theme.text)
                    .child(review_line)
                    .into_any_element(),
            ))
            .child(row(
                t!("integration.detail.prop_branch").into(),
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(12.0))
                    .text_color(theme.text)
                    .child(format!("{branch} → {trunk}"))
                    .into_any_element(),
            ))
            .child(row(
                t!("integration.detail.prop_changes").into(),
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.5))
                    .text_color(theme.text)
                    .child(if worktree.missing {
                        t!("integration.detail.missing_hint").into_owned()
                    } else {
                        changes.to_string()
                    })
                    .into_any_element(),
            ))
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

    /// The branch diff, created once per detail. Called from `render` so
    /// the capture is in flight while Summary is showing.
    fn ensure_changes(
        &mut self,
        worktree: &IntegrationWorktree,
        state: &Entity<AppState>,
        cx: &mut Context<IntegrationPage>,
    ) {
        if self.changes.is_some() {
            return;
        }
        let pin = self.pin(worktree);
        let trunk = self.trunk.clone();
        let state = state.clone();
        self.changes = Some(cx.new(|cx| Changes::pinned(state, pin, None, Some(trunk), cx)));
    }

    fn render_changes(&mut self, cx: &mut Context<IntegrationPage>) -> AnyElement {
        let changes = self.changes.clone().expect("ensured before render");
        if let Some(path) = self.pending_reveal.take() {
            changes.update(cx, |changes, cx| changes.reveal_file(path, cx));
        }
        div().size_full().child(changes).into_any_element()
    }
}
