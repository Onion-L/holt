//! Settings → Shortcuts (feature-inventory §1.4): a table of the rebindable
//! bindings — click a combo to record (Esc cancels), live conflict detection,
//! per-row Reset and Restore defaults. Changes emit [`ShortcutsEvent::Changed`];
//! the shell persists them and re-applies the app keymap.

use std::collections::HashMap;

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, KeyDownEvent, SharedString, Window, div,
    prelude::*, px,
};

use crate::settings::{
    KeymapConfig, ShortcutId, combo_from_keystroke, display_combo, display_combo_with_typed_key,
    typed_key_char,
};
use crate::state::AppState;
use crate::theme::Theme;

/// Outcome of one keystroke while recording. Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    /// Esc — abandon recording, keep the old combo.
    Cancelled,
    /// A bare modifier (or unusable key) — stay recording.
    Ignored,
    /// A full combo landed.
    Set(String),
}

pub fn record_key(key: &str, ctrl: bool, alt: bool, shift: bool, cmd: bool) -> RecordOutcome {
    if key.eq_ignore_ascii_case("escape") {
        return RecordOutcome::Cancelled;
    }
    match combo_from_keystroke(ctrl, alt, shift, cmd, key) {
        Some(combo) => RecordOutcome::Set(combo),
        None => RecordOutcome::Ignored,
    }
}

#[derive(Debug, Clone)]
pub enum ShortcutsEvent {
    /// The keymap changed — persist + re-apply.
    Changed(KeymapConfig),
}

pub struct ShortcutsPage {
    /// Working copy (kept in sync with the shell via `Changed` events).
    keymap: KeymapConfig,
    recording: Option<ShortcutId>,
    /// Per-row key-cap override from the most recent recording: the character
    /// the user actually typed for the key (`typed_key_char` — macOS Opt+B
    /// types "∫"). Display-only and session-local; the stored combo stays
    /// layout-neutral so it binds everywhere.
    typed_keys: HashMap<ShortcutId, String>,
    /// A rejected record attempt ("{Combo} is already assigned to {label}.") —
    /// conflicts never persist; they're refused at record time, as in holt.
    conflict_notice: Option<SharedString>,
    focus: FocusHandle,
    // The page never talks RPC; state is kept for parity with sibling pages
    // (and future per-device keymaps).
    _state: Entity<AppState>,
}

impl EventEmitter<ShortcutsEvent> for ShortcutsPage {}

impl ShortcutsPage {
    pub fn new(state: Entity<AppState>, keymap: KeymapConfig, cx: &mut Context<Self>) -> Self {
        Self {
            keymap,
            recording: None,
            typed_keys: HashMap::new(),
            conflict_notice: None,
            focus: cx.focus_handle(),
            _state: state,
        }
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        cx.emit(ShortcutsEvent::Changed(self.keymap.clone()));
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(recording) = self.recording else {
            return;
        };
        let mods = &event.keystroke.modifiers;
        let typed = typed_key_char(&event.keystroke.key, event.keystroke.key_char.as_deref());
        match record_key(
            &event.keystroke.key,
            mods.control,
            mods.alt,
            mods.shift,
            mods.platform,
        ) {
            RecordOutcome::Cancelled => {
                self.recording = None;
                cx.notify();
            }
            RecordOutcome::Ignored => {}
            RecordOutcome::Set(combo) => {
                // A combo already bound elsewhere is REFUSED, naming the owner
                // (holt settings.shortcuts.tsx: "… is already assigned to …").
                if let Some(owner) = conflict_owner(&self.keymap, recording, &combo) {
                    self.conflict_notice = Some(
                        format!(
                            "{} is already assigned to {}.",
                            display_combo_with_typed_key(&combo, typed.as_deref()),
                            owner.label()
                        )
                        .into(),
                    );
                    self.recording = None;
                    cx.notify();
                } else {
                    self.keymap.set(recording, combo);
                    match typed {
                        Some(typed) => {
                            self.typed_keys.insert(recording, typed);
                        }
                        None => {
                            self.typed_keys.remove(&recording);
                        }
                    }
                    self.recording = None;
                    self.conflict_notice = None;
                    self.commit(cx);
                }
            }
        }
        cx.stop_propagation();
    }

