//! The find-file palette (ticket 09, ⌘P): quick fuzzy lookup of
//! workspace-relative paths through the engine's existing `SearchFiles` —
//! the same bounded, hidden-inclusive, `.git`-exclusive walk the composer's
//! `@` popup rides. Files open (or reuse) their tab; folders reveal in the
//! File tree. The palette never bypasses the sidebar's fences: results are
//! root-relative and join against the root they were searched under, so an
//! outside-root symlink stays the external-open row the tree already shows.

use super::*;

use std::path::PathBuf;

use holt_proto::FileSearchMatch;

/// The search owner snapshotted with a request: the RPC selector plus the
/// absolute root results join against — one owner, selector and root from
/// the same selection read so they can never disagree. A reply that
/// outlives a query or owner change is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileLookupScope {
    selector: crate::state::SearchSelector,
    root: PathBuf,
}

/// The open palette (a ⌘K-style surface, summoned by ⌘P or the tree
/// column's search row).
pub(super) struct FileLookup {
    search: Entity<ComposerInput>,
    results: Vec<FileSearchMatch>,
    active: Option<usize>,
    request: u64,
    task: Option<Task<()>>,
    loading: bool,
    error: Option<SharedString>,
    scope: Option<FileLookupScope>,
    /// Tracked on the card (`track_focus`) — puts the palette on the
    /// keyboard dispatch path so ↑↓/⏎/esc reach `file_lookup_key` while the
    /// search input holds focus (the structure every working picker uses).
    focus: gpui::FocusHandle,
    list_scroll: gpui::ScrollHandle,
    focus_pending: bool,
    _search_events: Subscription,
}

/// The join of a root-relative result against the root it was searched
/// under — the absolute path every downstream action (tab open, tree
/// reveal, chip attach) binds to. Pure; unit-tested for spaces/Unicode.
fn absolute_result(root: &std::path::Path, relative: &str) -> String {
    root.join(relative).to_string_lossy().into_owned()
}

impl Shell {
    /// The scope the palette searches: the selected Chat's working
    /// directory, else the selected Space's path (the ticket's owner rule —
    /// the same selector/root pair the composer's `@` popup rides).
    fn file_lookup_scope(&self, cx: &App) -> Option<FileLookupScope> {
        let state = self.state.read(cx);
        Some(FileLookupScope {
            selector: state.search_selector()?,
            root: state.search_root()?,
        })
    }

