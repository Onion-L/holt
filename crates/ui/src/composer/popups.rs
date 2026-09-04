//! Completion popups: `@` file mentions and `/` slash commands — parallel
//! token-driven state machines (mutually exclusive by token shape) sharing one
//! floating-scrollbar treatment.

use super::Composer;

use std::ops::Range;
use std::time::Duration;

use gpui::{App, Context, SharedString, Window, div, prelude::*, px};

use holt_proto::{FileSearchMatch, ProviderId, SkillListing, SlashCommand};
use holt_rpc::{RpcError, methods};

use super::slash::{SlashCandidate, popup_candidates};
use crate::theme::Theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MentionToken {
    range: Range<usize>,
    query: String,
}

/// The `@` must begin a token. This intentionally excludes `name@example.com`
/// and ordinary words while allowing punctuation such as `(@src`.
fn mention_token(text: &str, cursor: usize) -> Option<MentionToken> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let token_start = text[..cursor]
        .char_indices()
        .rev()
        .find_map(|(at, ch)| ch.is_whitespace().then_some(at + ch.len_utf8()))
        .unwrap_or(0);
    let relative_at = text[token_start..cursor].rfind('@')?;
    let at = token_start + relative_at;
    let valid_boundary = at == 0
        || text[..at]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_whitespace() || matches!(ch, '(' | '[' | '{'));
    if text[at + 1..cursor].contains('@') || !valid_boundary {
        return None;
    }
    let end = text[cursor..]
        .char_indices()
        .find_map(|(at, ch)| ch.is_whitespace().then_some(cursor + at))
        .unwrap_or(text.len());
    Some(MentionToken {
        range: at..end,
        query: text[at + 1..cursor].to_string(),
    })
}

/// Restart a popup's row stack at the top (fresh open / query / result set).
fn reset_scroll_offset(scroll: &gpui::ScrollHandle) {
    scroll.set_offset(gpui::Point::new(px(0.0), px(0.0)));
}

