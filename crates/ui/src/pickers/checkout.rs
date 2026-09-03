//! Branch/checkout behavior: ref selection (in-place safe switches of the
//! target working directory — the chat's own folder for sessions, the
//! space's folder for the draft), branch creation, checkout-kind selection
//! (draft-only), and the branch + checkout popover rendering.

use gpui::{AnyElement, App, Context, Focusable as _, SharedString, div, prelude::*, px};

use holt_proto::RepoRef;
use holt_rpc::methods;

use crate::popover::{self, Loadable};
use crate::theme::Theme;

use super::Pickers;
use super::logic::{
    CheckoutKind, CheckoutPlan, PickRouting, PickSurface, branch_create_name, pick_routing,
    worktree_hosted_dialog,
};
use super::{MAX_REF_ROWS, PickerKind};

impl Pickers {
    // ---- selections ----

    /// One ref-pick, routed the same way on every surface (ADR-0007): the
    /// already-current ref closes, a worktree-hosted ref explains itself,
    /// and a plain ref safe-switches the target working directory right
    /// away — mid-Turn included; the running Turn is never interrupted.
    pub(super) fn pick_ref(&mut self, row: RepoRef, cx: &mut Context<Self>) {
        let surface = self.pick_surface(cx);
        match pick_routing(
            surface,
            self.config.checkout == CheckoutKind::NewWorktree,
            &row,
        ) {
            PickRouting::AlreadyCurrent => {
                self.animate_close(cx);
                cx.notify();
            }
            PickRouting::RecordPick => {
                // Draft state only (a new-worktree base): no git yet.
                self.config.branch = Some(row.name.clone());
                self.animate_close(cx);
                cx.notify();
            }
            PickRouting::Switch => {
                self.safe_switch_ref(row, surface, cx);
            }
            PickRouting::WorktreeHosted => {
                self.switch_dialog = Some(worktree_hosted_dialog(
                    &row.name,
                    row.worktree_path.as_deref().unwrap_or_default(),
                ));
                self.animate_close(cx);
                cx.notify();
            }
        }
    }

    /// Which surface the composer is configuring: an existing chat, or the
    /// new-chat draft.
    pub(super) fn pick_surface(&self, cx: &App) -> PickSurface {
        match self.state.read(cx).selected_chat_row() {
            Some(_) => PickSurface::Session,
            None => PickSurface::Draft,
        }
    }

    /// The working directory branch operations address (ADR-0007): the
    /// chat's own folder in a session — a legacy worktree chat switches
    /// inside its own worktree — and the space's folder in the draft.
    pub(super) fn switch_target_path(&self, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        if let Some(cwd) = state.selected_chat_row().and_then(|chat| chat.cwd.clone()) {
            return Some(cwd);
        }
        state.selected_space_row().map(|space| space.path.clone())
    }

