//! Chat manager (glossary "Chat manager"): the global page listing every
//! chat on the device — active and archived, across all spaces — with
//! search, status and space filters, and a persistent multi-selection for
//! batch archive/unarchive/delete.
//!
//! Reads ride the chats watch the AppState already holds (no new RPC);
//! writes are one Mutate op per chat, looped sequentially — the Archived
//! page's clear-all precedent. Delete is irreversible and confirmed once
//! with a breakdown; archive/unarchive are reversible and unconfirmed.

use std::collections::HashSet;

use chrono::Utc;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable as _, SharedString, Subscription,
    Task, Window, div, prelude::*, px,
};

use holt_proto::{Chat, ChatIndicator};
use holt_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::settings::widgets::{self, CheckboxState};
use crate::state::AppState;
use crate::theme::Theme;

/// Row click asks the shell to open that chat — the page has no route
/// access, navigation is the shell's job.
#[derive(Debug)]
pub enum ChatManagerEvent {
    OpenChat(String),
}

/// Which chats the list shows (glossary: status filter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusFilter {
    All,
    Active,
    Archived,
}

impl StatusFilter {
    const ALL: [StatusFilter; 3] = [Self::All, Self::Active, Self::Archived];

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Active => "Active",
            Self::Archived => "Archived",
        }
    }
}

/// What the delete confirmation reports (glossary: the confirmation's
/// breakdown). `spaces` counts DISTINCT spaces the selection touches.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DeleteBreakdown {
    pub total: usize,
    pub spaces: usize,
    pub worktrees: usize,
    pub live: usize,
}

/// Filter + sort the manager's rows. Pure. Recency desc (last message, else
/// creation), ties on id. The query matches the title or the space's display
/// name, case-insensitive; an empty query passes everything.
fn filter_chats<'a>(
    chats: &'a [Chat],
    query: &str,
    status: StatusFilter,
    space: Option<&str>,
    space_name: impl Fn(&'a Chat) -> Option<&'a str>,
) -> Vec<&'a Chat> {
    let query = query.trim().to_lowercase();
    let mut rows: Vec<&Chat> = chats
        .iter()
        .filter(|chat| match status {
            StatusFilter::All => true,
            StatusFilter::Active => !chat.archived,
            StatusFilter::Archived => chat.archived,
        })
        .filter(|chat| space.is_none_or(|id| chat.space_id.as_deref() == Some(id)))
        .filter(|chat| {
            query.is_empty()
                || chat
                    .title
                    .as_deref()
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(&query)
                || space_name(chat)
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(&query)
        })
        .collect();
    rows.sort_by(|a, b| {
        let recency = |chat: &Chat| chat.last_message_at.unwrap_or(chat.created_at);
        recency(b).cmp(&recency(a)).then_with(|| a.id.cmp(&b.id))
    });
    rows
}

/// The delete confirmation's breakdown. Pure.
fn delete_breakdown(chats: &[&Chat], live: impl Fn(&Chat) -> bool) -> DeleteBreakdown {
    let mut spaces = HashSet::new();
    let mut breakdown = DeleteBreakdown::default();
    for chat in chats {
        breakdown.total += 1;
        spaces.insert(chat.space_id.as_deref());
        if chat.worktree.is_some() {
            breakdown.worktrees += 1;
        }
        if live(chat) {
            breakdown.live += 1;
        }
    }
    breakdown.spaces = spaces.len();
    breakdown
}

/// Which batch a footer button runs. Archive skips already-archived rows and
/// Unarchive skips active ones, so a mixed selection stays meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchKind {
    Archive,
    Unarchive,
    Delete,
}

impl BatchKind {
    /// Mutate params for one chat under this batch, or None when the op
    /// does not apply to it (archive an archived chat, …).
    fn params(self, chat: &Chat) -> Option<serde_json::Value> {
        match self {
            Self::Archive if !chat.archived => Some(serde_json::json!({
                "op": "setChatArchived", "chatId": chat.id, "archived": true,
            })),
            Self::Unarchive if chat.archived => Some(serde_json::json!({
                "op": "setChatArchived", "chatId": chat.id, "archived": false,
            })),
            Self::Delete => Some(serde_json::json!({
                "op": "deleteChat", "chatId": chat.id,
            })),
            _ => None,
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Archive => "Archive",
            Self::Unarchive => "Unarchive",
            Self::Delete => "Delete",
        }
    }