/// The `/` must open the input: slash commands are whole-prompt prefixes
/// (`/compact`, `/goal ship it`), so only the first token triggers, and a
/// query containing another `/` (a typed path) never does.
fn slash_token(text: &str, cursor: usize) -> Option<MentionToken> {
    if cursor > text.len() || !text.is_char_boundary(cursor) || !text.starts_with('/') {
        return None;
    }
    let end = text
        .char_indices()
        .find_map(|(at, ch)| ch.is_whitespace().then_some(at))
        .unwrap_or(text.len());
    // Cursor outside the command token (typing the argument): popup closed.
    if cursor == 0 || cursor > end {
        return None;
    }
    let query = &text[1..cursor];
    if query.contains('/') {
        return None;
    }
    Some(MentionToken {
        range: 0..end,
        query: query.to_string(),
    })
}
/// Slash-command completion state: like [`FileMentionState`] but the
/// candidate list is a mixed-source model — catalog skills (`ListSkills`,
/// fetched once per cwd, provider-agnostic) merged ahead of the provider's
/// commands (`ListCommands`, cached per provider) — filtered locally per
/// keystroke, no RPC/debounce/skeleton churn while typing.
#[derive(Debug, Clone, Default)]
pub(super) struct SlashState {
    pub(super) token: Option<MentionToken>,
    /// The merged rows for the current open: skills first, commands after.
    candidates: Vec<SlashCandidate>,
    /// Indices into `candidates`, filter-ranked for the query.
    filtered: Vec<usize>,
    active: Option<usize>,
    /// Provider the command entries are for (the `slash_cache` key).
    provider: Option<ProviderId>,
    /// The last skills catalog, with the cwd it was fetched for.
    skills: SkillListing,
    skills_cwd: Option<String>,
    request: u64,
    skills_request: u64,
    loading: bool,
    skills_loading: bool,
    error: Option<SharedString>,
    dismissed: Option<(Range<usize>, String)>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct FileMentionState {
    pub(super) token: Option<MentionToken>,
    results: Vec<FileSearchMatch>,
    active: Option<usize>,
    request: u64,
    loading: bool,
    /// Why the last search failed, for the popup. A failure MUST NOT render
    /// as "No matching files": searches fail for reasons the user can act on
    /// (an engine too old for `SearchFiles`, or unreachable), and the empty
    /// state hid them (user report).
    error: Option<SharedString>,
    /// Full token text, not just the cursor-relative query: moving within a
    /// dismissed token keeps it closed, while any edit re-enables completion.
    dismissed: Option<(Range<usize>, String)>,
}

fn mention_response_is_current(state: &FileMentionState, request: u64) -> bool {
    state.request == request && state.token.is_some()
}

/// A failed file search, translated for the popup. `UnknownMethod` is the
/// version-skew case: `SearchFiles` shipped after v0.1.9, so an engine older
/// than that answers "unknown method".
fn mention_error_message(err: &RpcError) -> SharedString {
    match err {
        RpcError::UnknownMethod(_) => {
            "File search isn't available — the engine doesn't support it yet".into()
        }
        RpcError::Transport(_) | RpcError::Closed => "The engine is unreachable".into(),
        RpcError::BadParams(_) | RpcError::Failed(_) => "File search failed".into(),
    }
}

/// A failed command discovery, translated for the popup.
fn slash_error_message(err: &RpcError) -> SharedString {
    match err {
        RpcError::UnknownMethod(_) => {
            "Command listing isn't available — the engine doesn't support it yet".into()
        }
        RpcError::Transport(_) | RpcError::Closed => "The engine is unreachable".into(),
        RpcError::BadParams(_) | RpcError::Failed(_) => {
            "Couldn't load this agent's commands".into()
        }
    }
}

/// A failed skill listing, translated for the popup.
fn skills_error_message(err: &RpcError) -> SharedString {
    match err {
        RpcError::UnknownMethod(_) => {
            "Skills aren't available — the engine doesn't support them yet".into()
        }
        RpcError::Transport(_) | RpcError::Closed => "The engine is unreachable".into(),
        RpcError::BadParams(_) | RpcError::Failed(_) => "Couldn't load skills".into(),
    }
}

impl Composer {
    fn sync_mention_controls(&mut self, cx: &mut Context<Self>) {
        let open = self.mention.token.is_some() || self.slash.token.is_some();
        let has_selection = if self.slash.token.is_some() {
            self.slash.active.is_some()
        } else {
            self.mention.active.is_some()
        };
        self.input.update(cx, |input, cx| {
            input.set_mention_controls(open, has_selection, cx)
        });
    }

    /// Tear down the entire completion lifecycle. Advancing the generation is
    /// important even when the spawned task is dropped: an RPC response may
    /// already be queued for delivery on the UI executor.
    pub(super) fn reset_mention(
        &mut self,
        dismissed: Option<(Range<usize>, String)>,
        cx: &mut Context<Self>,
    ) {
        let request = self.mention.request.wrapping_add(1);
        self.mention_task = None;
        self.mention = FileMentionState {
            request,
            dismissed,
            ..FileMentionState::default()
        };
        self.sync_mention_controls(cx);
    }