    /// One shortcut row: label + description left, Reset when customized, and
    /// the click-to-record combo chip (recording inverts it to
    /// white-on-black). `ix` is the id's position in [`ShortcutId::ALL`]
    /// (unique element ids across the groups).
    fn render_row(
        &self,
        id: ShortcutId,
        ix: usize,
        recording: Option<ShortcutId>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let combo = self.keymap.get(id).to_string();
        let typed_key = self.typed_keys.get(&id).map(String::as_str);
        let is_recording = recording == Some(id);
        let non_default = combo != id.default_combo();
        // holt settings.shortcuts.tsx row: min-h-[72px] px-5 gap-5.
        div()
            .min_h(px(72.0))
            .px(px(0.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(20.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(id.label())),
                    )
                    .child(
                        div()
                            .mt(px(2.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(description(id))),
                    ),
            )
            .when(non_default && !is_recording, |el| {
                el.child(
                    div()
                        .id(("shortcut-reset", ix))
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_muted.opacity(0.7))
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.accent))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.keymap.reset(id);
                            this.typed_keys.remove(&id);
                            this.recording = None;
                            this.commit(cx);
                        }))
                        .child(SharedString::from("Reset")),
                )
            })
            .child(
                div()
                    .id(("shortcut-combo", ix))
                    .min_w(px(88.0))
                    .px(px(8.0))
                    .py(px(4.0))
                    .flex()
                    .justify_center()
                    .cursor_pointer()
                    .map(|el| {
                        if is_recording {
                            el.rounded(px(8.0))
                                .border_1()
                                .border_color(theme.accent)
                                .bg(theme.accent_wash)
                                .text_color(theme.text)
                        } else {
                            el.text_color(theme.text)
                                .hover(|s| s.text_color(theme.accent))
                        }
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.recording = Some(id);
                        this.conflict_notice = None;
                        window.focus(&this.focus, cx);
                        cx.notify();
                    }))
                    .when(is_recording, |el| {
                        el.child(
                            div()
                                .font_family(theme.font_mono.clone())
                                .text_size(crate::typography::ui_rems(12.0))
                                .child(SharedString::from("Press keys…")),
                        )
                    })
                    .when(!is_recording, |el| {
                        el.child(render_keycaps(&combo, typed_key, theme))
                    }),
            )
    }
}

/// Keycaps for a stored combo. The final segment is the key; `typed_key` —
/// what the user actually typed when the combo was recorded (`∫` for a
/// recorded Opt+B, per [`typed_key_char`]) — replaces the canonical label
/// there. Defaults and file-loaded combos pass `None` and render canonically.
fn render_keycaps(combo: &str, typed_key: Option<&str>, theme: &Theme) -> gpui::Div {
    let parts: Vec<&str> = combo.split('-').collect();
    let last = parts.len().saturating_sub(1);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .children(parts.into_iter().enumerate().map(|(ix, part)| {
            let label = if ix == last {
                match typed_key {
                    Some(typed) => typed.to_owned(),
                    None => display_combo(part),
                }
            } else {
                match part {
                    "mod" if cfg!(target_os = "macos") => "⌘".to_owned(),
                    "mod" => "Ctrl".to_owned(),
                    "alt" if cfg!(target_os = "macos") => "⌥".to_owned(),
                    "alt" => "Alt".to_owned(),
                    "shift" => "⇧".to_owned(),
                    other => display_combo(other),
                }
            };
            div()
                .min_w(px(28.0))
                .h(px(28.0))
                .px(px(6.0))
                .rounded(px(6.0))
                .bg(crate::theme::ink(0.06))
                .flex()
                .items_center()
                .justify_center()
                .font_family(theme.font_mono.clone())
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted)
                .child(SharedString::from(label))
        }))
}

/// The shortcut (other than `id`) already bound to `combo`, if any. Pure.
pub fn conflict_owner(keymap: &KeymapConfig, id: ShortcutId, combo: &str) -> Option<ShortcutId> {
    ShortcutId::ALL
        .into_iter()
        .find(|&other| other != id && keymap.get(other) == combo)
}