    fn gerund(self) -> &'static str {
        match self {
            Self::Archive => "Archiving…",
            Self::Unarchive => "Unarchiving…",
            Self::Delete => "Deleting…",
        }
    }
}

/// One footer action's static description (keeps [`ChatManagerPage::render_action`]
/// under the argument ceiling).
struct ActionSpec {
    id: &'static str,
    label: SharedString,
    enabled: bool,
    danger: bool,
}

/// One row status pill (Working / Worktree / Archived). `live` tints it
/// accent — the rest stay neutral.
fn status_badge(theme: &Theme, label: &'static str, live: bool) -> gpui::Div {
    let (bg, fg) = if live {
        (theme.accent.opacity(0.12), theme.accent)
    } else {
        (crate::theme::ink(0.05), theme.text_muted)
    };
    div()
        .flex_none()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .bg(bg)
        .text_size(crate::typography::ui_rems(10.0))
        .text_color(fg)
        .child(SharedString::from(label))
}

pub struct ChatManagerPage {
    state: Entity<AppState>,
    search: Entity<ComposerInput>,
    status: StatusFilter,
    /// Space id the list is scoped to (None = every space).
    space: Option<String>,
    space_menu_open: bool,
    /// Persistent id set — survives filter/search changes (glossary). Ids
    /// of chats the engine has dropped are pruned at render.
    selection: HashSet<String>,
    confirm_delete: bool,
    /// The batch in flight; footer buttons disable under it.
    working: Option<BatchKind>,
    error: Option<SharedString>,
    /// Focus the search field on the first paint (opened without window access).
    focus_pending: bool,
    search_focus: FocusHandle,
    task: Option<Task<()>>,
    _observe: Subscription,
    _search_events: Subscription,
}

