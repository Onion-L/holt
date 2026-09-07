//! Session context menu and clipboard commands for the session under the pointer.

use super::*;
use gpui::{Bounds, FocusHandle, Size};
use holt_doc::{MessagePart, MessageRole, SessionMessageEntry, TranscriptFrame};

const COPY_ROW: usize = 2;
const MENU_INSET: f32 = 5.0;
const SEPARATOR_HEIGHT: f32 = 9.0;

#[derive(Clone)]
pub(super) struct ChatMenuState {
    chat_id: String,
    position: Point<Pixels>,
    highlighted: Option<usize>,
    copy_open: bool,
    copy_highlighted: Option<usize>,
    focus: FocusHandle,
    return_focus: Option<FocusHandle>,
}

struct MenuLayout {
    root: Bounds<Pixels>,
    copy: Bounds<Pixels>,
    row_height: Pixels,
}

impl MenuLayout {
    fn new(position: Point<Pixels>, viewport: Size<Pixels>, font_size: Pixels) -> Self {
        let row_height = (font_size * 1.5 + px(12.0)).max(px(30.0));
        let root_size = Size::new(
            px(216.0).max(font_size * 15.0),
            row_height * 4.0 + px(MENU_INSET * 2.0 + SEPARATOR_HEIGHT),
        );
        let copy_size = Size::new(
            px(240.0).max(font_size * 18.0),
            row_height * 3.0 + px(MENU_INSET * 2.0),
        );
        let clamp = |position: Point<Pixels>, size: Size<Pixels>| {
            Point::new(
                position.x.clamp(
                    px(8.0),
                    (viewport.width - size.width - px(8.0)).max(px(8.0)),
                ),
                position.y.clamp(
                    px(8.0),
                    (viewport.height - size.height - px(8.0)).max(px(8.0)),
                ),
            )
        };
        let root = Bounds::new(clamp(position, root_size), root_size);
        let copy_x = if root.right() + copy_size.width <= viewport.width - px(8.0) {
            root.right()
        } else {
            root.left() - copy_size.width
        };
        let copy = Bounds::new(
            clamp(
                Point::new(copy_x, root.top() + row_height * COPY_ROW as f32),
                copy_size,
            ),
            copy_size,
        );
        Self {
            root,
            copy,
            row_height,
        }
    }
}

impl Shell {
    pub(super) fn open_chat_menu(
        &mut self,
        chat_id: String,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let return_focus = self
            .chat_menu
            .get()
            .filter(|menu| menu.focus.is_focused(window))
            .map(|menu| menu.return_focus.clone())
            .unwrap_or_else(|| window.focused(cx));
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.chat_menu.open(ChatMenuState {
            chat_id,
            position,
            highlighted: None,
            copy_open: false,
            copy_highlighted: None,
            focus,
            return_focus,
        });
        cx.notify();
    }

    fn highlight_chat_menu(&mut self, index: usize, copy: bool, cx: &mut Context<Self>) {
        if let Some(menu) = self.chat_menu.open_mut() {
            if copy {
                if menu.copy_highlighted == Some(index) {
                    return;
                }
                menu.copy_highlighted = Some(index);
            } else {
                if menu.highlighted == Some(index) && menu.copy_open == (index == COPY_ROW) {
                    return;
                }
                menu.highlighted = Some(index);
                menu.copy_open = index == COPY_ROW;
                menu.copy_highlighted = None;
            }
            cx.notify();
        }
    }

