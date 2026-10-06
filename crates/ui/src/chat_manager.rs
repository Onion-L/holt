//! Chat manager (glossary "Chat manager"): the global page listing every
//! chat on the device — active and archived, across all spaces — with
//! search, status and space filters, and a persistent multi-selection for
//! batch archive/unarchive/delete.
//!
//! Interaction follows a file browser: a row click toggles the row in the
//! selection (shift-click extends a range from the last click), a double
//! click opens an active chat, and the hover actions act on one row. Batch
//! actions live in a floating bar that only exists while something is
//! selected. ⌘A selects the filtered list, ⌘⌫ asks to delete the
//! selection, Esc backs out one layer.
//!
//! Reads ride the chats watch the AppState already holds (no new RPC);
//! writes are one Mutate op per chat, looped sequentially — the Archived
//! page's clear-all precedent. Delete is irreversible and confirmed once
//! with a breakdown; archive/unarchive are reversible and unconfirmed.

use std::collections::HashSet;

use chrono::{DateTime, Local, NaiveDate, Utc};
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, SharedString, Subscription, Task, Window, div,
    prelude::*, px,
};

use holt_proto::{Chat, ChatIndicator};
use holt_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::popover::{self, Popup};
use crate::settings::widgets::{self, CheckboxState};
use crate::state::AppState;
use crate::theme::{Theme, hairline, ink};

/// Column widths shared by the list header and the rows so they align.
const PROJECT_COL: f32 = 160.0;
const TRAILING_COL: f32 = 84.0;
const ROW_HEIGHT: f32 = 36.0;
const PAGE_MAX_W: f32 = 960.0;

/// Row double-click asks the shell to open that chat — the page has no
/// route access, navigation is the shell's job.
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

/// Recency section a row sorts under, by local calendar day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bucket {
    Today,
    Yesterday,
    Week,
    Month,
    Older,
}

impl Bucket {
    fn of(day: NaiveDate, today: NaiveDate) -> Self {
        match (today - day).num_days() {
            ..=0 => Self::Today,
            1 => Self::Yesterday,
            2..=6 => Self::Week,
            7..=29 => Self::Month,
            _ => Self::Older,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::Yesterday => "Yesterday",
            Self::Week => "Previous 7 days",
            Self::Month => "Previous 30 days",
            Self::Older => "Older",
        }
    }
}

fn recency(chat: &Chat) -> DateTime<Utc> {
    chat.last_message_at.unwrap_or(chat.created_at)
}

/// What the delete confirmation reports (glossary: the confirmation's
/// breakdown). `spaces` counts DISTINCT spaces the targets touch.
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
    rows.sort_by(|a, b| recency(b).cmp(&recency(a)).then_with(|| a.id.cmp(&b.id)));
    rows
}

/// Ids from `anchor` to `target` inclusive, in display order — the
/// shift-click range. None when either has left the list. Pure.
fn range_ids(rows: &[Chat], anchor: &str, target: &str) -> Option<Vec<String>> {
    let a = rows.iter().position(|chat| chat.id == anchor)?;
    let b = rows.iter().position(|chat| chat.id == target)?;
    let (lo, hi) = (a.min(b), a.max(b));
    Some(rows[lo..=hi].iter().map(|chat| chat.id.clone()).collect())
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

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// Which batch an action runs. Archive skips already-archived rows and
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

/// One-line hover help on the page's icon buttons.
struct Hint(SharedString);

impl Render for Hint {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border_strong)
            .bg(theme.surface_raised)
            .shadow_md()
            .text_size(crate::typography::ui_rems(11.0))
            .text_color(theme.text)
            .child(self.0.clone())
    }
}

fn with_hint(el: gpui::Stateful<gpui::Div>, hint: &'static str) -> gpui::Stateful<gpui::Div> {
    el.tooltip(move |_, cx| cx.new(|_| Hint(hint.into())).into())
        .tooltip_show_delay(std::time::Duration::from_millis(350))
}

/// Small neutral tag after a row title (Worktree / Archived).
fn tag(theme: &Theme, label: &'static str) -> gpui::Div {
    div()
        .flex_none()
        .h(px(16.0))
        .px(px(5.0))
        .flex()
        .items_center()
        .rounded(px(4.0))
        .bg(ink(0.06))
        .text_size(crate::typography::ui_rems(10.0))
        .text_color(theme.text_muted)
        .child(SharedString::from(label))
}