/// The page's sections, in display order. [`group`] is a total match, so every
/// [`ShortcutId::ALL`] entry lands in exactly one — a shortcut added later
/// extends the match and appears on the page by construction
/// (`every_shortcut_lands_in_a_rendered_group` holds the other half: its group
/// name must be listed here).
const GROUP_ORDER: [&str; 3] = ["Panels", "Sessions", "Jump to session"];

/// The section a shortcut's row renders under.
fn group(id: ShortcutId) -> &'static str {
    match id {
        ShortcutId::ToggleSidebar | ShortcutId::ToggleChanges | ShortcutId::ToggleTerminal => {
            "Panels"
        }
        ShortcutId::NewSession
        | ShortcutId::NextSession
        | ShortcutId::PrevSession
        | ShortcutId::ArchiveSession => "Sessions",
        ShortcutId::JumpSession(_) => "Jump to session",
    }
}

/// One-line purpose copy per shortcut (holt lib/shortcuts.ts
/// `SHORTCUT_DEFINITIONS` descriptions, verbatim).
fn description(id: ShortcutId) -> &'static str {
    match id {
        ShortcutId::ToggleSidebar => "Show or hide sessions and settings navigation.",
        ShortcutId::ToggleChanges => "Show or hide changes for the current session.",
        ShortcutId::ToggleTerminal => "Show or hide the terminal for the current session.",
        ShortcutId::NewSession => "Open a blank session canvas to start a new session.",
        ShortcutId::NextSession => "Select the next session in the sidebar, wrapping at the end.",
        ShortcutId::PrevSession => {
            "Select the previous session in the sidebar, wrapping at the start."
        }
        ShortcutId::ArchiveSession => "Move the current session to the archived shelf.",
        // One line per slot would repeat itself nine times; the ordinal is
        // already in the row's label.
        ShortcutId::JumpSession(_) => "Open the session at this place in the sidebar list.",
    }
}