impl ChatManagerPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let search = cx.new(|cx| ComposerInput::new("Search sessions by title or project…", cx));
        let search_focus = search.read(cx).focus_handle(cx);
        let search_events = cx.subscribe(&search, |this, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                this.space_menu_open = false;
                cx.notify();
            }
        });
        Self {
            state,
            search,
            status: StatusFilter::All,
            space: None,
            space_menu_open: false,
            selection: HashSet::new(),
            confirm_delete: false,
            working: None,
            error: None,
            focus_pending: true,
            search_focus,
            task: None,
            _observe: observe,
            _search_events: search_events,
        }
    }

    /// The rows the current filter/search produces, cloned out of the state.
    fn filtered_rows(&self, cx: &App) -> Vec<Chat> {
        let state = self.state.read(cx);
        let query = self.search.read(cx).text();
        filter_chats(
            &state.chats,
            query,
            self.status,
            self.space.as_deref(),
            |chat| state.space_for_chat(chat).map(|space| space.display_name()),
        )
        .into_iter()
        .cloned()
        .collect()
    }

    /// Chats currently in the selection (vanishing ids are already pruned
    /// by render; a read between frames just skips them).
    fn selected_rows(&self, cx: &App) -> Vec<Chat> {
        self.state
            .read(cx)
            .chats
            .iter()
            .filter(|chat| self.selection.contains(&chat.id))
            .cloned()
            .collect()
    }

    fn toggle(&mut self, chat_id: String, cx: &mut Context<Self>) {
        if !self.selection.remove(&chat_id) {
            self.selection.insert(chat_id);
        }
        cx.notify();
    }

    /// Select all covers the currently FILTERED list (glossary).
    fn select_all_filtered(&mut self, cx: &mut Context<Self>) {
        self.selection
            .extend(self.filtered_rows(cx).into_iter().map(|chat| chat.id));
        cx.notify();
    }

    /// Run one batch: one Mutate per applicable chat, sequentially, stopping
    /// at the first failure — the Archived page's clear-all precedent. On
    /// success the selection empties; on failure it survives so the pass
    /// can be retried — rows already applied drop out on their own
    /// (`params` skips no-ops, render prunes deleted chats).
    fn run_batch(&mut self, kind: BatchKind, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params: Vec<serde_json::Value> = self
            .selected_rows(cx)
            .iter()
            .filter_map(|chat| kind.params(chat))
            .collect();
        if params.is_empty() {
            return;
        }
        self.confirm_delete = false;
        self.working = Some(kind);
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let mut failure = None;
            for param in params {
                if let Err(err) = engine.client().call(methods::MUTATE, param).await {
                    failure = Some(err.to_string());
                    break;
                }
            }
            this.update(cx, |page, cx| {
                page.batch_settled(kind, failure, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    /// A pass settled: the busy flag always clears, the selection empties
    /// only on success, a failure surfaces its error.
    fn batch_settled(&mut self, kind: BatchKind, failure: Option<String>, cx: &mut Context<Self>) {
        self.working = None;
        if failure.is_none() {
            self.selection.clear();
        }
        if let Some(err) = failure {
            self.error = Some(format!("{} failed: {err}", kind.verb()).into());
        }
        cx.notify();
    }

    fn set_status(&mut self, status: StatusFilter, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    fn set_space(&mut self, space: Option<String>, cx: &mut Context<Self>) {
        self.space = space;
        self.space_menu_open = false;
        cx.notify();
    }

    /// One filter segment (All / Active / Archived) in the toolbar pill.
    fn render_segment(
        &self,
        theme: &Theme,
        status: StatusFilter,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let active = self.status == status;
        div()
            .id(("cm-status", ix))
            .flex_none()
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(6.0))
            .text_size(crate::typography::ui_rems(12.0))
            .font_weight(if active {
                gpui::FontWeight::MEDIUM
            } else {
                gpui::FontWeight::NORMAL
            })
            .text_color(if active { theme.text } else { theme.text_muted })
            .when(active, |el| el.bg(theme.surface_raised))
            .cursor_pointer()
            .hover(|s| s.text_color(theme.text))
            .on_click(cx.listener(move |this, _, _, cx| this.set_status(status, cx)))
            .child(status.label())
    }

    /// The space filter: trigger naming the current pick + a dropdown card
    /// ("All spaces" then every space, checked on the active one).
    fn render_space_filter(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let label: SharedString = match self.space.as_deref() {
            Some(id) => match self.state.read(cx).space_row(id) {
                Some(space) => space.display_name().to_string().into(),
                None => "All spaces".into(),
            },
            None => "All spaces".into(),
        };
        let trigger = div()
            .id("cm-space-filter")
            .flex_none()
            .h(px(28.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .px(px(10.0))
            .max_w(px(220.0))
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|s| {
                s.text_color(theme.text)
                    .bg(theme.surface_raised.opacity(0.5))
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.space_menu_open = !this.space_menu_open;
                cx.notify();
            }))
            .child(
                crate::icons::icon(crate::icons::FOLDER)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(div().flex_1().min_w_0().truncate().child(label))
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.7)),
            );
        if !self.space_menu_open {
            return trigger;
        }
        // The card lists "All spaces" plus every space in display order;
        // a check marks the active pick. Click-outside closes.
        let active = self.space.clone();
        let mut rows = div().flex().flex_col().gap(px(2.0)).p(px(4.0));
        let options: Vec<(Option<String>, SharedString)> = {
            let state = self.state.read(cx);
            std::iter::once((None, SharedString::from("All spaces")))
                .chain(state.spaces_sorted().into_iter().map(|space| {
                    (
                        Some(space.id.clone()),
                        SharedString::from(space.display_name().to_string()),
                    )
                }))
                .collect()
        };
        for (ix, (id, name)) in options.into_iter().enumerate() {
            let checked = id == active;
            let row_id = id.clone();
            rows = rows.child(
                div()
                    .id(("cm-space-row", ix))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(6.0))
                    .px(px(8.0))
                    .py(px(6.0))
                    .text_color(if checked {
                        theme.text
                    } else {
                        theme.text_muted
                    })
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.surface_raised).text_color(theme.text))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_space(row_id.clone(), cx);
                    }))
                    .child(div().flex_none().size(px(12.0)).children(checked.then(|| {
                        crate::icons::icon(crate::icons::CHECK)
                            .size(px(12.0))
                            .text_color(theme.accent)
                    })))
                    .child(div().flex_1().min_w_0().truncate().child(name)),
            );
        }
        let card = div()
            .id("cm-space-menu-card")
            .occlude()
            .w(px(240.0))
            .max_h(px(320.0))
            .overflow_y_scroll()
            .rounded(px(10.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface_dialog)
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.space_menu_open = false;
                cx.notify();
            }))
            .child(rows)
            .into_any_element();
        trigger
            .relative()
            .child(crate::popover::anchored_menu_below(
                "cm-space-menu",
                card,
                None,
            ))
    }

    /// One chat row: checkbox, title + time, space + status badges.
    /// Clicking an ACTIVE row opens the chat; an archived chat has no
    /// viewing surface (the watch snapshot drops an archived selection), so
    /// clicking an archived row toggles its selection instead. The checkbox
    /// has its own stop-propagating hit either way.
    fn render_row(
        &self,
        theme: &Theme,
        chat: &Chat,
        live: bool,
        ix: usize,
        now: chrono::DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selection.contains(&chat.id);
        let title: SharedString = chat
            .title
            .clone()
            .unwrap_or_else(|| "Untitled session".into())
            .into();
        let time_ago: SharedString =
            crate::state::format_time_ago(chat.last_message_at.unwrap_or(chat.created_at), now)
                .into();
        let space_name: Option<SharedString> = self
            .state
            .read(cx)
            .space_for_chat(chat)
            .map(|space| space.display_name().to_string().into());
        let chat_id = chat.id.clone();
        let toggle_id = chat.id.clone();
        let archived = chat.archived;

        let checkbox = widgets::checkbox(
            theme,
            if selected {
                CheckboxState::Checked
            } else {
                CheckboxState::Unchecked
            },
        )
        .id(("cm-check", ix))
        .debug_selector(move || format!("cm-check-{ix}"))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.toggle(toggle_id.clone(), cx);
        }));

        let mut badges = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0));
        if live {
            badges = badges.child(status_badge(theme, "Working", true));
        }
        if chat.worktree.is_some() {
            badges = badges.child(status_badge(theme, "Worktree", false));
        }
        if chat.archived {
            badges = badges.child(status_badge(theme, "Archived", false));
        }

        div()
            .id(("cm-row", ix))
            .debug_selector(move || format!("cm-row-{ix}"))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .rounded(px(8.0))
            .px(px(12.0))
            .py(px(8.0))
            .cursor_pointer()
            .hover(|s| s.bg(crate::theme::ink(0.03)))
            .on_click(cx.listener(move |this, _, _, cx| {
                if archived {
                    this.toggle(chat_id.clone(), cx);
                } else {
                    cx.emit(ChatManagerEvent::OpenChat(chat_id.clone()));
                    cx.notify();
                }
            }))
            .child(checkbox)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(crate::typography::ui_rems(13.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.text_muted.opacity(0.5))
                                    .child(time_ago),
                            ),
                    )
                    .child(
                        div()
                            .mt(px(2.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_muted.opacity(0.55))
                            .when_some(space_name, |meta, name| {
                                meta.child(div().min_w_0().truncate().child(name))
                            }),
                    ),
            )
            .child(badges)
            .into_any_element()
    }

    /// One footer action button; disabled styling + no click while a batch
    /// runs or the button has nothing to act on.
    fn render_action(
        &self,
        theme: &Theme,
        spec: ActionSpec,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let ActionSpec {
            id,
            label,
            enabled,
            danger,
        } = spec;
        let busy = self.working.is_some();
        let active = enabled && !busy;
        let text_color = if !active {
            theme.text_muted.opacity(0.4)
        } else if danger {
            theme.danger_muted
        } else {
            theme.text_muted
        };
        let border_color = if danger && active {
            theme.danger.opacity(0.25)
        } else {
            theme.border
        };
        let button = div()
            .id(id)
            .debug_selector(move || id.to_string())
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(border_color)
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(text_color)
            .child(label);
        if !active {
            return button;
        }
        button
            .cursor_pointer()
            .hover(move |s| {
                if danger {
                    s.bg(theme.danger.opacity(0.08)).text_color(theme.danger)
                } else {
                    s.bg(theme.surface_raised).text_color(theme.text)
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
    }
}

impl gpui::EventEmitter<ChatManagerEvent> for ChatManagerPage {}

impl Render for ChatManagerPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        if std::mem::take(&mut self.focus_pending) {
            window.focus(&self.search_focus, cx);
        }
        // Prune ids of chats the engine has dropped (deleted elsewhere,
        // device sync) — the selection never acts on ghosts.
        {
            let state = self.state.read(cx);
            let existing: HashSet<&str> = state.chats.iter().map(|chat| chat.id.as_str()).collect();
            self.selection.retain(|id| existing.contains(id.as_str()));
            // A filter scoped to a space the engine no longer has would
            // show an empty list under an "All spaces" label — fall back.
            if self
                .space
                .as_deref()
                .is_some_and(|id| state.space_row(id).is_none())
            {
                self.space = None;
            }
        }

        let rows = self.filtered_rows(cx);
        // A live Turn (glossary): Working or AwaitingInput — both die with
        // the chat on delete, so both count in the confirmation.
        let live: HashSet<String> = {
            let state = self.state.read(cx);
            state
                .chats
                .iter()
                .filter(|chat| {
                    matches!(
                        state.display_status_for(chat, now),
                        ChatIndicator::Working | ChatIndicator::AwaitingInput
                    )
                })
                .map(|chat| chat.id.clone())
                .collect()
        };
        let selected = self.selected_rows(cx);
        let selected_count = selected.len();
        let any_active = selected.iter().any(|chat| !chat.archived);
        let any_archived = selected.iter().any(|chat| chat.archived);
        let filtered_ids: HashSet<&str> = rows.iter().map(|chat| chat.id.as_str()).collect();
        let all_filtered_selected =
            !rows.is_empty() && rows.iter().all(|chat| self.selection.contains(&chat.id));
        let total_count = self.state.read(cx).chats.len();

        let items: Vec<AnyElement> = rows
            .iter()
            .enumerate()
            .map(|(ix, chat)| self.render_row(&theme, chat, live.contains(&chat.id), ix, now, cx))
            .collect();

        let body: AnyElement = if items.is_empty() {
            div()
                .mt(px(96.0))
                .flex()
                .flex_col()
                .items_center()
                .text_center()
                .text_color(theme.text_muted.opacity(0.5))
                .child(
                    crate::icons::icon(crate::icons::CHECKLIST)
                        .size(px(28.0))
                        .text_color(theme.text_muted.opacity(0.2)),
                )
                .child(
                    div()
                        .mt(px(12.0))
                        .text_size(crate::typography::ui_rems(14.0))
                        .child(SharedString::from(if total_count == 0 {
                            "No sessions yet"
                        } else {
                            "No sessions match"
                        })),
                )
                .child(
                    div()
                        .mt(px(4.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted.opacity(0.4))
                        .child(SharedString::from(if total_count == 0 {
                            "New sessions you start will show up here."
                        } else {
                            "Try a different search or filter."
                        })),
                )
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .pt(px(8.0))
                .children(items)
                .into_any_element()
        };

        // Toolbar: search + status segments + space filter.
        let search = self.search.clone();
        let toolbar = div()
            .mt(px(16.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(crate::popover::search_input_frame(
                        &theme,
                        search.into_any_element(),
                    )),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(2.0))
                    .p(px(2.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.border)
                    .children(
                        StatusFilter::ALL
                            .into_iter()
                            .enumerate()
                            .map(|(ix, status)| self.render_segment(&theme, status, ix, cx)),
                    ),
            )
            .child(self.render_space_filter(&theme, cx));

        // Footer action bar: the selection count on the left, the batch
        // actions on the right. Archive/Unarchive enable off the selection's
        // contents; Delete always asks once.
        let working = self.working;
        let count_label: SharedString = match working {
            Some(kind) => kind.gerund().into(),
            None => {
                if selected_count == 0 {
                    "Select sessions to manage".into()
                } else if selected_count == 1 {
                    "1 selected".into()
                } else {
                    format!("{selected_count} selected").into()
                }
            }
        };
        let footer = div()
            .px(px(24.0))
            .py(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child(count_label),
            )
            .child(div().flex_1())
            .child(self.render_action(
                &theme,
                ActionSpec {
                    id: "cm-select-all",
                    label: if all_filtered_selected {
                        "All selected".into()
                    } else {
                        let n = filtered_ids.len();
                        if n == 0 {
                            "Select all".into()
                        } else {
                            format!("Select all ({n})").into()
                        }
                    },
                    enabled: !rows.is_empty() && !all_filtered_selected,
                    danger: false,
                },
                |this, cx| this.select_all_filtered(cx),
                cx,
            ))
            .child(self.render_action(
                &theme,
                ActionSpec {
                    id: "cm-clear",
                    label: "Clear".into(),
                    enabled: selected_count > 0,
                    danger: false,
                },
                |this, cx| {
                    this.selection.clear();
                    cx.notify();
                },
                cx,
            ))
            .child(self.render_action(
                &theme,
                ActionSpec {
                    id: "cm-archive",
                    label: "Archive".into(),
                    enabled: any_active,
                    danger: false,
                },
                |this, cx| this.run_batch(BatchKind::Archive, cx),
                cx,
            ))
            .child(self.render_action(
                &theme,
                ActionSpec {
                    id: "cm-unarchive",
                    label: "Unarchive".into(),
                    enabled: any_archived,
                    danger: false,
                },
                |this, cx| this.run_batch(BatchKind::Unarchive, cx),
                cx,
            ))
            .child(self.render_action(
                &theme,
                ActionSpec {
                    id: "cm-delete",
                    label: "Delete…".into(),
                    enabled: selected_count > 0,
                    danger: true,
                },
                |this, cx| {
                    this.confirm_delete = true;
                    cx.notify();
                },
                cx,
            ));

        // The delete confirmation: irreversible, once, with the breakdown
        // the glossary names (spaces, worktrees, live turns).
        let confirm_dialog = self.confirm_delete.then(|| {
            let refs: Vec<&Chat> = selected.iter().collect();
            let breakdown = delete_breakdown(&refs, |chat| live.contains(&chat.id));
            let mut detail = format!("Across {} space{}", breakdown.spaces, if breakdown.spaces == 1 { "" } else { "s" });
            if breakdown.worktrees > 0 {
                detail.push_str(&format!(
                    " · {} session worktree{}",
                    breakdown.worktrees,
                    if breakdown.worktrees == 1 { "" } else { "s" },
                ));
            }
            if breakdown.live > 0 {
                detail.push_str(&format!(" · {} with a live turn", breakdown.live,));
            }
            let copy = format!(
                "{} will be permanently deleted. Their terminals and running programs will also end. This can\u{2019}t be undone.",
                if breakdown.total == 1 {
                    "1 session".to_string()
                } else {
                    format!("{} sessions", breakdown.total)
                },
            );
            let card = crate::popover::dialog_card(&theme)
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.confirm_delete = false;
                    cx.notify();
                }))
                .child(crate::popover::dialog_title(
                    &theme,
                    if breakdown.total == 1 {
                        "Delete 1 session?"
                    } else {
                        "Delete sessions?"
                    },
                ))
                .child(
                    div()
                        .mt(px(6.0))
                        .child(crate::popover::dialog_body(&theme, copy)),
                )
                .child(
                    div()
                        .mt(px(6.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted.opacity(0.7))
                        .child(detail),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            crate::popover::btn_ghost(&theme, "Cancel", "cm-delete-cancel")
                                .id("cm-delete-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.confirm_delete = false;
                                    cx.notify();
                                })),
                        )
                        .child(
                            crate::popover::btn_danger(
                                &theme,
                                if breakdown.total == 1 {
                                    "Delete"
                                } else {
                                    "Delete all"
                                },
                            )
                            .id("cm-delete-confirm")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.run_batch(BatchKind::Delete, cx)
                            })),
                        ),
                )
                .into_any_element();
            crate::popover::modal("cm-delete-dialog", window.viewport_size(), card)
        });

        div()
            .id("chat-manager-page")
            .debug_selector(|| "chat-manager-page".into())
            // Escape cancels the delete confirmation; stop_propagation
            // keeps outer Esc surfaces out of the key.
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                if this.confirm_delete && ev.keystroke.key == "escape" {
                    this.confirm_delete = false;
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .w_full()
                    .max_w(px(960.0))
                    .mx_auto()
                    .px(px(24.0))
                    .pt(px(32.0))
                    .flex()
                    .flex_col()
                    .child(widgets::page_header(
                        &theme,
                        "Chat manager",
                        (total_count > 0).then_some(total_count),
                    ))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Every session on this device, active and archived. Select rows to archive, unarchive, or delete them in bulk.",
                    ))
                    .when_some(self.error.clone(), |el, message| {
                        el.child(
                            widgets::error_strip(&theme, message)
                                .id("cm-error")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(toolbar),
            )
            .child(
                div()
                    .id("cm-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .w_full()
                            .max_w(px(960.0))
                            .mx_auto()
                            .px(px(24.0))
                            .pb(px(24.0))
                            .child(body),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .w_full()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .w_full()
                            .max_w(px(960.0))
                            .mx_auto()
                            .child(footer),
                    ),
            )
            .children(confirm_dialog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn chat(id: &str) -> Chat {
        Chat {
            id: id.into(),
            device_id: "d".into(),
            title: None,
            title_source: Default::default(),
            title_task_started: false,
            archived: false,
            pinned: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            source_context: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            space_id: None,
            last_seen_at: None,
            room_gen: None,
            compact_before_next_turn: false,
            plan_mode: None,
            worktree: None,
            provider_mode: false,
        }
    }

    fn named(id: &str, title: &str, space: &str) -> Chat {
        let mut chat = chat(id);
        chat.title = Some(title.into());
        chat.space_id = Some(space.into());
        chat
    }

    fn no_space(_: &Chat) -> Option<&str> {
        None
    }

    #[test]
    fn filter_by_status() {
        let mut archived = chat("b");
        archived.archived = true;
        let chats = vec![chat("a"), archived, chat("c")];
        let rows = filter_chats(&chats, "", StatusFilter::Archived, None, no_space);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "b");
        let rows = filter_chats(&chats, "", StatusFilter::Active, None, no_space);
        assert_eq!(rows.len(), 2);
        let rows = filter_chats(&chats, "", StatusFilter::All, None, no_space);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn filter_by_space() {
        let chats = vec![named("a", "x", "s1"), named("b", "y", "s2"), chat("c")];
        let rows = filter_chats(&chats, "", StatusFilter::All, Some("s1"), no_space);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "a");
        // A dangling filter (space deleted) matches nothing rather than everything.
        let rows = filter_chats(&chats, "", StatusFilter::All, Some("gone"), no_space);
        assert!(rows.is_empty());
    }

    #[test]
    fn query_matches_title_and_space_name_case_insensitively() {
        let chats = vec![named("a", "Fix the Bug", "s1"), named("b", "other", "s2")];
        let rows = filter_chats(&chats, "bug", StatusFilter::All, None, no_space);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "a");
        let rows = filter_chats(&chats, "HOLT", StatusFilter::All, None, |chat| {
            match chat.space_id.as_deref() {
                Some("s2") => Some("holt repo"),
                _ => None,
            }
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "b");
    }

    #[test]
    fn rows_sort_by_recency_desc_with_id_tiebreak() {
        let now = Utc::now();
        let mut a = chat("a");
        a.created_at = now - Duration::days(2);
        let mut b = chat("b");
        b.created_at = now - Duration::days(5);
        b.last_message_at = Some(now - Duration::days(1));
        let mut c = chat("c");
        c.created_at = now - Duration::days(5);
        c.last_message_at = Some(now - Duration::days(1));
        let chats = vec![a, c, b];
        let rows = filter_chats(&chats, "", StatusFilter::All, None, no_space);
        let ids: Vec<&str> = rows.iter().map(|chat| chat.id.as_str()).collect();
        assert_eq!(ids, ["b", "c", "a"]);
    }

    #[test]
    fn breakdown_counts_distinct_spaces_worktrees_and_live_turns() {
        let mut a = named("a", "x", "s1");
        a.worktree = Some(holt_proto::WorktreeSpec {
            repo_path: "/repo".into(),
            base: "main".into(),
        });
        let b = named("b", "y", "s1");
        let c = named("c", "z", "s2");
        let chats = [a, b, c];
        let refs: Vec<&Chat> = chats.iter().collect();
        let breakdown = delete_breakdown(&refs, |chat| chat.id == "c");
        assert_eq!(
            breakdown,
            DeleteBreakdown {
                total: 3,
                spaces: 2,
                worktrees: 1,
                live: 1,
            }
        );
    }

    #[test]
    fn batch_params_skip_rows_the_op_does_not_apply_to() {
        let mut archived = chat("a");
        archived.archived = true;
        let active = chat("b");
        assert!(BatchKind::Archive.params(&archived).is_none());
        assert!(BatchKind::Archive.params(&active).is_some());
        assert!(BatchKind::Unarchive.params(&archived).is_some());
        assert!(BatchKind::Unarchive.params(&active).is_none());
        assert!(BatchKind::Delete.params(&archived).is_some());
        assert_eq!(
            BatchKind::Delete.params(&active).unwrap(),
            serde_json::json!({ "op": "deleteChat", "chatId": "b" }),
        );
    }

    /// Smoke: the page renders its rows, a checkbox click selects, the
    /// footer count follows, and clicking an ARCHIVED row selects rather
    /// than navigating (an archived chat has no viewing surface).
    #[gpui::test]
    fn rows_render_and_selection_wires(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::default());
            crate::settings::init(crate::settings::UiSettings::default(), dir.path(), cx);
        });
        let state = cx.new(|_| {
            let mut state = AppState::new();
            let mut archived = chat("b");
            archived.archived = true;
            state.chats = vec![chat("a"), archived];
            state
        });
        let (page, visual) =
            cx.add_window_view(|_window, cx| ChatManagerPage::new(state.clone(), cx));
        // A window only paints after a notify — force the first frame.
        page.update(&mut *visual, |_page, cx| cx.notify());
        visual.run_until_parked();

        visual
            .debug_bounds("chat-manager-page")
            .expect("page root renders");
        visual.debug_bounds("cm-delete").expect("footer renders");
        visual.debug_bounds("cm-row-0").expect("first row renders");
        visual.debug_bounds("cm-row-1").expect("second row renders");

        // The row-0 checkbox selects; the footer count follows.
        let point = visual.debug_bounds("cm-check-0").unwrap().center();
        visual.simulate_click(point, Default::default());
        visual.run_until_parked();
        page.read_with(&*visual, |page, _| {
            assert_eq!(page.selection.len(), 1);
        });
        visual
            .debug_bounds("cm-delete")
            .expect("delete button renders once the selection is live");

        // Clicking the archived row toggles its selection instead of
        // opening a chat the watch snapshot would drop.
        let point = visual.debug_bounds("cm-row-1").unwrap().center();
        visual.simulate_click(point, Default::default());
        visual.run_until_parked();
        page.read_with(&*visual, |page, _| {
            assert_eq!(page.selection.len(), 2);
        });

        // A space filter whose space no longer resolves falls back to all
        // spaces instead of an empty list labelled "All spaces".
        page.update(&mut *visual, |page, cx| {
            page.space = Some("gone".into());
            cx.notify();
        });
        visual.run_until_parked();
        page.read_with(&*visual, |page, _| {
            assert_eq!(page.space, None);
        });
    }

    /// A settled batch empties the selection only on success — a failure
    /// keeps it so the pass can be retried.
    #[gpui::test]
    fn failed_batch_keeps_selection(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::default());
            crate::settings::init(crate::settings::UiSettings::default(), dir.path(), cx);
        });
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.chats = vec![chat("a"), chat("b")];
            state
        });
        let (page, visual) =
            cx.add_window_view(|_window, cx| ChatManagerPage::new(state.clone(), cx));
        page.update(&mut *visual, |page, cx| {
            page.selection.insert("a".into());
            page.selection.insert("b".into());
            page.batch_settled(BatchKind::Archive, Some("engine gone".into()), cx);
            assert_eq!(page.selection.len(), 2);
            assert!(page.error.is_some());
            assert_eq!(page.working, None);
            page.batch_settled(BatchKind::Archive, None, cx);
            assert!(page.selection.is_empty());
        });
    }
}