    pub(super) fn on_input_edited(&mut self, cx: &mut Context<Self>) {
        if self.wizard.is_some() {
            if self.mention.token.is_some() || self.mention_task.is_some() {
                self.reset_mention(None, cx);
            }
            if self.slash.token.is_some() || self.slash_task.is_some() {
                self.reset_slash(None, cx);
            }
            return;
        }
        let (text, cursor) = {
            let input = self.input.read(cx);
            (input.text().to_string(), input.cursor_offset())
        };
        self.update_slash(&text, cursor, cx);
        let token = mention_token(&text, cursor);
        let still_dismissed = token.as_ref().is_some_and(|token| {
            self.mention
                .dismissed
                .as_ref()
                .is_some_and(|(range, value)| {
                    token.range == *range && text.get(range.clone()) == Some(value.as_str())
                })
        });
        if still_dismissed {
            self.mention.token = None;
            self.mention_task = None;
            self.sync_mention_controls(cx);
            cx.notify();
            return;
        }
        self.mention.dismissed = None;
        if token == self.mention.token {
            self.sync_mention_controls(cx);
            cx.notify();
            return;
        }
        self.mention.request = self.mention.request.wrapping_add(1);
        self.mention_task = None;
        // Refining an open menu keeps the stale rows visible until the new
        // response lands — clearing here made the popup bounce through the
        // skeleton (and a different height) on every keystroke.
        let refining = self.mention.token.is_some() && token.is_some();
        self.mention.token = token.clone();
        if !refining {
            self.mention.results.clear();
            self.mention.active = None;
            // Fresh open: the row stack restarts at the top.
            reset_scroll_offset(&self.mention_scroll);
        }
        self.mention.error = None;
        self.mention.loading = token.is_some();
        self.sync_mention_controls(cx);
        let Some(token) = token else {
            cx.notify();
            return;
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.mention.loading = false;
            cx.notify();
            return;
        };
        let (params, has_context) = {
            let state = self.state.read(cx);
            let mut params = serde_json::Map::new();
            params.insert("query".into(), token.query.clone().into());
            let has_context = if let Some(chat) = state.selected_chat_row() {
                params.insert("chatId".into(), chat.id.clone().into());
                true
            } else if let Some(space) = state.selected_space_row() {
                params.insert("spaceId".into(), space.id.clone().into());
                true
            } else {
                false
            };
            (serde_json::Value::Object(params), has_context)
        };
        if !has_context {
            self.mention.loading = false;
            cx.notify();
            return;
        }
        let request = self.mention.request;
        self.mention_task = Some(cx.spawn(async move |this, cx| {
            // A short debounce prevents one full workspace walk per keystroke
            // during normal typing. The generation check below still guards
            // requests that were already in flight when the query changed.
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            let mut result = engine
                .client()
                .call(methods::SEARCH_FILES, params.clone())
                .await;
            if matches!(result, Err(RpcError::Transport(_)) | Err(RpcError::Closed)) {
                // One retry rides out a cold engine start (the diffs pane
                // retries forever; a keystroke-scoped search gets a single
                // second chance).
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                result = engine.client().call(methods::SEARCH_FILES, params).await;
            }
            this.update(cx, |composer, cx| {
                if !mention_response_is_current(&composer.mention, request) {
                    return;
                }
                composer.mention.loading = false;
                match result {
                    Ok(value) => match serde_json::from_value::<Vec<FileSearchMatch>>(value) {
                        Ok(results) => {
                            composer.mention.error = None;
                            composer.mention.active = (!results.is_empty()).then_some(0);
                            composer.mention.results = results;
                            // New result set: the row stack restarts at the top.
                            reset_scroll_offset(&composer.mention_scroll);
                        }
                        Err(err) => tracing::warn!(%err, "file mention response decode failed"),
                    },
                    Err(err) => {
                        tracing::warn!(%err, "file mention search failed");
                        composer.mention.results.clear();
                        composer.mention.active = None;
                        composer.mention.error = Some(mention_error_message(&err));
                    }
                }
                composer.sync_mention_controls(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub(super) fn move_mention(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.mention.active =
            crate::popover::menu_step(self.mention.active, self.mention.results.len(), delta);
        if let Some(active) = self.mention.active {
            // Keep the keyboard cursor visible in the scrolled row stack.
            self.mention_scroll.scroll_to_item(active);
        }
        self.sync_mention_controls(cx);
        cx.notify();
    }

    pub(super) fn dismiss_mention(&mut self, cx: &mut Context<Self>) {
        let dismissed = self.mention.token.as_ref().and_then(|token| {
            self.input
                .read(cx)
                .text()
                .get(token.range.clone())
                .map(|text| (token.range.clone(), text.to_string()))
        });
        self.reset_mention(dismissed, cx);
        cx.notify();
    }

    pub(super) fn accept_mention(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.mention.token.clone() else {
            return;
        };
        let Some((path, is_dir)) = self
            .mention
            .active
            .and_then(|active| self.mention.results.get(active))
            .map(|result| (result.path.clone(), result.is_dir))
        else {
            return;
        };
        self.input.update(cx, |input, cx| {
            input.replace_mention(token.range, &path, is_dir, cx)
        });
        self.reset_mention(None, cx);
        cx.notify();
    }

    pub(super) fn render_file_mention_popup(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let token = self.mention.token.as_ref()?;
        let mut card = crate::popover::popover_card(theme)
            .w_full()
            .max_h(px(320.0))
            .overflow_hidden()
            // GPUI dispatches this captured stream while the thumb is
            // dragged, including when the pointer has left the popup.
            .on_drag_move(cx.listener(Self::on_popup_bar_drag_move))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.dismiss_mention(cx)));
        if self.mention.loading && self.mention.results.is_empty() {
            card = card.child(crate::popover::skeleton_rows(
                "file-mention-loading",
                theme,
                3,
                cx.entity_id(),
                cx,
            ));
        } else if let Some(error) = self.mention.error.clone() {
            card = card.child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.danger_muted)
                    .child(error),
            );
        } else if self.mention.results.is_empty() {
            card = card.child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child(if token.query.is_empty() {
                        "No files available"
                    } else {
                        "No matching files"
                    }),
            );
        } else {
            let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(self.mention.results.len());
            for (ix, result) in self.mention.results.iter().enumerate() {
                let selected = self.mention.active == Some(ix);
                let (directory, name) = match result.path.rsplit_once('/') {
                    Some((directory, name)) => (directory.to_string(), name.to_string()),
                    None => (String::new(), result.path.clone()),
                };
                rows.push(
                    crate::popover::menu_row(theme, selected, format!("file-mention-result-{ix}"))
                        .id(("file-mention-result", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.mention.active = Some(ix);
                            this.accept_mention(cx);
                        }))
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    crate::icons::icon(if result.is_dir {
                                        crate::icons::FOLDER
                                    } else {
                                        crate::icons::DOCUMENT
                                    })
                                    .size(px(14.0))
                                    .flex_none()
                                    .text_color(theme.text_muted),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(px(13.0))
                                        .text_color(theme.text)
                                        .child(name),
                                )
                                .when(!directory.is_empty(), |row| {
                                    row.child(
                                        div()
                                            .min_w_0()
                                            .flex_1()
                                            .overflow_hidden()
                                            .truncate()
                                            .text_size(px(12.5))
                                            .text_color(theme.text_muted)
                                            .child(directory),
                                    )
                                }),
                        )
                        .into_any_element(),
                );
            }
            // Overflowing rows wheel-scroll inside a bounded viewport; the
            // floating rail mirrors the model-list scrollbar treatment.
            card = card.child(
                div()
                    .id("mention-scroll-host")
                    .relative()
                    .on_hover(cx.listener(Self::on_popup_list_hover))
                    .child(
                        div()
                            .id("mention-list")
                            .max_h(px(312.0))
                            .flex()
                            .flex_col()
                            .overflow_y_scroll()
                            .track_scroll(&self.mention_scroll)
                            .children(rows),
                    )
                    .children(self.popup_scrollbar(
                        "mention-scrollbar",
                        &self.mention_scroll,
                        theme,
                        cx,
                    )),
            );
        }
        Some(crate::popover::full_width_menu_above(
            "file-mention-popup",
            card.into_any_element(),
            None,
        ))
    }
    // ---- slash commands ---------------------------------------------------