    /// Safe-switch the target working directory to a plain ref. Success
    /// closes the popover and refreshes the rows' `current` tags; failure
    /// closes it too and raises the switch dialog (ADR-0007 — dirty-tree
    /// refusals with their blocking files, and every other failure).
    fn safe_switch_ref(&mut self, row: RepoRef, surface: PickSurface, cx: &mut Context<Self>) {
        if self.switching.is_some() {
            return; // one switch at a time
        }
        let Some(repo_path) = self.switch_target_path(cx) else {
            return;
        };
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.switching = Some(row.name.clone());
        let ref_name = row.name.clone();
        self.switch_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(repo_path.clone()),
            );
            params.insert(
                "refName".into(),
                serde_json::Value::String(ref_name.clone()),
            );
            let result = engine
                .client()
                .call(methods::SWITCH_REF, serde_json::Value::Object(params))
                .await;
            this.update(cx, |pickers, cx| {
                pickers.switching = None;
                match result {
                    Ok(_) => {
                        // Draft-only bookkeeping: a session's branch label
                        // is the working directory's live HEAD, restamped by
                        // the refreshed rows (and by the next Turn).
                        if surface == PickSurface::Draft {
                            pickers.config.branch = Some(ref_name);
                        }
                        pickers.animate_close(cx);
                        pickers.ensure_refs(true, cx);
                    }
                    Err(err) => {
                        pickers.switch_dialog =
                            Some(super::logic::switch_dialog_content(&err.to_string()));
                        pickers.animate_close(cx);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The create row's submit: `CreateBranch` in the TARGET working
    /// directory (create-and-switch) — mid-chat included. Success closes
    /// the popover and refreshes the rows (the fresh branch is current);
    /// failure raises the switch dialog over the still-open popover, so
    /// the typed name survives for a retry.
    pub(super) fn create_branch_submit(&mut self, cx: &mut Context<Self>) {
        if !self.branch_create_engaged || self.switching.is_some() {
            return; // not engaged, or a checkout-changing op is in flight
        }
        let Some(name) = branch_create_name(self.branch_create.read(cx).text()) else {
            return; // empty after trim: nothing to create
        };
        let Some(repo_path) = self.switch_target_path(cx) else {
            return;
        };
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let surface = self.pick_surface(cx);
        self.switching = Some(name.clone());
        self.create_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(repo_path.clone()),
            );
            params.insert("name".into(), serde_json::Value::String(name.clone()));
            let result = engine
                .client()
                .call(methods::CREATE_BRANCH, serde_json::Value::Object(params))
                .await;
            this.update(cx, |pickers, cx| {
                pickers.switching = None;
                match result {
                    Ok(_) => {
                        if surface == PickSurface::Draft {
                            pickers.config.branch = Some(name);
                            // The fresh branch lives in the space's local checkout.
                            pickers.config.checkout = CheckoutKind::Local;
                        }
                        pickers.branch_create_engaged = false;
                        pickers
                            .branch_create
                            .update(cx, |input, cx| input.set_text("", cx));
                        pickers.animate_close(cx);
                        pickers.ensure_refs(true, cx);
                    }
                    Err(err) => {
                        pickers.switch_dialog =
                            Some(super::logic::switch_dialog_content(&err.to_string()));
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub(super) fn pick_checkout(&mut self, kind: CheckoutKind, cx: &mut Context<Self>) {
        if kind == CheckoutKind::Local
            && self.config.checkout == CheckoutKind::NewWorktree
            && self.selected_ref().is_some_and(|r| !r.current)
        {
            // Back to "Current checkout" with a non-current base picked:
            // drop the pick — the current branch takes over.
            self.config.branch = None;
        }
        self.config.checkout = kind;
        self.animate_close(cx);
        cx.notify();
    }

    pub(super) fn filtered_ref_rows(&self, cx: &App) -> Vec<RepoRef> {
        let Some(refs) = self.refs.ready() else {
            return Vec::new();
        };
        let names: Vec<String> = refs.iter().map(|r| r.name.clone()).collect();
        let query = self.search.read(cx).text().to_string();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| refs[ix].clone())
            .collect()
    }

    // ---- checkout resolution ----

    /// Index of the highlighted-by-default row in the (filtered) ref list:
    /// [`Self::highlighted_branch`] on a session, the draft pick on a new
    /// chat. Capped to the displayed window.
    pub(super) fn selected_ref_index(&self, cx: &App) -> usize {
        let rows = self.filtered_ref_rows(cx);
        let index = match self.highlighted_branch(cx) {
            Some(name) => rows.iter().position(|r| r.name == name).unwrap_or(0),
            None => rows.iter().position(|r| r.current).unwrap_or(0),
        };
        index.min(MAX_REF_ROWS.saturating_sub(1))
    }

    /// The branch the popover highlights: the working directory's live
    /// current branch on an existing chat (falling back to its stamped
    /// branch — its latest Turn's), the draft pick on a new one.
    fn highlighted_branch(&self, cx: &App) -> Option<String> {
        match self.pick_surface(cx) {
            PickSurface::Session => self.live_current_branch().or_else(|| {
                self.state
                    .read(cx)
                    .selected_chat_row()
                    .and_then(|c| c.branch.clone())
            }),
            PickSurface::Draft => self.config.branch.clone(),
        }
    }

    /// The target working directory's live current branch, as the loaded
    /// rows tag it (the rows are listed against the chat's own folder in a
    /// session — ADR-0007).
    pub(super) fn live_current_branch(&self) -> Option<String> {
        self.refs
            .ready()?
            .iter()
            .find(|r| r.current)
            .map(|r| r.name.clone())
    }

    /// The picked ref's row, else the repo's current branch's row.
    fn selected_ref(&self) -> Option<&RepoRef> {
        let refs = self.refs.ready()?;
        match self.config.branch.as_deref() {
            Some(name) => refs.iter().find(|r| r.name == name),
            None => refs.iter().find(|r| r.current),
        }
    }

    /// The picked (or current) ref's name.
    fn effective_ref_name(&self) -> Option<String> {
        self.config
            .branch
            .clone()
            .or_else(|| self.selected_ref().map(|r| r.name.clone()))
    }

    /// The resolved on-send checkout action for a new session. Two arms
    /// only (ADR-0007): the space folder, or a fresh worktree — the picked
    /// ref's existing worktree is never a target.
    pub fn checkout_plan(&self) -> CheckoutPlan {
        match self.config.checkout {
            CheckoutKind::NewWorktree => CheckoutPlan::NewWorktree {
                base: self.effective_ref_name(),
            },
            CheckoutKind::Local => CheckoutPlan::CurrentCheckout {
                branch: self.effective_ref_name(),
            },
        }
    }

    /// Label of the checkout-kind trigger.
    pub(super) fn checkout_label(&self) -> &'static str {
        match self.config.checkout {
            CheckoutKind::NewWorktree => "New worktree",
            CheckoutKind::Local => "Current checkout",
        }
    }

    /// Label of the ref trigger: `From <ref>` only when a NEW worktree will be
    /// created off it (t3code `getBranchTriggerLabel`); the bare name otherwise.
    pub(super) fn ref_label(&self) -> SharedString {
        match (self.config.checkout, self.effective_ref_name()) {
            (_, None) => SharedString::from("Select ref"),
            (CheckoutKind::NewWorktree, Some(name)) => SharedString::from(format!("From {name}")),
            (CheckoutKind::Local, Some(name)) => SharedString::from(name),
        }
    }

    /// The ref picker (t3code BranchToolbarBranchSelector): search on top,
    /// rows with right-aligned muted `current`/`worktree` tags, and a
    /// "Showing X of Y refs" footer when the list is capped.
    pub(super) fn render_branch_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        if self.state.read(cx).selected_space_row().is_none() {
            return div()
                .p(px(Theme::SPACE_SM))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("No project selected"))
                .into_any_element();
        }
        let rows = self.filtered_ref_rows(cx);
        let total = rows.len();
        let shown = total.min(MAX_REF_ROWS);
        // A pick safe-switches the chat's own folder (see `pick_ref`); the
        // highlighted row is [`Self::highlighted_branch`].
        let selected = self.highlighted_branch(cx);
        let switching = self.switching.clone();
        let body: AnyElement =
            match &self.refs {
                Loadable::Loading | Loadable::Idle => {
                    popover::skeleton_rows("branch-skeleton", &theme, 4, cx.entity_id(), cx)
                }
                Loadable::Error(message) => {
                    let message = message.clone();
                    self.retry_row("branch-retry", &message, PickerKind::Branch, &theme, cx)
                }
                Loadable::Ready(_) if rows.is_empty() => div()
                    .p(px(Theme::SPACE_SM))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from("No refs found."))
                    .into_any_element(),
                Loadable::Ready(_) => {
                    let active = self.active;
                    div()
                        .id("branch-list")
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .max_h(px(224.0))
                        .overflow_y_scroll()
                        .children(rows.into_iter().take(MAX_REF_ROWS).enumerate().map(
                            |(ix, row)| {
                                let label: SharedString = row.name.clone().into();
                                let is_selected = selected.as_deref() == Some(row.name.as_str());
                                // Right-aligned muted tag (t3code `text-[10px]
                                // text-muted-foreground/45`): current beats worktree.
                                let tag: Option<&'static str> = if row.current {
                                    Some("current")
                                } else if row.worktree_path.is_some() {
                                    Some("worktree")
                                } else {
                                    None
                                };
                                let is_switching = switching.as_deref() == Some(row.name.as_str());
                                popover::menu_row_nav(
                                    &theme,
                                    is_selected,
                                    ix == active,
                                    format!("branch-row-{ix}"),
                                )
                                .id(("branch-row", ix))
                                .when(switching.is_some(), |el| el.opacity(0.55))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_ref(row.clone(), cx);
                                }))
                                .child(div().flex_1().min_w_0().truncate().child(label))
                                .when(is_switching, |el| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .text_size(crate::typography::ui_rems(10.0))
                                            .text_color(theme.text_muted.opacity(0.6))
                                            .child(SharedString::from("switching…")),
                                    )
                                })
                                .when_some(tag, |el, tag| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .text_size(crate::typography::ui_rems(10.0))
                                            .text_color(theme.text_muted.opacity(0.45))
                                            .child(SharedString::from(tag)),
                                    )
                                })
                            },
                        ))
                        .into_any_element()
                }
            };
        let mut popover = div()
            .flex()
            .flex_col()
            .child(self.search_box(&theme))
            .child(body)
            .child(self.branch_create_row(&theme, cx));
        if total > shown {
            popover = popover.child(
                popover::menu_section().child(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!(
                            "Showing {shown} of {total} refs"
                        ))),
                ),
            );
        }
        popover.into_any_element()
    }

    /// The branch picker's create row: a collapsed affordance that expands
    /// into an inline name input — mid-chat too, where a create lands on
    /// the fresh branch in the chat's own working directory (ADR-0007).
    fn branch_create_row(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        if !self.branch_create_engaged {
            return popover::menu_row_nav(theme, false, false, "branch-create-row".to_string())
                .id("branch-create-row")
                .mt(px(2.0))
                .when(self.switching.is_some(), |el| el.opacity(0.55))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.branch_create_engaged = true;
                    let handle = this.branch_create.read(cx).focus_handle(cx);
                    window.focus(&handle, cx);
                    cx.notify();
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from("Create branch…")),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_muted.opacity(0.6))
                        .child(SharedString::from("+")),
                )
                .into_any_element();
        }
        let enabled = branch_create_name(self.branch_create.read(cx).text()).is_some()
            && self.switching.is_none();
        popover::menu_section()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(Theme::SPACE_SM))
            .py(px(4.0))
            .child(popover::search_input_frame(
                theme,
                self.branch_create.clone().into_any_element(),
            ))
            .child(
                div()
                    .id("branch-create-submit")
                    .flex_none()
                    .px(px(8.0))
                    .py(px(3.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .border_1()
                    .border_color(theme.border)
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(if enabled {
                        theme.text
                    } else {
                        theme.text_faint
                    })
                    .when(enabled, |el| {
                        el.cursor_pointer()
                            .hover(|state| state.bg(theme.element_hover))
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.create_branch_submit(cx)))
                    .child(SharedString::from("Create")),
            )
            .into_any_element()
    }

    /// The checkout-kind dropdown: two rows — "Current checkout" (the
    /// space's folder, always the new chat's working directory) and "New
    /// worktree".
    pub(super) fn render_checkout_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let options: [(CheckoutKind, &'static str, &'static str); 2] = [
            (
                CheckoutKind::Local,
                "Current checkout",
                crate::icons::FOLDER,
            ),
            (
                CheckoutKind::NewWorktree,
                "New worktree",
                crate::icons::FOLDER_WITH_FILES,
            ),
        ];
        let active = self.active;
        let current = self.config.checkout;
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(
                options
                    .into_iter()
                    .enumerate()
                    .map(|(ix, (kind, label, icon_path))| {
                        let is_selected = current == kind;
                        popover::menu_row_nav(
                            &theme,
                            is_selected,
                            ix == active,
                            format!("checkout-row-{ix}"),
                        )
                        .id(("checkout-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pick_checkout(kind, cx);
                        }))
                        .child(
                            crate::icons::icon(icon_path)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(label)),
                        )
                    }),
            )
            .into_any_element()
    }
}