/// A 24px square icon button. The name doubles as id and hover group — gpui
/// Svgs only paint their own text color, so the glyph brightens through
/// `group_hover`. Caller adds the click.
fn icon_button(
    theme: &Theme,
    name: SharedString,
    path: &'static str,
    danger: bool,
) -> gpui::Stateful<gpui::Div> {
    let (wash, hover_fg) = if danger {
        (theme.danger.opacity(0.12), theme.danger)
    } else {
        (ink(0.08), theme.text)
    };
    div()
        .id(name.clone())
        .group(name.clone())
        .flex_none()
        .size(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(move |s| s.bg(wash))
        .child(
            icon(path)
                .size(px(14.0))
                .text_color(theme.text_muted)
                .group_hover(name, move |s| s.text_color(hover_fg)),
        )
}

/// One action in the floating selection bar.
struct BarAction {
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    danger: bool,
}

pub struct ChatManagerPage {
    state: Entity<AppState>,
    search: Entity<ComposerInput>,
    status: StatusFilter,
    /// Space id the list is scoped to (None = every space).
    space: Option<String>,
    space_menu: Popup<()>,
    /// Persistent id set — survives filter/search changes (glossary). Ids
    /// of chats the engine has dropped are pruned at render.
    selection: HashSet<String>,
    /// Last plain-clicked row — the fixed end of a shift-click range.
    anchor: Option<String>,
    /// Chats the open delete confirmation targets (the selection, or one
    /// row's hover action).
    confirm: Option<Vec<String>>,
    /// The batch in flight; actions disable under it.
    working: Option<BatchKind>,
    error: Option<SharedString>,
    /// The page root's focus — keyboard shortcuts land here.
    focus: FocusHandle,
    /// Take focus on the next paint (opened without window access).
    focus_pending: bool,
    task: Option<Task<()>>,
    _observe: Subscription,
    _search_events: Subscription,
}

impl ChatManagerPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let search = cx.new(|cx| ComposerInput::new("Search sessions", cx));
        let search_events = cx.subscribe(&search, |_, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                cx.notify();
            }
        });
        Self {
            state,
            search,
            status: StatusFilter::All,
            space: None,
            space_menu: Popup::default(),
            selection: HashSet::new(),
            anchor: None,
            confirm: None,
            working: None,
            error: None,
            focus: cx.focus_handle(),
            focus_pending: true,
            task: None,
            _observe: observe,
            _search_events: search_events,
        }
    }

    /// The shell shows the page again — re-land keyboard focus on it.
    pub fn reveal(&mut self, cx: &mut Context<Self>) {
        self.focus_pending = true;
        cx.notify();
    }

    /// The rows a status filter produces under the current search + space.
    fn rows_for(&self, status: StatusFilter, cx: &App) -> Vec<Chat> {
        let state = self.state.read(cx);
        filter_chats(
            &state.chats,
            self.search.read(cx).text(),
            status,
            self.space.as_deref(),
            |chat| state.space_for_chat(chat).map(|space| space.display_name()),
        )
        .into_iter()
        .cloned()
        .collect()
    }

    fn filtered_rows(&self, cx: &App) -> Vec<Chat> {
        self.rows_for(self.status, cx)
    }

    fn chats_by_id(&self, ids: &HashSet<&str>, cx: &App) -> Vec<Chat> {
        self.state
            .read(cx)
            .chats
            .iter()
            .filter(|chat| ids.contains(chat.id.as_str()))
            .cloned()
            .collect()
    }

    fn selected_rows(&self, cx: &App) -> Vec<Chat> {
        let ids: HashSet<&str> = self.selection.iter().map(String::as_str).collect();
        self.chats_by_id(&ids, cx)
    }

    /// A row click. Plain toggles and re-anchors; shift adds the range from
    /// the anchor. A double click opens an active chat — its first click
    /// already toggled, so the second undoes that and the selection ends
    /// where it started. An archived chat has no viewing surface (the watch
    /// snapshot drops an archived selection), so it never opens.
    fn click_row(
        &mut self,
        chat_id: &str,
        event: &gpui::ClickEvent,
        archived: bool,
        cx: &mut Context<Self>,
    ) {
        if event.click_count() >= 2 {
            if !archived {
                self.toggle(chat_id);
                cx.emit(ChatManagerEvent::OpenChat(chat_id.to_string()));
            }
            cx.notify();
            return;
        }
        let range = event
            .modifiers()
            .shift
            .then(|| {
                let anchor = self.anchor.clone()?;
                range_ids(&self.filtered_rows(cx), &anchor, chat_id)
            })
            .flatten();
        match range {
            Some(ids) => self.selection.extend(ids),
            None => {
                self.toggle(chat_id);
                self.anchor = Some(chat_id.to_string());
            }
        }
        cx.notify();
    }

    fn toggle(&mut self, chat_id: &str) {
        if !self.selection.remove(chat_id) {
            self.selection.insert(chat_id.to_string());
        }
    }

    /// The header checkbox: select the whole filtered list, or — when it is
    /// already all selected — drop it from the selection.
    fn toggle_all_filtered(&mut self, cx: &mut Context<Self>) {
        let rows = self.filtered_rows(cx);
        if !rows.is_empty() && rows.iter().all(|chat| self.selection.contains(&chat.id)) {
            for chat in &rows {
                self.selection.remove(&chat.id);
            }
        } else {
            self.selection.extend(rows.into_iter().map(|chat| chat.id));
        }
        cx.notify();
    }

    fn select_all_filtered(&mut self, cx: &mut Context<Self>) {
        self.selection
            .extend(self.filtered_rows(cx).into_iter().map(|chat| chat.id));
        cx.notify();
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.selection.clear();
        self.anchor = None;
        cx.notify();
    }

    fn ask_delete(&mut self, ids: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        if ids.is_empty() || self.working.is_some() {
            return;
        }
        self.confirm = Some(ids);
        // Enter / Esc answer the dialog through the root's key handler.
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Run one batch over `ids`: one Mutate per applicable chat,
    /// sequentially, stopping at the first failure — the Archived page's
    /// clear-all precedent.
    fn run(&mut self, kind: BatchKind, ids: Vec<String>, cx: &mut Context<Self>) {
        if self.working.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params: Vec<serde_json::Value> = {
            let lookup: HashSet<&str> = ids.iter().map(String::as_str).collect();
            self.chats_by_id(&lookup, cx)
                .iter()
                .filter_map(|chat| kind.params(chat))
                .collect()
        };
        if params.is_empty() {
            return;
        }
        self.confirm = None;
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
            this.update(cx, |page, cx| page.batch_settled(kind, &ids, failure, cx))
                .ok();
        }));
        cx.notify();
    }

    /// A pass settled: the busy flag always clears. On success the targets
    /// leave the selection; on failure the selection survives so the pass
    /// can be retried — rows already applied drop out on their own
    /// (`params` skips no-ops, render prunes deleted chats).
    fn batch_settled(
        &mut self,
        kind: BatchKind,
        ids: &[String],
        failure: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.working = None;
        match failure {
            None => {
                for id in ids {
                    self.selection.remove(id);
                }
                if self.selection.is_empty() {
                    self.anchor = None;
                }
            }
            Some(err) => self.error = Some(format!("{} failed: {err}", kind.verb()).into()),
        }
        cx.notify();
    }

    fn set_space(&mut self, space: Option<String>, cx: &mut Context<Self>) {
        self.space = space;
        self.close_space_menu(cx);
        cx.notify();
    }

    fn close_space_menu(&mut self, cx: &mut Context<Self>) {
        if self.space_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.space_menu);
            cx.notify();
        }
    }

    /// Page-level keys. With the delete confirmation up, Enter confirms and
    /// Esc cancels; otherwise Esc closes the space menu, then clears the
    /// selection. ⌘A / ⌘⌫ only reach here when the search field is not
    /// focused (the input's own bindings win there).
    fn on_key(&mut self, event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = &event.keystroke.modifiers;
        if let Some(ids) = self.confirm.clone() {
            match key {
                "escape" => self.confirm = None,
                "enter" => self.run(BatchKind::Delete, ids, cx),
                _ => return,
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        match key {
            "escape" if self.space_menu.is_open() => self.close_space_menu(cx),
            "escape" if !self.selection.is_empty() => self.clear_selection(cx),
            "a" if mods.secondary() && !mods.shift && !mods.alt => self.select_all_filtered(cx),
            "backspace" | "delete" if mods.secondary() && !self.selection.is_empty() => {
                let ids = self.selection.iter().cloned().collect();
                self.ask_delete(ids, window, cx);
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    // ---- render pieces ----

    fn render_search(&self, theme: &Theme) -> gpui::Div {
        div()
            .flex_none()
            .w(px(260.0))
            .h(px(30.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(10.0))
            .rounded(px(8.0))
            .bg(ink(0.04))
            .text_size(crate::typography::ui_rems(13.0))
            .child(
                icon(icons::MAGNIFER)
                    .size(px(13.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(div().flex_1().min_w_0().child(self.search.clone()))
    }

    fn render_tab(
        &self,
        theme: &Theme,
        status: StatusFilter,
        count: usize,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let active = self.status == status;
        div()
            .id(SharedString::from(format!("cm-tab-{}", status.label())))
            .flex_none()
            .h(px(28.0))
            .px(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .rounded(px(8.0))
            .text_size(crate::typography::ui_rems(12.5))
            .cursor_pointer()
            .when(active, |el| {
                el.bg(ink(0.07))
                    .text_color(theme.text)
                    .font_weight(gpui::FontWeight::MEDIUM)
            })
            .when(!active, |el| {
                el.text_color(theme.text_muted)
                    .hover(|s| s.bg(ink(0.04)).text_color(theme.text))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.status = status;
                cx.notify();
            }))
            .child(status.label())
            .child(
                div()
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from(count.to_string())),
            )
    }

    /// The project filter: a quiet trigger naming the pick, opening the same
    /// menu card the sidebar's project picker uses.
    fn render_space_filter(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (label, options): (SharedString, Vec<(Option<String>, SharedString)>) = {
            let state = self.state.read(cx);
            let label = self
                .space
                .as_deref()
                .and_then(|id| state.space_row(id))
                .map(|space| space.display_name().to_string())
                .unwrap_or_else(|| "All projects".into());
            let options = std::iter::once((None, SharedString::from("All projects")))
                .chain(state.spaces_sorted().into_iter().map(|space| {
                    (
                        Some(space.id.clone()),
                        space.display_name().to_string().into(),
                    )
                }))
                .collect();
            (label.into(), options)
        };
        let open = self.space_menu.is_open();
        let trigger = div()
            .id("cm-space-filter")
            .flex_none()
            .h(px(28.0))
            .max_w(px(220.0))
            .px(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .rounded(px(8.0))
            .text_size(crate::typography::ui_rems(12.5))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .when(open, |el| el.bg(ink(0.07)).text_color(theme.text))
            .hover(|s| s.bg(ink(0.07)).text_color(theme.text))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.space_menu.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                if this.space_menu.take_press_was_open() {
                    this.close_space_menu(cx);
                } else {
                    this.space_menu.open(());
                    cx.notify();
                }
            }))
            .child(
                icon(icons::FOLDER)
                    .size(px(13.0))
                    .text_color(theme.text_muted),
            )
            .child(div().min_w_0().truncate().child(label))
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(11.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            );
        if self.space_menu.get().is_none() {
            return trigger;
        }
        let mut list = div()
            .id("cm-space-list")
            .max_h(px(320.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(2.0));
        for (ix, (id, name)) in options.into_iter().enumerate() {
            let active = id == self.space;
            list = list.child(
                popover::menu_row(theme, active, format!("cm-space-row-{ix}"))
                    .id(("cm-space-row", ix))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_space(id.clone(), cx)))
                    .child(div().flex_1().min_w_0().truncate().child(name)),
            );
        }
        let card = popover::popover_card(theme)
            .w(px(240.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_space_menu(cx)))
            .child(list)
            .into_any_element();
        trigger.relative().child(popover::anchored_menu_below_end(
            "cm-space-menu",
            card,
            self.space_menu.closing_since(),
        ))
    }

    /// Column labels over the list, led by the tri-state select-all box.
    fn render_list_header(
        &self,
        theme: &Theme,
        rows: &[Chat],
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let picked = rows
            .iter()
            .filter(|chat| self.selection.contains(&chat.id))
            .count();
        let state = match picked {
            0 => CheckboxState::Unchecked,
            n if n == rows.len() => CheckboxState::Checked,
            _ => CheckboxState::Mixed,
        };
        let label = |text: &'static str| {
            div()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_muted.opacity(0.6))
                .child(SharedString::from(text))
        };
        div()
            .h(px(32.0))
            .px(px(12.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .border_b_1()
            .border_color(hairline(0.06))
            .child(
                div()
                    .id("cm-select-all")
                    .debug_selector(|| "cm-select-all".into())
                    .flex_none()
                    .size(px(20.0))
                    .ml(px(-3.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(!rows.is_empty(), |el| {
                        el.cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_all_filtered(cx)))
                    })
                    .child(widgets::checkbox(
                        theme,
                        if rows.is_empty() {
                            CheckboxState::Disabled
                        } else {
                            state
                        },
                    )),
            )
            .child(div().flex_1().min_w_0().child(label("Title")))
            .child(div().flex_none().w(px(PROJECT_COL)).child(label("Project")))
            .child(
                div()
                    .flex_none()
                    .w(px(TRAILING_COL))
                    .flex()
                    .justify_end()
                    .child(label("Updated")),
            )
    }

    fn render_bucket(theme: &Theme, bucket: Bucket, first: bool) -> AnyElement {
        div()
            .px(px(12.0))
            .pt(px(if first { 12.0 } else { 20.0 }))
            .pb(px(4.0))
            .text_size(crate::typography::ui_rems(11.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted.opacity(0.6))
            .child(bucket.label())
            .into_any_element()
    }

    /// One chat row: [checkbox] title + tags | project | time. The checkbox
    /// shows on hover and stays out while nothing is selected; the time
    /// swaps for Open / Archive / Delete on hover.
    fn render_row(
        &self,
        theme: &Theme,
        chat: &Chat,
        ix: usize,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selection.contains(&chat.id);
        let selecting = !self.selection.is_empty();
        let (space_name, status) = {
            let state = self.state.read(cx);
            (
                state
                    .space_for_chat(chat)
                    .map(|space| SharedString::from(space.display_name().to_string())),
                state.display_status_for(chat, now),
            )
        };
        let group: SharedString = format!("cm-row-{ix}").into();
        let title: SharedString = chat
            .title
            .clone()
            .unwrap_or_else(|| "Untitled session".into())
            .into();
        let time_ago: SharedString = crate::state::format_time_ago(recency(chat), now).into();
        let archived = chat.archived;
        let live_dot = match status {
            ChatIndicator::Working => Some(theme.busy),
            ChatIndicator::AwaitingInput => Some(theme.accent),
            _ => None,
        };

        let checkbox = widgets::checkbox(
            theme,
            if selected {
                CheckboxState::Checked
            } else {
                CheckboxState::Unchecked
            },
        )
        .opacity(if selecting { 1.0 } else { 0.0 })
        .group_hover(group.clone(), |s| s.opacity(1.0));

        let title_cell = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .when_some(live_dot, |el, color| {
                el.child(div().flex_none().size(px(6.0)).rounded_full().bg(color))
            })
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(if archived {
                        theme.text_muted
                    } else {
                        theme.text
                    })
                    .child(title),
            )
            .when(chat.worktree.is_some(), |el| {
                el.child(tag(theme, "Worktree"))
            })
            .when(archived, |el| el.child(tag(theme, "Archived")));

        let project_cell = div()
            .flex_none()
            .w(px(PROJECT_COL))
            .truncate()
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text_muted.opacity(0.75))
            .child(space_name.unwrap_or_else(|| "—".into()));

        let id = chat.id.clone();
        let mut actions = div()
            .absolute()
            .top_0()
            .bottom_0()
            .right(px(-4.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .opacity(0.0)
            .group_hover(group.clone(), |s| s.opacity(1.0));
        if !archived {
            let open_id = id.clone();
            actions = actions.child(with_hint(
                icon_button(
                    theme,
                    format!("cm-open-{ix}").into(),
                    icons::ARROW_UP_RIGHT,
                    false,
                )
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.stop_propagation();
                    cx.emit(ChatManagerEvent::OpenChat(open_id.clone()));
                })),
                "Open",
            ));
        }
        let (kind, glyph, hint) = if archived {
            (
                BatchKind::Unarchive,
                icons::ARCHIVE_UP_MINIMALISTIC,
                "Unarchive",
            )
        } else {
            (BatchKind::Archive, icons::ARCHIVE_MINIMALISTIC, "Archive")
        };
        let archive_id = id.clone();
        actions = actions.child(with_hint(
            icon_button(theme, format!("cm-archive-{ix}").into(), glyph, false).on_click(
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.run(kind, vec![archive_id.clone()], cx);
                }),
            ),
            hint,
        ));
        let delete_id = id.clone();
        actions = actions.child(with_hint(
            icon_button(
                theme,
                format!("cm-delete-{ix}").into(),
                icons::TRASH_BIN_MINIMALISTIC,
                true,
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.ask_delete(vec![delete_id.clone()], window, cx);
            })),
            "Delete",
        ));

        let trailing = div()
            .flex_none()
            .relative()
            .w(px(TRAILING_COL))
            .h(px(24.0))
            .child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_end()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .group_hover(group.clone(), |s| s.opacity(0.0))
                    .child(time_ago),
            )
            .child(actions);

        let (rest_bg, hover_bg) = if selected {
            (theme.accent.opacity(0.10), theme.accent.opacity(0.14))
        } else {
            (gpui::transparent_black(), ink(0.04))
        };
        div()
            .id(("cm-row", ix))
            .debug_selector(move || format!("cm-row-{ix}"))
            .group(group)
            .h(px(ROW_HEIGHT))
            .px(px(12.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .rounded(px(8.0))
            .bg(rest_bg)
            .hover(move |s| s.bg(hover_bg))
            .cursor_pointer()
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                this.click_row(&id, event, archived, cx);
            }))
            .child(checkbox)
            .child(title_cell)
            .child(project_cell)
            .child(trailing)
            .into_any_element()
    }

    fn render_bar_action(
        &self,
        theme: &Theme,
        action: BarAction,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let BarAction {
            id,
            icon: glyph,
            label,
            danger,
        } = action;
        let busy = self.working.is_some();
        let (fg, wash) = if danger {
            (theme.danger_muted, theme.danger.opacity(0.12))
        } else {
            (theme.text, ink(0.08))
        };
        let button = div()
            .id(id)
            .debug_selector(move || id.to_string())
            .flex_none()
            .h(px(28.0))
            .px(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .rounded(px(8.0))
            .text_size(crate::typography::ui_rems(12.5))
            .text_color(fg)
            .child(icon(glyph).size(px(14.0)).text_color(fg))
            .child(label);
        if busy {
            return button.opacity(0.4);
        }
        button
            .cursor_pointer()
            .hover(move |s| s.bg(wash))
            .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
    }

    /// The floating selection bar: count, the batch actions that apply to
    /// the selection, and a clear button.
    fn render_action_bar(
        &self,
        theme: &Theme,
        selected: &[Chat],
        hidden: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let any_active = selected.iter().any(|chat| !chat.archived);
        let any_archived = selected.iter().any(|chat| chat.archived);
        let count: SharedString = match self.working {
            Some(kind) => kind.gerund().into(),
            None if hidden > 0 => format!("{} selected · {hidden} hidden", selected.len()).into(),
            None => format!("{} selected", selected.len()).into(),
        };
        let divider = || {
            div()
                .flex_none()
                .w(px(1.0))
                .h(px(16.0))
                .mx(px(4.0))
                .bg(hairline(0.10))
        };
        let bar = div()
            .id("cm-action-bar")
            .debug_selector(|| "cm-action-bar".into())
            .occlude()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .p(px(6.0))
            .pl(px(14.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(hairline(0.10))
            .bg(theme.surface_dialog)
            .shadow_lg()
            .child(
                div()
                    .flex_none()
                    .mr(px(6.0))
                    .text_size(crate::typography::ui_rems(12.5))
                    .text_color(theme.text_muted)
                    .child(count),
            )
            .child(divider())
            .when(any_active, |el| {
                el.child(self.render_bar_action(
                    theme,
                    BarAction {
                        id: "cm-bar-archive",
                        icon: icons::ARCHIVE_MINIMALISTIC,
                        label: "Archive",
                        danger: false,
                    },
                    |this, _, cx| {
                        let ids = this.selection.iter().cloned().collect();
                        this.run(BatchKind::Archive, ids, cx);
                    },
                    cx,
                ))
            })
            .when(any_archived, |el| {
                el.child(self.render_bar_action(
                    theme,
                    BarAction {
                        id: "cm-bar-unarchive",
                        icon: icons::ARCHIVE_UP_MINIMALISTIC,
                        label: "Unarchive",
                        danger: false,
                    },
                    |this, _, cx| {
                        let ids = this.selection.iter().cloned().collect();
                        this.run(BatchKind::Unarchive, ids, cx);
                    },
                    cx,
                ))
            })
            .child(self.render_bar_action(
                theme,
                BarAction {
                    id: "cm-bar-delete",
                    icon: icons::TRASH_BIN_MINIMALISTIC,
                    label: "Delete",
                    danger: true,
                },
                |this, window, cx| {
                    let ids = this.selection.iter().cloned().collect();
                    this.ask_delete(ids, window, cx);
                },
                cx,
            ))
            .child(divider())
            .child(with_hint(
                icon_button(theme, "cm-bar-clear".into(), icons::CLOSE, false)
                    .on_click(cx.listener(|this, _, _, cx| this.clear_selection(cx))),
                "Clear selection",
            ));
        div()
            .absolute()
            .left_0()
            .right_0()
            .bottom(px(20.0))
            .flex()
            .justify_center()
            .child(bar)
            .into_any_element()
    }

    /// The delete confirmation over `ids`: irreversible, once, with the
    /// breakdown the glossary names (spaces, worktrees, live turns).
    fn render_confirm(
        &self,
        theme: &Theme,
        ids: &[String],
        now: DateTime<Utc>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let lookup: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let targets = self.chats_by_id(&lookup, cx);
        let refs: Vec<&Chat> = targets.iter().collect();
        // A live Turn (glossary): Working or AwaitingInput — both die with
        // the chat on delete.
        let breakdown = {
            let state = self.state.read(cx);
            delete_breakdown(&refs, |chat| {
                matches!(
                    state.display_status_for(chat, now),
                    ChatIndicator::Working | ChatIndicator::AwaitingInput
                )
            })
        };
        let title = if breakdown.total == 1 {
            match targets.first().and_then(|chat| chat.title.as_deref()) {
                Some(title) => format!("Delete \u{201c}{title}\u{201d}?"),
                None => "Delete this session?".to_string(),
            }
        } else {
            format!("Delete {}?", plural(breakdown.total, "session", "sessions"))
        };
        let mut notes: Vec<String> = Vec::new();
        if breakdown.spaces > 1 {
            notes.push(format!("Across {} projects", breakdown.spaces));
        }
        if breakdown.worktrees > 0 {
            notes.push(format!("{} in a session worktree", breakdown.worktrees));
        }
        if breakdown.live > 0 {
            notes.push(format!("{} still running", breakdown.live));
        }
        let card = popover::dialog_card(theme)
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.confirm = None;
                cx.notify();
            }))
            .child(div().truncate().child(popover::dialog_title(theme, &title)))
            .child(div().mt(px(8.0)).child(popover::dialog_body(
                theme,
                "The transcript, terminals, and running programs go with it. This can\u{2019}t be undone.",
            )))
            .when(!notes.is_empty(), |el| {
                el.child(
                    div()
                        .mt(px(10.0))
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(6.0))
                        .children(notes.into_iter().map(|note| {
                            div()
                                .px(px(6.0))
                                .py(px(2.0))
                                .rounded(px(4.0))
                                .bg(theme.warning.opacity(0.10))
                                .text_size(crate::typography::ui_rems(11.5))
                                .text_color(theme.warning_muted)
                                .child(note)
                        })),
                )
            })
            .child(
                div()
                    .mt(px(18.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(theme, "Cancel", "cm-delete-cancel")
                            .id("cm-delete-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.confirm = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_danger(theme, "Delete")
                            .id("cm-delete-confirm")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(ids) = this.confirm.clone() {
                                    this.run(BatchKind::Delete, ids, cx);
                                }
                            })),
                    ),
            )
            .into_any_element();
        popover::modal("cm-delete-dialog", window.viewport_size(), card)
    }

    fn render_empty(theme: &Theme, no_chats: bool) -> AnyElement {
        let (headline, hint) = if no_chats {
            ("No sessions yet", "Sessions you start show up here.")
        } else {
            ("Nothing matches", "Try a different search or filter.")
        };
        div()
            .mt(px(96.0))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(4.0))
            .child(
                div()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text_muted)
                    .child(headline),
            )
            .child(
                div()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted.opacity(0.55))
                    .child(hint),
            )
            .into_any_element()
    }
}