    /// The cwd the popup's skill entries resolve against (the project root
    /// derives from it).
    fn skills_cwd(&self, cx: &App) -> Option<String> {
        self.state.read(cx).skills_cwd()
    }

    /// Rebuild the merged candidate list from the cached sources: the
    /// loaded skills catalog plus the resolved provider's commands.
    fn rebuild_slash_candidates(&mut self) {
        let commands = self
            .slash
            .provider
            .as_ref()
            .and_then(|provider| self.slash_cache.get(provider))
            .cloned()
            .unwrap_or_default();
        self.slash.candidates = popup_candidates(&self.slash.skills, &commands);
    }

    /// Track the `/` token on every edit: open/refresh the popup, fetch the
    /// skills catalog once per cwd and the provider's command list once per
    /// provider, filter locally per keystroke.
    fn update_slash(&mut self, text: &str, cursor: usize, cx: &mut Context<Self>) {
        let token = slash_token(text, cursor);
        let still_dismissed = token.as_ref().is_some_and(|token| {
            self.slash.dismissed.as_ref().is_some_and(|(range, value)| {
                token.range == *range && text.get(range.clone()) == Some(value.as_str())
            })
        });
        if still_dismissed {
            self.slash.token = None;
            self.sync_mention_controls(cx);
            return;
        }
        self.slash.dismissed = None;
        let provider = self.pickers.read(cx).resolved(cx).provider;
        let cwd = self.skills_cwd(cx);
        let provider_changed = self.slash.provider != provider;
        // Skills are provider-agnostic: only a cwd change (or a fresh open)
        // refetches them — never a provider switch.
        let skills_stale = self.slash.skills_cwd != cwd;
        if token == self.slash.token && !provider_changed && !skills_stale {
            self.refilter_slash(cx);
            return;
        }
        self.slash.token = token.clone();
        self.slash.provider = provider.clone();
        if token.is_none() {
            self.slash.active = None;
            self.sync_mention_controls(cx);
            return;
        }
        // Show what is cached while anything stale is in flight.
        self.slash.error = None;
        self.rebuild_slash_candidates();
        self.refilter_slash(cx);

        // Skills: one ListSkills against the engine for this cwd. Unknown
        // on an older engine: the popup just shows commands.
        if skills_stale {
            self.slash.skills_request = self.slash.skills_request.wrapping_add(1);
            if let Some(engine) = self.state.read(cx).engine().cloned() {
                self.slash.skills_loading = true;
                let cwd = cwd.clone();
                let request = self.slash.skills_request;
                self.slash_skills_task = Some(cx.spawn(async move |this, cx| {
                    let mut params = serde_json::Map::new();
                    if let Some(cwd) = &cwd {
                        params.insert("cwd".into(), cwd.clone().into());
                    }
                    let result = engine
                        .client()
                        .call(methods::LIST_SKILLS, serde_json::Value::Object(params))
                        .await;
                    this.update(cx, |composer, cx| {
                        if composer.slash.skills_request != request {
                            return;
                        }
                        composer.slash.skills_loading = false;
                        match result {
                            Ok(value) => match serde_json::from_value::<SkillListing>(value) {
                                Ok(listing) => {
                                    composer.slash.skills = listing;
                                    composer.slash.skills_cwd = cwd;
                                    composer.rebuild_slash_candidates();
                                }
                                Err(err) => {
                                    tracing::warn!(%err, "skill listing decode failed")
                                }
                            },
                            Err(err) => {
                                tracing::debug!(%err, "skill listing failed");
                                composer.slash.error = Some(skills_error_message(&err));
                            }
                        }
                        composer.refilter_slash(cx);
                    })
                    .ok();
                }));
            } else {
                self.slash.skills_loading = false;
            }
        }

        // Commands: the provider's cached list, or one ListCommands fetch.
        if let Some(provider) = provider.filter(|provider| !self.slash_cache.contains_key(provider))
        {
            self.slash.request = self.slash.request.wrapping_add(1);
            self.slash.loading = true;
            let Some(engine) = self.state.read(cx).engine().cloned() else {
                self.slash.loading = false;
                return;
            };
            let request = self.slash.request;
            self.slash_task = Some(cx.spawn(async move |this, cx| {
                let params = serde_json::json!({ "providerId": provider });
                let result = engine.client().call(methods::LIST_COMMANDS, params).await;
                this.update(cx, |composer, cx| {
                    if composer.slash.request != request {
                        return;
                    }
                    composer.slash.loading = false;
                    match result {
                        Ok(value) => match serde_json::from_value::<Vec<SlashCommand>>(value) {
                            Ok(commands) => {
                                composer.slash_cache.insert(provider, commands);
                                composer.rebuild_slash_candidates();
                            }
                            Err(err) => tracing::warn!(%err, "slash command decode failed"),
                        },
                        Err(err) => {
                            tracing::debug!(%err, "slash command discovery failed");
                            composer.slash.error = Some(slash_error_message(&err));
                        }
                    }
                    composer.refilter_slash(cx);
                })
                .ok();
            }));
        } else {
            self.slash.loading = false;
        }
        cx.notify();
    }