impl Render for ShortcutsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let recording = self.recording;
        let customized = self.keymap != KeymapConfig::default();

        // Each group stays aligned to the Appearance page's flat row rhythm;
        // `ix` keys the interactive elements so ids remain unique.
        let mut groups: Vec<gpui::AnyElement> = Vec::new();
        for name in GROUP_ORDER {
            let mut rows = div().flex().flex_col();
            let ids = ShortcutId::ALL.into_iter().filter(|&id| group(id) == name);
            for id in ids {
                let ix = ShortcutId::ALL.iter().position(|&a| a == id).unwrap_or(0);
                rows = rows.child(self.render_row(id, ix, recording, &theme, cx));
            }
            groups.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(widgets::field_label(&theme, name))
                    .child(rows)
                    .into_any_element(),
            );
        }

        // Helper line stays in the muted tone even for a rejected conflict —
        // the message names the specific clash (holt settings.shortcuts.tsx).
        let helper: SharedString = if recording.is_some() {
            "Press Escape to cancel.".into()
        } else if let Some(notice) = self.conflict_notice.clone() {
            notice
        } else {
            "Shortcuts must be unique.".into()
        };

        div()
            .id("shortcuts-page")
            .size_full()
            .overflow_y_scroll()
            .track_focus(&self.focus)
            .on_key_down(
                cx.listener(|this, event: &KeyDownEvent, _, cx| this.on_key_down(event, cx)),
            )
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_start()
                            .justify_between()
                            .gap(px(24.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(widgets::page_header(&theme, "Keyboard shortcuts", None))
                                    .child(
                                        widgets::page_subtitle(
                                            &theme,
                                            "Click a binding, then press the key combination you \
                                             want to use. Changes apply immediately and stay on \
                                             this device.",
                                        )
                                        .max_w(px(512.0))
                                        .line_height(px(20.0)),
                                    ),
                            )
                            .child({
                                // `disabled:opacity-35` when nothing is
                                // customized or while recording.
                                let disabled = !customized || recording.is_some();
                                widgets::ghost_action(&theme)
                                    .id("shortcuts-restore-defaults")
                                    .flex_none()
                                    .when(disabled, |el| el.opacity(0.35))
                                    .when(!disabled, |el| {
                                        el.hover(|s| {
                                            s.bg(crate::theme::ink(0.04)).text_color(theme.text)
                                        })
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.keymap = KeymapConfig::default();
                                                this.typed_keys.clear();
                                                this.recording = None;
                                                this.conflict_notice = None;
                                                this.commit(cx);
                                            }),
                                        )
                                    })
                                    .child(
                                        crate::icons::icon(crate::icons::RESTART)
                                            .size(px(14.0))
                                            .text_color(theme.text_muted),
                                    )
                                    .child(SharedString::from("Restore defaults"))
                            }),
                    )
                    .child(
                        div()
                            .mt(px(32.0))
                            .flex()
                            .flex_col()
                            .gap(px(28.0))
                            .children(groups),
                    )
                    .child(
                        div()
                            .mt(px(12.0))
                            .px(px(4.0))
                            .min_h(px(20.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(if recording.is_some() {
                                theme.accent
                            } else if self.conflict_notice.is_some() {
                                theme.warning
                            } else {
                                theme.text_muted
                            })
                            .child(helper),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_outcomes() {
        assert_eq!(
            record_key("escape", false, false, false, false),
            RecordOutcome::Cancelled
        );
        assert_eq!(
            record_key("Escape", true, false, false, false),
            RecordOutcome::Cancelled
        );
        assert_eq!(
            record_key("s", false, false, false, true),
            RecordOutcome::Set("mod-s".into())
        );
        assert_eq!(
            record_key("k", false, true, true, true),
            RecordOutcome::Set("mod-alt-shift-k".into())
        );
        // macOS-only: elsewhere ctrl IS the primary and records as "mod".
        #[cfg(target_os = "macos")]
        assert_eq!(
            record_key("tab", true, false, true, false),
            RecordOutcome::Set("ctrl-shift-tab".into())
        );
        // Bare modifiers stay recording.
        assert_eq!(
            record_key("shift", false, false, true, false),
            RecordOutcome::Ignored
        );
        assert_eq!(
            record_key("ctrl", true, false, false, false),
            RecordOutcome::Ignored
        );
    }

    #[test]
    fn every_shortcut_lands_in_a_rendered_group() {
        // The page renders GROUP_ORDER's cards and nothing else — a group()
        // arm returning a name missing from GROUP_ORDER would silently drop
        // its rows from Settings.
        for id in ShortcutId::ALL {
            assert!(
                GROUP_ORDER.contains(&group(id)),
                "{:?} is grouped under {:?}, which GROUP_ORDER does not render",
                id,
                group(id)
            );
        }
        // And every named group has at least one row — no empty cards.
        for name in GROUP_ORDER {
            assert!(
                ShortcutId::ALL.into_iter().any(|id| group(id) == name),
                "group {:?} would render an empty card",
                name
            );
        }
    }

    #[test]
    fn conflicting_records_are_refused() {
        // holt parity: a combo bound elsewhere is refused at record time (the
        // helper names the owner) — conflicts never persist into the keymap.
        let keymap = KeymapConfig::default();
        let RecordOutcome::Set(combo) = record_key("b", false, false, false, true) else {
            panic!("expected Set");
        };
        // "mod-b" is now the LEFT sidebar's default, so re-recording it on the
        // left sidebar is free, while the right sidebar (⌘⌥B) hits it.
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleSidebar, &combo),
            None
        );
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleChanges, &combo),
            Some(ShortcutId::ToggleSidebar)
        );
        // A free combo conflicts with nothing.
        assert_eq!(
            conflict_owner(&keymap, ShortcutId::ToggleSidebar, "mod-shift-x"),
            None
        );
    }

    #[test]
    fn recorded_composed_keys_display_as_typed() {
        // Opt+B on macOS composes "∫": the stored combo keeps the layout key,
        // and the display overrides only its key segment.
        assert_eq!(typed_key_char("b", Some("∫")).as_deref(), Some("∫"));
        assert_eq!(
            display_combo_with_typed_key("mod-alt-b", Some("∫")),
            "Cmd+Opt+∫"
        );
        // A keystroke whose character is the key itself records canonically.
        assert_eq!(typed_key_char("b", Some("b")), None);
        assert_eq!(
            display_combo_with_typed_key("mod-alt-b", None),
            display_combo("mod-alt-b")
        );
    }
}