    fn activate_chat_menu(&mut self, index: usize, copy: bool, cx: &mut Context<Self>) {
        let Some(menu) = self.chat_menu.open_mut() else {
            return;
        };
        let chat_id = menu.chat_id.clone();
        if copy {
            self.chat_copy_task = None;
            match index {
                0 => self.copy_chat_directory(&chat_id, cx),
                1 => self.copy_holt_conversation_link(&chat_id, cx),
                2 => self.copy_chat_markdown(&chat_id, cx),
                _ => {}
            }
        } else {
            match index {
                0 => self.open_rename_chat(chat_id, cx),
                1 => self.request_archive_chat(chat_id, cx),
                COPY_ROW => {
                    menu.copy_open = true;
                    menu.copy_highlighted = Some(0);
                }
                3 => {
                    self.close_chat_menu(cx);
                    self.delete_confirm = Some(chat_id);
                }
                _ => {}
            }
        }
        cx.notify();
    }

    fn chat_menu_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let Some(menu) = self.chat_menu.open_mut() else {
            return;
        };
        match event.keystroke.key.as_str() {
            "escape" => self.close_chat_menu(cx),
            "left" if menu.copy_open => {
                menu.copy_open = false;
                menu.copy_highlighted = None;
            }
            "right" if menu.highlighted == Some(COPY_ROW) => {
                menu.copy_open = true;
                menu.copy_highlighted = Some(0);
            }
            "up" | "down" => {
                let step = if event.keystroke.key == "up" { -1 } else { 1 };
                if menu.copy_open {
                    menu.copy_highlighted = popover::menu_step(menu.copy_highlighted, 3, step);
                } else {
                    menu.highlighted = popover::menu_step(menu.highlighted, 4, step);
                }
            }
            "enter" | "space" => {
                let copy = menu.copy_open;
                let index = if copy {
                    menu.copy_highlighted
                } else {
                    menu.highlighted
                };
                if let Some(index) = index {
                    self.activate_chat_menu(index, copy, cx);
                }
            }
            _ => {}
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn render_chat_menu(
        &mut self,
        viewport: Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(menu) = self.chat_menu.get().cloned() else {
            return Vec::new();
        };
        let closing = self.chat_menu.closing_since();
        if closing.is_some() && menu.focus.is_focused(window) {
            if let Some(focus) = &menu.return_focus {
                window.focus(focus, cx);
            } else {
                window.blur();
            }
        }
        let theme = Theme::of(cx).clone();
        let layout = MenuLayout::new(
            menu.position,
            viewport,
            crate::typography::ui_rems(13.0).to_pixels(window.rem_size()),
        );
        let copy_bounds = menu.copy_open.then_some(layout.copy);
        let mut root = popover::popover_card(&theme)
            .id("chat-menu-root")
            .w(layout.root.size.width)
            .track_focus(&menu.focus)
            .on_key_down(cx.listener(|this, event, _, cx| this.chat_menu_key(event, cx)))
            .on_mouse_down_out(cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                if !copy_bounds.is_some_and(|bounds| bounds.contains(&event.position)) {
                    this.close_chat_menu(cx);
                }
            }))
            .flex()
            .flex_col();
        for (index, (label, glyph)) in [
            ("Rename...", icons::PEN),
            ("Archive", icons::ARCHIVE_MINIMALISTIC),
            ("Copy", icons::COPY),
            ("Delete...", icons::TRASH_BIN_MINIMALISTIC),
        ]
        .into_iter()
        .enumerate()
        {
            if index == 3 {
                root = root.child(popover::menu_separator());
            }
            root = root.child(
                popover::menu_row(
                    &theme,
                    menu.highlighted == Some(index),
                    format!("chat-menu-{}-{index}", menu.chat_id),
                )
                .id(("chat-menu-action", index))
                .debug_selector(move || format!("chat-menu-action-{index}"))
                .h(layout.row_height)
                .flex_none()
                .when(index == 3, |row| row.text_color(theme.danger))
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    this.highlight_chat_menu(index, false, cx);
                }))
                .on_click(
                    cx.listener(move |this, _, _, cx| this.activate_chat_menu(index, false, cx)),
                )
                .child(icon(glyph).size(px(16.0)).text_color(if index == 3 {
                    theme.danger
                } else {
                    theme.text_muted
                }))
                .child(div().flex_1().child(label))
                .when(index == COPY_ROW, |row| {
                    row.child(
                        icon(icons::ALT_ARROW_RIGHT)
                            .size(px(14.0))
                            .text_color(theme.text_muted),
                    )
                }),
            );
        }
        let mut overlays = vec![popover::menu_at(
            "chat-context-menu",
            layout.root.origin,
            root.into_any_element(),
            closing,
        )];
        if menu.copy_open {
            let mut copy = popover::popover_card(&theme)
                .w(layout.copy.size.width)
                .flex()
                .flex_col();
            for (index, label) in [
                "Copy working directory",
                "Copy deep link",
                "Copy as Markdown",
            ]
            .into_iter()
            .enumerate()
            {
                copy = copy.child(
                    popover::menu_row(
                        &theme,
                        menu.copy_highlighted == Some(index),
                        format!("chat-copy-{}-{index}", menu.chat_id),
                    )
                    .id(("chat-copy-action", index))
                    .debug_selector(move || format!("chat-copy-action-{index}"))
                    .h(layout.row_height)
                    .flex_none()
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        this.highlight_chat_menu(index, true, cx);
                    }))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.activate_chat_menu(index, true, cx)),
                    )
                    .child(
                        icon(icons::COPY)
                            .size(px(16.0))
                            .text_color(theme.text_muted),
                    )
                    .child(label),
                );
            }
            overlays.push(popover::menu_at(
                "chat-copy-menu",
                layout.copy.origin,
                copy.into_any_element(),
                closing,
            ));
        }
        overlays
    }

    fn copy_chat_directory(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let cwd = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .and_then(|chat| chat.cwd.clone())
            .filter(|cwd| !cwd.trim().is_empty());
        self.sidebar_notice = Some(if let Some(cwd) = cwd {
            cx.write_to_clipboard(ClipboardItem::new_string(cwd));
            "Working directory copied".into()
        } else {
            "This session has no working directory".into()
        });
        self.close_chat_menu(cx);
        cx.notify();
    }

    fn copy_chat_markdown(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        self.close_chat_menu(cx);
        let state = self.state.read(cx);
        let title = state
            .chats
            .iter()
            .find(|chat| chat.id == chat_id)
            .and_then(|chat| chat.title.clone())
            .unwrap_or_else(|| "Session".into());
        let Some(engine) = state.engine().cloned() else {
            self.sidebar_notice = Some("Cannot copy session: engine is unavailable".into());
            cx.notify();
            return;
        };
        let chat_id = chat_id.to_owned();
        self.sidebar_notice = Some("Copying session as Markdown...".into());
        self.chat_copy_task = Some(cx.spawn(async move |this, cx| {
            let snapshot = read_conversation_markdown(engine.client(), &chat_id, &title);
            futures::pin_mut!(snapshot);
            let result = match futures::future::select(
                snapshot,
                cx.background_executor().timer(Duration::from_secs(15)),
            )
            .await
            {
                futures::future::Either::Left((result, _)) => result,
                futures::future::Either::Right(_) => Err("Reading session timed out".into()),
            };
            this.update(cx, |this, cx| {
                this.sidebar_notice = Some(match result {
                    Ok(markdown) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(markdown));
                        "Session copied as Markdown".into()
                    }
                    Err(error) => format!("Could not copy session: {error}").into(),
                });
                this.chat_copy_task = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

async fn read_conversation_markdown(
    client: &holt_rpc::RpcClient,
    chat_id: &str,
    title: &str,
) -> Result<String, String> {
    // The opening frame is the full joined transcript, even for an unselected
    // session. A checked subscription cancels immediately when this snapshot ends.
    let mut stream = client
        .subscribe_checked(
            methods::WATCH_DOC_MESSAGES,
            serde_json::json!({ "chatId": chat_id }),
        )
        .await
        .map_err(|error| error.to_string())?;
    let value = stream.recv().await.ok_or("Session stream closed")?;
    match serde_json::from_value::<TranscriptFrame>(value).map_err(|error| error.to_string())? {
        TranscriptFrame::Reset { reset } => Ok(conversation_markdown(title, &reset)),
        TranscriptFrame::Delta { .. } => Err("Session snapshot is unavailable".into()),
    }
}

fn conversation_markdown(title: &str, entries: &[SessionMessageEntry]) -> String {
    let mut markdown = format!("# {}\n", title.lines().collect::<Vec<_>>().join(" "));
    for entry in entries {
        let body = entry
            .parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::Text { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if body.is_empty() {
            continue;
        }
        let role = match entry.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::System => "System",
        };
        markdown.push_str(&format!("\n## {role}\n\n{body}\n"));
    }
    markdown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn markdown_reads_target_snapshot_and_releases_the_watch() {
        use futures::StreamExt;
        use std::sync::{Arc, Mutex};

        struct Watch {
            cancelled: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        }
        struct CancelSignal(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for CancelSignal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }
        #[async_trait::async_trait]
        impl holt_rpc::RpcService for Watch {
            async fn handle(
                &self,
                method: &str,
                params: serde_json::Value,
            ) -> Result<holt_rpc::RpcReply, holt_rpc::RpcError> {
                assert_eq!(method, methods::WATCH_DOC_MESSAGES);
                assert_eq!(params["chatId"], "unselected-session");
                let signal = CancelSignal(self.cancelled.lock().unwrap().take());
                let stream = futures::stream::once(async move {
                    serde_json::json!({"reset": [{
                        "id": "m", "role": "user", "parts": [{"kind": "text", "id": "p", "text": "Target conversation"}],
                        "createdAt": 0, "deviceId": "test-device"
                    }]})
                }).chain(futures::stream::once(async move {
                    let _signal = signal;
                    std::future::pending::<serde_json::Value>().await
                }));
                Ok(holt_rpc::RpcReply::Stream(stream.boxed()))
            }
        }
        let (cancelled, receiver) = tokio::sync::oneshot::channel();
        let client = holt_rpc::memory_client(Arc::new(Watch {
            cancelled: Mutex::new(Some(cancelled)),
        }));
        let markdown = read_conversation_markdown(&client, "unselected-session", "Target")
            .await
            .unwrap();
        assert_eq!(markdown, "# Target\n\n## User\n\nTarget conversation\n");
        tokio::time::timeout(Duration::from_secs(1), receiver)
            .await
            .expect("watch must be cancelled without another message")
            .unwrap();
    }

    struct MenuHarness {
        shell: Entity<Shell>,
        _observe: Subscription,
    }

    impl Render for MenuHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let menus = self.shell.update(cx, |shell, cx| {
                shell.render_chat_menu(window.viewport_size(), window, cx)
            });
            div().size_full().children(menus)
        }
    }

    #[gpui::test]
    fn hover_submenu_click_and_keyboard_use_the_context_session(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.workspace_scope = Some(holt_proto::WorkspaceScope::Local);
            state.local_device_id = Some("test-device".into());
            state.selected_chat = Some("another-session".into());
            state.chats.push(
                serde_json::from_value(serde_json::json!({
                    "id": "context-session", "deviceId": "test-device", "archived": false,
                    "cwd": "/project with spaces", "createdAt": "2026-09-07T00:00:00Z"
                }))
                .unwrap(),
            );
            state
        });
        let shell = cx.new(|cx| {
            Shell::new(
                state.clone(),
                EngineBootConfig {
                    data_dir: std::env::temp_dir(),
                },
                cx,
            )
        });
        let (_, cx) = cx.add_window_view(|window, cx| {
            let _observe = cx.observe(&shell, |_, _, cx| cx.notify());
            shell.update(cx, |shell, cx| {
                shell.open_chat_menu(
                    "context-session".into(),
                    Point::new(px(100.0), px(100.0)),
                    window,
                    cx,
                )
            });
            MenuHarness {
                shell: shell.clone(),
                _observe,
            }
        });
        cx.run_until_parked();
        let copy_row = cx.debug_bounds("chat-menu-action-2").unwrap();
        assert!(cx.debug_bounds("chat-copy-action-0").is_none());
        cx.simulate_mouse_move(copy_row.center(), None, Default::default());
        cx.run_until_parked();
        let directory = cx
            .debug_bounds("chat-copy-action-0")
            .expect("hover opens submenu");
        cx.simulate_mouse_move(directory.center(), None, Default::default());
        cx.run_until_parked();
        shell.read_with(cx, |shell, _| {
            assert!(shell.chat_menu.closing_since().is_none())
        });
        cx.simulate_click(directory.center(), Default::default());
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                cx.read_from_clipboard().unwrap().text().as_deref(),
                Some("/project with spaces")
            )
        });

        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.open_chat_menu(
                    "context-session".into(),
                    Point::new(px(100.0), px(100.0)),
                    window,
                    cx,
                )
            })
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("down down down right down enter");
        cx.run_until_parked();
        cx.update(|_, cx| {
            let link = cx.read_from_clipboard().unwrap().text().unwrap();
            assert_eq!(
                crate::links::parse_holt_conversation_link(&link)
                    .unwrap()
                    .chat_id,
                "context-session"
            );
        });
        state.read_with(cx, |state, _| {
            assert_eq!(state.selected_chat.as_deref(), Some("another-session"))
        });

        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.open_chat_menu(
                    "context-session".into(),
                    Point::new(px(100.0), px(100.0)),
                    window,
                    cx,
                )
            })
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        shell.read_with(cx, |shell, _| {
            assert!(shell.chat_menu.closing_since().is_some())
        });
    }

    #[test]
    fn submenu_flips_left_and_stays_inside_bottom_edge() {
        let viewport = Size::new(px(1000.0), px(600.0));
        let layout = MenuLayout::new(Point::new(px(990.0), px(590.0)), viewport, px(13.0));
        assert_eq!(layout.root.right(), px(992.0));
        assert_eq!(layout.copy.right(), layout.root.left());
        assert!(layout.copy.bottom() <= px(592.0));
        let layout = MenuLayout::new(Point::new(px(50.0), px(50.0)), viewport, px(13.0));
        assert_eq!(layout.copy.left(), layout.root.right());
        assert_eq!(
            layout.copy.top(),
            layout.root.top() + layout.row_height * 2.0
        );
    }

    #[test]
    fn markdown_preserves_roles_code_and_whitespace_without_tool_traces() {
        let entry = |role, parts| SessionMessageEntry {
            id: "m".into(),
            role,
            parts,
            created_at: 0,
            device_id: "d".into(),
            status: None,
            continuation_of: None,
        };
        let text = |text: &str| MessagePart::Text {
            id: "p".into(),
            text: text.into(),
        };
        let entries = vec![
            entry(MessageRole::User, vec![text("hello  \nworld")]),
            entry(
                MessageRole::Assistant,
                vec![
                    MessagePart::Reasoning {
                        id: "r".into(),
                        text: "thinking".into(),
                    },
                    text("    indented code\n\n```rust\nfn main() {}\n```"),
                    text("tail"),
                ],
            ),
            entry(
                MessageRole::System,
                vec![MessagePart::Notice {
                    id: "n".into(),
                    message: "internal notice".into(),
                }],
            ),
        ];
        assert_eq!(
            conversation_markdown("Title\ncontinued", &entries),
            "# Title continued\n\n## User\n\nhello  \nworld\n\n## Assistant\n\n    indented code\n\n```rust\nfn main() {}\n```\n\ntail\n"
        );
        assert_eq!(conversation_markdown("Empty", &[]), "# Empty\n");
    }
}