    /// Re-rank the merged candidate list for the current query (pure local
    /// filter over the row titles).
    fn refilter_slash(&mut self, cx: &mut Context<Self>) {
        let query = self
            .slash
            .token
            .as_ref()
            .map(|t| t.query.clone())
            .unwrap_or_default();
        let labels: Vec<String> = self
            .slash
            .candidates
            .iter()
            .map(|candidate| candidate.filter_label())
            .collect();
        let mut ranked = crate::popover::filter_indices(&query, &labels);
        // Section order: skills before commands (stable, so within-section
        // match rank survives) — the menu renders contiguous groups under
        // their headers, and `filtered` stays the rendered row order.
        ranked
            .sort_by_key(|&ix| matches!(self.slash.candidates[ix], SlashCandidate::Command { .. }));
        self.slash.filtered = ranked;
        self.slash.active = (!self.slash.filtered.is_empty()).then_some(0);
        // A fresh query/reopen restarts the row stack at the top.
        reset_scroll_offset(&self.slash_scroll);
        self.sync_mention_controls(cx);
        cx.notify();
    }

    pub(super) fn move_slash(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.slash.active =
            crate::popover::menu_step(self.slash.active, self.slash.filtered.len(), delta);
        if let Some(active) = self.slash.active {
            // Keep the keyboard cursor visible in the scrolled row stack.
            self.slash_scroll.scroll_to_item(active);
        }
        self.sync_mention_controls(cx);
        cx.notify();
    }