    pub(super) fn open_file_lookup(&mut self, cx: &mut Context<Self>) {
        // "PaletteSearch" context: navigation keys stay unbound so ↑↓/⏎
        // bubble to the palette card instead of moving the text caret.
        let search =
            cx.new(|cx| ComposerInput::with_context("Search files by path…", "PaletteSearch", cx));
        let search_events = cx.subscribe(&search, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                this.on_file_lookup_query(cx);
            }
        });
        self.file_lookup = Some(FileLookup {
            search,
            results: Vec::new(),
            active: None,
            request: 0,
            task: None,
            loading: false,
            error: None,
            scope: self.file_lookup_scope(cx),
            focus: cx.focus_handle(),
            list_scroll: gpui::ScrollHandle::new(),
            focus_pending: true,
            _search_events: search_events,
        });
        cx.notify();
    }

    pub(super) fn close_file_lookup(&mut self, cx: &mut Context<Self>) {
        if self.file_lookup.take().is_some() {
            cx.notify();
        }
    }

    /// ⌘P / the tree column's search row: dismiss when open, summon when
    /// not — the ⌘K add-space palette's toggle behavior.
    pub(super) fn toggle_file_lookup(&mut self, cx: &mut Context<Self>) {
        if self.file_lookup.is_some() {
            self.close_file_lookup(cx);
        } else {
            self.open_file_lookup(cx);
        }
    }

    /// An owner switch (chat/space selection) re-runs the search: results
    /// never outlive the selection they were found under. The in-flight
    /// reply dies on the request counter; this starts the fresh one.
    /// Called from the palette's render — the shell repaints on every
    /// state change, so no separate observer is needed.
    pub(super) fn refresh_file_lookup_if_owner_changed(&mut self, cx: &mut Context<Self>) {
        if self.file_lookup.is_none() {
            return;
        }
        let scope_now = self.file_lookup_scope(cx);
        let scope_then = self.file_lookup.as_ref().and_then(|l| l.scope.clone());
        if scope_now != scope_then {
            self.on_file_lookup_query(cx);
        }
    }

    /// Run the search for the current query under the current owner. Every
    /// query edit and every owner change funnels here; the request counter
    /// invalidates replies that were already in flight.
    pub(super) fn on_file_lookup_query(&mut self, cx: &mut Context<Self>) {
        let query = {
            let Some(lookup) = self.file_lookup.as_mut() else {
                return;
            };
            lookup.request = lookup.request.wrapping_add(1);
            lookup.task = None;
            lookup.active = None;
            lookup.error = None;
            lookup.search.read(cx).text().trim().to_string()
        };
        let scope = self.file_lookup_scope(cx);
        let Some(lookup) = self.file_lookup.as_mut() else {
            return;
        };
        lookup.scope = scope.clone();
        if query.is_empty() {
            lookup.loading = false;
            lookup.results.clear();
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            lookup.loading = false;
            lookup.results.clear();
            lookup.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let Some(scope) = scope else {
            lookup.loading = false;
            lookup.results.clear();
            lookup.error = Some("Select a space to search its files.".into());
            cx.notify();
            return;
        };
        lookup.loading = true;
        let request = lookup.request;
        let (chat_id, space_id) = scope.selector.ids();
        let chat_id = chat_id.map(str::to_string);
        let space_id = space_id.map(str::to_string);
        lookup.task = Some(cx.spawn(async move |this, cx| {
            // The shared `SearchFiles` round trip (debounce, call, one cold-
            // start retry, decode) — the same plumbing the `@` popup rides.
            let result = crate::attachments::search_files(
                &engine,
                cx.background_executor(),
                &query,
                chat_id.as_deref(),
                space_id.as_deref(),
            )
            .await;
            this.update(cx, |this, cx| {
                let Some(lookup) = this.file_lookup.as_mut() else {
                    return;
                };
                // The query or the owner moved on — drop the stale reply.
                if lookup.request != request {
                    return;
                }
                lookup.loading = false;
                lookup.task = None;
                match result {
                    Ok(results) => {
                        lookup.error = None;
                        lookup.active = (!results.is_empty()).then_some(0);
                        lookup.results = results;
                        lookup.list_scroll.set_offset(gpui::Point::default());
                    }
                    Err(message) => {
                        lookup.results.clear();
                        lookup.active = None;
                        lookup.error = Some(message);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Palette keys (bubbling from the focused search input): ↑↓ navigate,
    /// ⏎ opens the highlighted row, esc closes.
    pub(super) fn file_lookup_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        // The card stays mounted through teardown — keys must not drive a
        // dying palette.
        if self.file_lookup.is_none() {
            return;
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => self.close_file_lookup(cx),
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = self
                    .file_lookup
                    .as_ref()
                    .map(|lookup| lookup.results.len())
                    .unwrap_or(0);
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                if let Some(lookup) = self.file_lookup.as_mut() {
                    lookup.active = popover::menu_step(lookup.active, count, delta);
                    if let Some(active) = lookup.active {
                        lookup.list_scroll.scroll_to_item(active);
                    }
                }
                cx.notify();
            }
            popover::MenuKey::Enter => self.accept_file_lookup(cx),
            popover::MenuKey::Other | popover::MenuKey::ModEnter | popover::MenuKey::Backspace => {}
        }
    }

    /// Open the highlighted row: a file opens (or reuses) its tab; a folder
    /// reveals in the File tree — never as editable text. The result binds
    /// to the root it was searched under, even if the selection moved while
    /// the palette was open (the same binding rule as `@` mentions).
    pub(super) fn accept_file_lookup(&mut self, cx: &mut Context<Self>) {
        let (result, root) = {
            let Some(lookup) = self.file_lookup.as_ref() else {
                return;
            };
            let result = lookup
                .active
                .and_then(|active| lookup.results.get(active))
                .cloned();
            let root = lookup.scope.as_ref().map(|scope| scope.root.clone());
            (result, root)
        };
        let Some(result) = result else {
            return;
        };
        let Some(root) = root else {
            return;
        };
        let absolute = absolute_result(&root, &result.path);
        self.close_file_lookup(cx);
        if result.is_dir {
            self.reveal_file_tree_path(&absolute, cx);
        } else {
            self.open_file(absolute, None, false, cx);
        }
        cx.notify();
    }

    /// Reveal a folder in the File tree (ticket 09): show the tree column
    /// if hidden, then walk the ancestor chain and select the row.
    pub(super) fn reveal_file_tree_path(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.file_tree_visible {
            self.file_tree_visible = true;
            self.file_tree_tween = Some(WidthTween::new(0.0, self.file_tree_target(cx)));
        }
        let tree = self.file_tree_panel(cx);
        tree.update(cx, |tree, cx| tree.reveal_path(path, cx));
        cx.notify();
    }

    /// The palette card: search bar over the result rows, with loading, no
    /// matches, and errors as separate states.
    pub(super) fn render_file_lookup_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        {
            let lookup = self.file_lookup.as_mut()?;
            if std::mem::take(&mut lookup.focus_pending) {
                let handle = gpui::Focusable::focus_handle(lookup.search.read(cx), cx).clone();
                window.focus(&handle, cx);
            }
        }
        // An owner switch re-runs the search: results never outlive the
        // selection they were found under.
        self.refresh_file_lookup_if_owner_changed(cx);

        let (search, results, active, loading, error, scope, focus, list_scroll) = {
            let lookup = self.file_lookup.as_ref()?;
            (
                lookup.search.clone(),
                lookup.results.clone(),
                lookup.active,
                lookup.loading,
                lookup.error.clone(),
                lookup.scope.clone(),
                lookup.focus.clone(),
                lookup.list_scroll.clone(),
            )
        };
        let query_empty = search.read(cx).text().trim().is_empty();
        let card_radius = 14.0;

        // ── search bar: ⌘P chip · input · esc chip (the ⌘K bar's shape).
        let key_chip = || {
            div()
                .h(px(22.0))
                .px(px(6.0))
                .rounded(px(5.0))
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(2.0))
                .bg(crate::theme::ink(0.05))
                .text_size(crate::typography::ui_rems(11.0))
                .font_family(theme.font_mono.clone())
                .text_color(theme.text_muted.opacity(0.7))
        };
        let input_row = div()
            .h(px(46.0))
            .flex_none()
            .rounded_t(px(card_radius))
            .pl(px(12.0))
            .pr(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .bg(popover::band())
            .border_b_1()
            .border_color(crate::theme::hairline(0.06))
            .child(
                key_chip()
                    .child(
                        icon(icons::COMMAND)
                            .size(px(11.0))
                            .text_color(theme.text_muted.opacity(0.7)),
                    )
                    .child(SharedString::from("P")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(crate::typography::ui_rems(14.0))
                    .child(search.into_any_element()),
            )
            .child(
                key_chip()
                    .id("file-lookup-esc")
                    .cursor_pointer()
                    .hover(|s| s.bg(crate::theme::ink(0.09)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.close_file_lookup(cx);
                    }))
                    .child(SharedString::from("esc")),
            );

        // ── body: one of the four states, or the rows.
        let body: AnyElement = if let Some(error) = error {
            popover::error_row(&theme, &error).into_any_element()
        } else if query_empty {
            let hint = match scope.as_ref().map(|scope| scope.root.clone()) {
                Some(root) => {
                    let name = root
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| root.to_string_lossy().into_owned());
                    format!(
                        "Type to search every file and folder in {name} — hidden entries included."
                    )
                }
                None => "Select a space to search its files.".to_string(),
            };
            div()
                .px(px(12.0))
                .py(px(12.0))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_faint)
                .child(hint)
                .into_any_element()
        } else if loading && results.is_empty() {
            popover::skeleton_rows("file-lookup-loading", &theme, 5, cx.entity_id(), cx)
                .into_any_element()
        } else if results.is_empty() {
            div()
                .px(px(12.0))
                .py(px(12.0))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted)
                .child("No matching paths")
                .into_any_element()
        } else {
            let root = scope.as_ref().map(|scope| scope.root.clone());
            let mut rows: Vec<AnyElement> = Vec::with_capacity(results.len());
            for (ix, result) in results.iter().enumerate() {
                let selected = active == Some(ix);
                let absolute = root
                    .as_ref()
                    .map(|root| absolute_result(root, &result.path))
                    .unwrap_or_else(|| result.path.clone());
                let (name, directory) = result
                    .path
                    .rsplit_once('/')
                    .map(|(directory, name)| (name.to_string(), directory.to_string()))
                    .unwrap_or((result.path.clone(), String::new()));
                rows.push(
                    popover::menu_row(&theme, selected, format!("file-lookup-row-{ix}"))
                        .id(("file-lookup-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(lookup) = this.file_lookup.as_mut() {
                                lookup.active = Some(ix);
                            }
                            this.accept_file_lookup(cx);
                        }))
                        .tooltip(move |_, cx| {
                            cx.new(|_| crate::image_viewer::ViewerTooltip(absolute.clone().into()))
                                .into()
                        })
                        .child(
                            icon(if result.is_dir {
                                icons::FOLDER
                            } else {
                                icons::DOCUMENT
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
                        // The complete workspace-relative path — the row's
                        // claim, muted beside the name (truncating).
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .overflow_hidden()
                                .truncate()
                                .text_size(px(12.5))
                                .text_color(theme.text_muted)
                                .child(result.path.clone()),
                        )
                        .when(!directory.is_empty() || result.is_dir, |row| {
                            row.child(
                                div()
                                    .flex_none()
                                    .pl(px(6.0))
                                    .text_size(crate::typography::ui_rems(10.5))
                                    .text_color(if result.is_dir {
                                        theme.text_muted.opacity(0.8)
                                    } else {
                                        theme.text_muted.opacity(0.55)
                                    })
                                    .child(if result.is_dir { "folder" } else { "file" }),
                            )
                        })
                        .into_any_element(),
                );
            }
            div()
                .id("file-lookup-list")
                .max_h(px(312.0))
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(&list_scroll)
                .children(rows)
                .into_any_element()
        };

        // ── footer: the keyboard legend (the ⌘K palette's quiet caption).
        let footer = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(14.0))
            .px(px(12.0))
            .h(px(28.0))
            .border_t_1()
            .border_color(crate::theme::hairline(0.06))
            .text_size(crate::typography::ui_rems(11.0))
            .text_color(theme.text_faint)
            .child(popover::kbd_hint(&theme, "↑↓"))
            .child(SharedString::from("navigate"))
            .child(popover::kbd_hint(&theme, "↵"))
            .child(SharedString::from("open"))
            .child(div().flex_1())
            .child(SharedString::from("folders reveal in the file tree"));

        let card = popover::palette_card(&theme, px(560.0), card_radius)
            .id("file-lookup-palette")
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                this.file_lookup_key(event, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.close_file_lookup(cx);
            }))
            .child(input_row)
            .child(body)
            .child(footer)
            .into_any_element();
        Some(popover::modal_glass(
            "file-lookup-dialog",
            viewport,
            card,
            card_radius,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_space(id: &str, path: &str) -> holt_proto::Space {
        holt_proto::Space {
            id: id.into(),
            device_id: "device".into(),
            path: path.into(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn results_join_their_root_keeping_spaces_and_unicode() {
        assert_eq!(
            absolute_result(&PathBuf::from("/work/holt"), "src/a file.rs"),
            "/work/holt/src/a file.rs"
        );
        assert_eq!(
            absolute_result(&PathBuf::from("/work/holt"), "ünïcode/dïr"),
            "/work/holt/ünïcode/dïr"
        );
        // A nested absolute-looking relative still joins (never replaces).
        assert_eq!(
            absolute_result(&PathBuf::from("/r"), "deep/pkg/mod.rs"),
            "/r/deep/pkg/mod.rs"
        );
    }

    /// Ticket 09's accept paths at the Shell seam: a FILE result opens a
    /// contents tab bound to the searched root's absolute join; a FOLDER
    /// result starts the tree reveal instead. The palette closes on accept.
    #[gpui::test]
    fn accepting_results_opens_files_and_reveals_folders(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.selected_space = Some("space-1".into());
            state
        });
        let (shell, cx) = cx.add_window_view(|_, cx| {
            let mut shell = Shell::new(
                state,
                EngineBootConfig {
                    data_dir: std::env::temp_dir(),
                },
                cx,
            );
            shell.route = Route::Chat;
            shell
        });

        let scope = FileLookupScope {
            selector: crate::state::SearchSelector::Space("space-1".into()),
            root: PathBuf::from("/tmp/space-1"),
        };

        // A file result opens a tab under the space's file state.
        cx.update(|_, cx| {
            shell.update(cx, |shell, cx| {
                shell.open_file_lookup(cx);
                let lookup = shell.file_lookup.as_mut().expect("open");
                lookup.scope = Some(scope.clone());
                lookup.results = vec![FileSearchMatch {
                    path: "src/a file.rs".into(),
                    is_dir: false,
                }];
                lookup.active = Some(0);
                shell.accept_file_lookup(cx);
                assert!(shell.file_lookup.is_none(), "accept closes the palette");
                let tabs = shell.file_state.space("space-1").expect("space state");
                assert_eq!(tabs.tabs.len(), 1);
                assert_eq!(tabs.tabs[0].path, "/tmp/space-1/src/a file.rs");
            });
        });

        // A folder result reveals in the tree: the panel exists, the column
        // shows, and the reveal is walking (no engine in this test, so it
        // waits on the root listing) — never a new tab.
        cx.update(|_, cx| {
            shell.update(cx, |shell, cx| {
                let tabs_before = shell
                    .file_state
                    .space("space-1")
                    .map(|tabs| tabs.tabs.len())
                    .unwrap_or(0);
                shell.open_file_lookup(cx);
                let lookup = shell.file_lookup.as_mut().expect("open");
                lookup.scope = Some(scope);
                lookup.results = vec![FileSearchMatch {
                    path: "src/deep".into(),
                    is_dir: true,
                }];
                lookup.active = Some(0);
                shell.accept_file_lookup(cx);
                assert!(shell.file_lookup.is_none());
                assert!(shell.file_tree_visible, "the tree column shows");
                let tabs_after = shell
                    .file_state
                    .space("space-1")
                    .map(|tabs| tabs.tabs.len())
                    .unwrap_or(0);
                assert_eq!(tabs_after, tabs_before, "a folder reveal opens no tab");
                let tree = shell.file_tree.as_ref().expect("panel created");
                tree.read_with(cx, |tree, _| {
                    assert_eq!(tree.pending_reveal_path(), Some("/tmp/space-1/src/deep"));
                });
            });
        });
    }
    /// Ticket 09's staleness rules at the Shell seam: every query edit (and
    /// every owner switch, below) bumps the request counter, so replies that
    /// were already in flight for an older query or owner die on arrival.
    #[gpui::test]
    fn owner_and_query_changes_invalidate_in_flight_replies(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.spaces = vec![
                test_space("space-1", "/tmp/space-1"),
                test_space("space-2", "/tmp/space-2"),
            ];
            state.selected_space = Some("space-1".into());
            state
        });
        let (shell, cx) = cx.add_window_view(|_, cx| {
            let mut shell = Shell::new(
                state,
                EngineBootConfig {
                    data_dir: std::env::temp_dir(),
                },
                cx,
            );
            shell.route = Route::Chat;
            shell
        });

        cx.update(|_, cx| {
            shell.update(cx, |shell, cx| {
                shell.open_file_lookup(cx);
                // Typing a query funnels through the query runner: the
                // request counter advances even though no engine answers in
                // this test (the error row takes that path instead).
                shell.on_file_lookup_query(cx);
                let first = shell.file_lookup.as_ref().expect("open").request;
                assert!(first > 0);
                shell.on_file_lookup_query(cx);
                assert_eq!(
                    shell.file_lookup.as_ref().unwrap().request,
                    first + 1,
                    "a query edit invalidates the in-flight reply"
                );
                // The scope snapshot settled on the opening owner.
                assert_eq!(
                    shell.file_lookup.as_ref().unwrap().scope,
                    shell.file_lookup_scope(cx)
                );
            });
        });

        // Switching the owner (Space selection) re-runs the search under the
        // new owner — and only once: the snapshot follows, so a settled
        // palette does not re-query on every render.
        cx.update(|_, cx| {
            shell.update(cx, |shell, cx| {
                shell.state.update(cx, |state, cx| {
                    state.select_space(Some("space-2".into()), cx);
                });
                let settled = shell.file_lookup.as_ref().expect("open").request;
                shell.refresh_file_lookup_if_owner_changed(cx);
                assert_eq!(
                    shell.file_lookup.as_ref().unwrap().request,
                    settled + 1,
                    "an owner switch re-runs the search"
                );
                let requeries = shell.file_lookup.as_ref().unwrap().request;
                shell.refresh_file_lookup_if_owner_changed(cx);
                assert_eq!(
                    shell.file_lookup.as_ref().unwrap().request,
                    requeries,
                    "a settled scope does not re-query again"
                );
            });
        });
    }
}