impl gpui::EventEmitter<ChatManagerEvent> for ChatManagerPage {}

impl Render for ChatManagerPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        if std::mem::take(&mut self.focus_pending) {
            window.focus(&self.focus, cx);
        }
        // Prune ids of chats the engine has dropped (deleted elsewhere,
        // device sync) — the selection never acts on ghosts.
        let total = {
            let state = self.state.read(cx);
            let existing: HashSet<&str> = state.chats.iter().map(|chat| chat.id.as_str()).collect();
            self.selection.retain(|id| existing.contains(id.as_str()));
            if let Some(ids) = self.confirm.as_mut() {
                ids.retain(|id| existing.contains(id.as_str()));
            }
            // A filter scoped to a space the engine no longer has would
            // show an empty list under an "All projects" label — fall back.
            if self
                .space
                .as_deref()
                .is_some_and(|id| state.space_row(id).is_none())
            {
                self.space = None;
            }
            state.chats.len()
        };
        if self.confirm.as_ref().is_some_and(Vec::is_empty) {
            self.confirm = None;
        }

        let rows = self.filtered_rows(cx);
        let selected = self.selected_rows(cx);
        let hidden = {
            let shown: HashSet<&str> = rows.iter().map(|chat| chat.id.as_str()).collect();
            selected
                .iter()
                .filter(|chat| !shown.contains(chat.id.as_str()))
                .count()
        };

        let today = Local::now().date_naive();
        let mut items: Vec<AnyElement> = Vec::with_capacity(rows.len() + 5);
        let mut bucket = None;
        for (ix, chat) in rows.iter().enumerate() {
            let this_bucket = Bucket::of(recency(chat).with_timezone(&Local).date_naive(), today);
            if bucket != Some(this_bucket) {
                items.push(Self::render_bucket(&theme, this_bucket, bucket.is_none()));
                bucket = Some(this_bucket);
            }
            items.push(self.render_row(&theme, chat, ix, now, cx));
        }
        let body = if items.is_empty() {
            Self::render_empty(&theme, total == 0)
        } else {
            div().flex().flex_col().children(items).into_any_element()
        };

        let tabs: Vec<_> = StatusFilter::ALL
            .into_iter()
            .map(|status| {
                let count = self.rows_for(status, cx).len();
                self.render_tab(&theme, status, count, cx)
            })
            .collect();

        let header = div()
            .flex_none()
            .w_full()
            .max_w(px(PAGE_MAX_W))
            .mx_auto()
            .px(px(24.0))
            .pt(px(28.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(16.0))
                    .child(widgets::page_header(&theme, "Chat manager", None))
                    .child(self.render_search(&theme)),
            )
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(2.0))
                    .children(tabs)
                    .child(div().flex_1())
                    .child(self.render_space_filter(&theme, cx)),
            )
            .when_some(self.error.clone(), |el, message| {
                el.child(
                    div().mt(px(12.0)).child(
                        widgets::error_strip(&theme, message)
                            .id("cm-error")
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.error = None;
                                cx.notify();
                            })),
                    ),
                )
            })
            .child(
                div()
                    .mt(px(12.0))
                    .child(self.render_list_header(&theme, &rows, cx)),
            );

        let selecting = !selected.is_empty();
        let action_bar = selecting.then(|| self.render_action_bar(&theme, &selected, hidden, cx));
        let confirm = self
            .confirm
            .clone()
            .map(|ids| self.render_confirm(&theme, &ids, now, window, cx));

        div()
            .id("chat-manager-page")
            .debug_selector(|| "chat-manager-page".into())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                this.on_key(event, window, cx);
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .id("cm-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .w_full()
                            .max_w(px(PAGE_MAX_W))
                            .mx_auto()
                            .px(px(24.0))
                            // Room for the floating bar over the last rows.
                            .pb(px(if selecting { 88.0 } else { 24.0 }))
                            .child(body),
                    ),
            )
            .children(action_bar)
            .children(confirm)
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
            routine_run: None,
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
    fn buckets_follow_calendar_days() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let day = |n: i64| today - Duration::days(n);
        assert_eq!(Bucket::of(today, today), Bucket::Today);
        // Clock skew (a row stamped "tomorrow") still reads as today.
        assert_eq!(Bucket::of(day(-1), today), Bucket::Today);
        assert_eq!(Bucket::of(day(1), today), Bucket::Yesterday);
        assert_eq!(Bucket::of(day(2), today), Bucket::Week);
        assert_eq!(Bucket::of(day(6), today), Bucket::Week);
        assert_eq!(Bucket::of(day(7), today), Bucket::Month);
        assert_eq!(Bucket::of(day(29), today), Bucket::Month);
        assert_eq!(Bucket::of(day(30), today), Bucket::Older);
    }

    #[test]
    fn range_spans_anchor_to_target_in_either_direction() {
        let rows = vec![chat("a"), chat("b"), chat("c"), chat("d")];
        assert_eq!(range_ids(&rows, "b", "d").unwrap(), ["b", "c", "d"]);
        assert_eq!(range_ids(&rows, "d", "b").unwrap(), ["b", "c", "d"]);
        assert_eq!(range_ids(&rows, "c", "c").unwrap(), ["c"]);
        assert!(range_ids(&rows, "gone", "c").is_none());
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

    fn page_with(
        cx: &mut gpui::TestAppContext,
        chats: Vec<Chat>,
    ) -> (Entity<ChatManagerPage>, &mut gpui::VisualTestContext) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            cx.set_global(Theme::default());
            crate::settings::init(crate::settings::UiSettings::default(), dir.path(), cx);
        });
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.chats = chats;
            state
        });
        let (page, visual) =
            cx.add_window_view(|_window, cx| ChatManagerPage::new(state.clone(), cx));
        // A window only paints after a notify — force the first frame.
        page.update(&mut *visual, |_page, cx| cx.notify());
        visual.run_until_parked();
        (page, visual)
    }

    /// Smoke: rows render, a click selects any row (archived included), the
    /// floating bar appears with the selection, shift-click extends a range,
    /// and the header box toggles the whole filtered list.
    #[gpui::test]
    fn row_clicks_drive_the_selection(cx: &mut gpui::TestAppContext) {
        let mut archived = chat("b");
        archived.archived = true;
        let (page, visual) = page_with(cx, vec![chat("a"), archived, chat("c")]);

        visual
            .debug_bounds("chat-manager-page")
            .expect("page root renders");
        assert!(
            visual.debug_bounds("cm-action-bar").is_none(),
            "no bar without a selection"
        );

        let click = |visual: &mut gpui::VisualTestContext, selector: &'static str, mods| {
            let point = visual.debug_bounds(selector).unwrap().center();
            visual.simulate_click(point, mods);
            visual.run_until_parked();
        };

        click(visual, "cm-row-0", gpui::Modifiers::none());
        page.read_with(&*visual, |page, _| assert_eq!(page.selection.len(), 1));
        visual
            .debug_bounds("cm-action-bar")
            .expect("bar renders once the selection is live");

        // Shift-click extends from the anchor (row 0) through row 2,
        // archived row 1 included.
        click(visual, "cm-row-2", gpui::Modifiers::shift());
        page.read_with(&*visual, |page, _| assert_eq!(page.selection.len(), 3));

        // Plain click on a selected row toggles it back off.
        click(visual, "cm-row-1", gpui::Modifiers::none());
        page.read_with(&*visual, |page, _| assert_eq!(page.selection.len(), 2));

        // Header box: partial → all, all → none.
        click(visual, "cm-select-all", gpui::Modifiers::none());
        page.read_with(&*visual, |page, _| assert_eq!(page.selection.len(), 3));
        click(visual, "cm-select-all", gpui::Modifiers::none());
        page.read_with(&*visual, |page, _| assert!(page.selection.is_empty()));

        // A space filter whose space no longer resolves falls back to all
        // projects instead of an empty list.
        page.update(&mut *visual, |page, cx| {
            page.space = Some("gone".into());
            cx.notify();
        });
        visual.run_until_parked();
        page.read_with(&*visual, |page, _| assert_eq!(page.space, None));
    }

    /// A settled batch drops its targets from the selection only on
    /// success — a failure keeps them so the pass can be retried.
    #[gpui::test]
    fn failed_batch_keeps_selection(cx: &mut gpui::TestAppContext) {
        let (page, visual) = page_with(cx, vec![chat("a"), chat("b"), chat("c")]);
        page.update(&mut *visual, |page, cx| {
            page.selection.extend(["a".into(), "b".into(), "c".into()]);
            let ids = vec!["a".to_string(), "b".to_string()];
            page.batch_settled(BatchKind::Archive, &ids, Some("engine gone".into()), cx);
            assert_eq!(page.selection.len(), 3);
            assert!(page.error.is_some());
            assert_eq!(page.working, None);
            page.batch_settled(BatchKind::Archive, &ids, None, cx);
            assert_eq!(page.selection.len(), 1);
            assert!(page.selection.contains("c"));
        });
    }
}