    pub(super) fn dismiss_slash(&mut self, cx: &mut Context<Self>) {
        let dismissed = self.slash.token.as_ref().and_then(|token| {
            self.input
                .read(cx)
                .text()
                .get(token.range.clone())
                .map(|text| (token.range.clone(), text.to_string()))
        });
        self.reset_slash(dismissed, cx);
        cx.notify();
    }

    pub(super) fn accept_slash(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.slash.token.clone() else {
            return;
        };
        let Some(candidate) = self
            .slash
            .active
            .and_then(|active| self.slash.filtered.get(active))
            .and_then(|&ix| self.slash.candidates.get(ix))
            .cloned()
        else {
            return;
        };
        // The title is the fill: `/compact` for a command, `/skill <name>`
        // for a skill — ready for extra instructions and submit.
        let title = candidate.title();
        // `/compact` takes no arguments, so a selection sends it right
        // away instead of staging it in the input for review; anything
        // else (skills, argument-taking commands) still fills.
        let send_now = matches!(super::slash::parse(&title), super::slash::Parsed::Compact);
        self.reset_slash(None, cx);
        if send_now {
            // Dispatch the completed command directly. Writing it into the
            // input first makes the command flash in the composer and also
            // causes failed compact requests to be restored as draft text.
            self.submit_text(title, cx);
        } else {
            self.input.update(cx, |input, cx| {
                input.replace_plain_token(token.range, &title, cx)
            });
        }
        cx.notify();
    }

    /// Tear down the slash completion (mirrors [`Self::reset_mention`]).
    pub(super) fn reset_slash(
        &mut self,
        dismissed: Option<(Range<usize>, String)>,
        cx: &mut Context<Self>,
    ) {
        let request = self.slash.request.wrapping_add(1);
        let skills_request = self.slash.skills_request.wrapping_add(1);
        self.slash_task = None;
        self.slash_skills_task = None;
        self.slash = SlashState {
            request,
            skills_request,
            dismissed,
            provider: self.slash.provider.clone(),
            ..SlashState::default()
        };
        self.sync_mention_controls(cx);
    }

    pub(super) fn render_slash_popup(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        // Only while a slash token is active.
        self.slash.token.as_ref()?;
        let candidates = self.slash.candidates.as_slice();
        // Full pill width at the mention card's height budget — both composer
        // completions share the same surface shape.
        let mut card = crate::popover::popover_card(theme)
            .w_full()
            .max_h(px(320.0))
            .overflow_hidden()
            // GPUI dispatches this captured stream while the thumb is
            // dragged, including when the pointer has left the popup.
            .on_drag_move(cx.listener(Self::on_popup_bar_drag_move))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.dismiss_slash(cx)));
        let fetching = self.slash.loading || self.slash.skills_loading;
        if fetching && candidates.is_empty() {
            card = card.child(crate::popover::skeleton_rows(
                "slash-loading",
                theme,
                3,
                cx.entity_id(),
                cx,
            ));
        } else if let Some(error) = self.slash.error.clone() {
            card = card.child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.danger_muted)
                    .child(error),
            );
        } else if self.slash.filtered.is_empty() {
            card = card.child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted)
                    .child(if candidates.is_empty() {
                        "No slash commands or skills available"
                    } else {
                        "No matching commands or skills"
                    }),
            );
        } else {
            let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(self.slash.filtered.len() + 2);
            // Section headers ("Skills" / "Commands") introduce each
            // contiguous group, reference-menu style; `filtered` is already
            // section-ordered.
            let mut in_skills_section: Option<bool> = None;
            for (row_ix, &candidate_ix) in self.slash.filtered.iter().enumerate() {
                let Some(candidate) = candidates.get(candidate_ix) else {
                    continue;
                };
                let is_skill = matches!(candidate, SlashCandidate::Skill { .. });
                if in_skills_section != Some(is_skill) {
                    in_skills_section = Some(is_skill);
                    rows.push(slash_section_header(
                        theme,
                        if is_skill { "Skills" } else { "Commands" },
                    ));
                }
                let selected = self.slash.active == Some(row_ix);
                let label: SharedString = candidate.row_label().into();
                let description: SharedString = candidate.description().into();
                let root_tag = candidate.root_tag();
                rows.push(
                    crate::popover::menu_row(theme, selected, format!("slash-result-{row_ix}"))
                        .id(("slash-result", row_ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.slash.active = Some(row_ix);
                            this.accept_slash(cx);
                        }))
                        // Label and description share one line (reference
                        // style): bold name, muted truncating description,
                        // the source root as a quiet right-aligned tag.
                        .child(
                            crate::icons::icon(if is_skill {
                                crate::icons::CUBE
                            } else {
                                crate::icons::COMMAND
                            })
                            .size(px(15.0))
                            .flex_none()
                            .text_color(if is_skill {
                                theme.accent
                            } else {
                                theme.text_muted
                            }),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(crate::typography::ui_rems(12.5))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(if is_skill { theme.accent } else { theme.text })
                                .child(label),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .overflow_hidden()
                                .truncate()
                                .text_size(crate::typography::ui_rems(12.5))
                                .text_color(theme.text_muted.opacity(0.75))
                                .child(description),
                        )
                        .when_some(root_tag, |row, tag| {
                            row.child(
                                div()
                                    .flex_none()
                                    .pl(px(8.0))
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.text_muted.opacity(0.65))
                                    .child(SharedString::from(tag)),
                            )
                        })
                        .into_any_element(),
                );
            }
            // Overflowing rows wheel-scroll inside a bounded viewport; the
            // floating rail mirrors the model-list scrollbar treatment.
            card = card.child(
                div()
                    .id("slash-scroll-host")
                    .relative()
                    .on_hover(cx.listener(Self::on_popup_list_hover))
                    .child(
                        div()
                            .id("slash-list")
                            .max_h(px(312.0))
                            .flex()
                            .flex_col()
                            .overflow_y_scroll()
                            .track_scroll(&self.slash_scroll)
                            .children(rows),
                    )
                    .children(self.popup_scrollbar(
                        "slash-scrollbar",
                        &self.slash_scroll,
                        theme,
                        cx,
                    )),
            );
        }
        // Full pill width above the composer, matching the file-mention popup.
        Some(crate::popover::full_width_menu_above(
            "slash-popup",
            card.into_any_element(),
            None,
        ))
    }

    /// The floating scrollbar rail for a composer popup's scroll host (the
    /// model-list treatment). Callers pass the id and that popup's scroll
    /// handle; the hover/drag interaction state is shared.
    fn popup_scrollbar(
        &self,
        id: &'static str,
        scroll: &gpui::ScrollHandle,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let metrics = self.popup_bar.metrics(scroll)?;
        Some(
            self.popup_bar
                .render_rail(theme, metrics)?
                .id(id)
                .on_hover(cx.listener(Self::on_popup_bar_hover))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(Self::on_popup_bar_mouse_down),
                )
                .on_drag(crate::popover::MenuScrollbarDrag, |_, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| crate::popover::MenuScrollbarDragGhost)
                })
                .on_mouse_up_out(
                    gpui::MouseButton::Left,
                    cx.listener(Self::on_popup_bar_mouse_up),
                )
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(Self::on_popup_bar_mouse_up),
                )
                .into_any_element(),
        )
    }

    /// The popup whose rows a scrollbar drag is moving — the tokens are
    /// mutually exclusive, so at most one exists.
    fn active_popup_scroll(&self) -> Option<gpui::ScrollHandle> {
        if self.slash.token.is_some() {
            Some(self.slash_scroll.clone())
        } else if self.mention.token.is_some() {
            Some(self.mention_scroll.clone())
        } else {
            None
        }
    }

    fn on_popup_list_hover(
        &mut self,
        hovered: &bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.popup_bar.set_list_hovered(*hovered) {
            cx.notify();
        }
    }

    fn on_popup_bar_hover(&mut self, hovered: &bool, _window: &mut Window, cx: &mut Context<Self>) {
        if self.popup_bar.set_bar_hovered(*hovered) {
            cx.notify();
        }
    }

    fn on_popup_bar_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scroll) = self.active_popup_scroll() else {
            return;
        };
        if !self.popup_bar.begin_press(&scroll, event.position.y) {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn on_popup_bar_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<crate::popover::MenuScrollbarDrag>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scroll) = self.active_popup_scroll() else {
            return;
        };
        if self.popup_bar.drag_to(&scroll, event.event.position.y) {
            cx.notify();
        }
    }

    fn on_popup_bar_mouse_up(
        &mut self,
        _event: &gpui::MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.popup_bar.end_press();
        cx.notify();
    }
}

/// A slash popup section caption ("Skills" / "Commands"), reference-menu
/// style: small, quiet, aligned with the rows' content column.
fn slash_section_header(theme: &Theme, label: &'static str) -> gpui::AnyElement {
    div()
        .px(px(12.0))
        .pt(px(8.0))
        .pb(px(3.0))
        .text_size(crate::typography::ui_rems(10.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text_muted.opacity(0.6))
        .child(SharedString::from(label))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mention_token_requires_a_token_boundary_and_tracks_full_token() {
        assert_eq!(
            mention_token("Fix @src/com", 12),
            Some(MentionToken {
                range: 4..12,
                query: "src/com".into(),
            })
        );
        assert!(mention_token("mail@example.com", 16).is_none());
        assert!(mention_token("word@file", 9).is_none());
        assert!(mention_token("path/@file", 10).is_none());
        assert_eq!(
            mention_token("See (@lib", 9).map(|token| token.range),
            Some(5..9)
        );
    }

    #[test]
    fn slash_token_only_opens_the_prompt() {
        assert_eq!(
            slash_token("/comp", 5),
            Some(MentionToken {
                range: 0..5,
                query: "comp".into(),
            })
        );
        // Token range spans the whole command word even mid-cursor.
        assert_eq!(
            slash_token("/compact now", 3),
            Some(MentionToken {
                range: 0..8,
                query: "co".into(),
            })
        );
        // Not at offset 0 → prose, not a command.
        assert!(slash_token("run /compact", 12).is_none());
        // Cursor past the command word (typing the argument) → closed.
        assert!(slash_token("/goal ship it", 10).is_none());
        // A typed absolute path is not a command.
        assert!(slash_token("/usr/bin", 8).is_none());
        // Bare "/" with cursor at 0 → closed; cursor after it → open-all.
        assert!(slash_token("/", 0).is_none());
        assert_eq!(slash_token("/", 1).map(|t| t.query), Some(String::new()));
    }

    #[test]
    fn dismissed_mentions_reject_stale_responses() {
        let mut state = FileMentionState {
            token: mention_token("@src", 4),
            request: 7,
            ..FileMentionState::default()
        };
        assert!(mention_response_is_current(&state, 7));
        state.request += 1;
        state.token = None;
        assert!(!mention_response_is_current(&state, 7));
        assert!(!mention_response_is_current(&state, 8));
    }
}
