//! The custom-provider logo dialog: pick an image file or paste SVG markup,
//! preview it, save or remove. Opened from the expanded panel's mark tile.

use super::*;
use gpui::{App, Focusable as _};

use crate::composer::ComposerInputEvent;
use crate::provider_logos::{self, BrandMark};

/// The engine's raw upload ceiling; checked here first so an oversized pick
/// never crosses the transport.
pub(super) const MAX_LOGO_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LogoSource {
    File,
    Svg,
}

pub(super) struct PickedLogo {
    name: SharedString,
    bytes: Vec<u8>,
    preview: Option<BrandMark>,
}

pub(super) struct LogoDialog {
    pub(super) provider: String,
    name: SharedString,
    abbreviation: String,
    has_logo: bool,
    pub(super) source: LogoSource,
    file: Option<PickedLogo>,
    pub(super) svg_input: Entity<ComposerInput>,
    saving: bool,
    pub(super) error: Option<String>,
    _svg_events: gpui::Subscription,
}

impl LogoDialog {
    /// The bytes Save would upload, if any.
    fn pending(&self, cx: &App) -> Option<Vec<u8>> {
        match self.source {
            LogoSource::File => self.file.as_ref().map(|file| file.bytes.clone()),
            LogoSource::Svg => {
                let text = self.svg_input.read(cx).text().trim();
                provider_logos::looks_like_svg(text).then(|| text.as_bytes().to_vec())
            }
        }
    }

    fn preview(&self, cx: &App) -> Option<BrandMark> {
        match self.source {
            LogoSource::File => self.file.as_ref().and_then(|file| file.preview.clone()),
            LogoSource::Svg => provider_logos::preview_svg(self.svg_input.read(cx).text().trim()),
        }
    }

    fn release(self) {
        if let Some(PickedLogo {
            preview: Some(BrandMark::Image(image)),
            ..
        }) = self.file
        {
            crate::images::discard(image);
        }
        provider_logos::clear_previews();
    }
}

/// Send `bytes` to the engine, which normalizes and stores them. `Err` is a
/// user-facing reason.
pub(super) async fn upload_logo_bytes(
    client: &holt_rpc::RpcClient,
    provider_id: &str,
    bytes: &[u8],
) -> Result<(), String> {
    client
        .call(
            methods::SET_PROVIDER_LOGO,
            serde_json::json!({
                "providerId": provider_id,
                "data": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes),
            }),
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub(super) fn read_logo_file(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let size = std::fs::metadata(path)
        .map_err(|error| error.to_string())?
        .len();
    if size > MAX_LOGO_BYTES {
        return Err("Logo exceeds the 8 MB limit".into());
    }
    std::fs::read(path).map_err(|error| error.to_string())
}

impl ProvidersPage {
    pub(super) fn open_logo_dialog(&mut self, provider: &Provider, cx: &mut Context<Self>) {
        let svg_input = cx.new(|cx| {
            let mut input = ComposerInput::new("<svg xmlns=\"http://www.w3.org/2000/svg\" …>", cx);
            input.set_max_display_height(104.0);
            input
        });
        let _svg_events = cx.subscribe(
            &svg_input,
            |page: &mut Self, _, event: &ComposerInputEvent, cx: &mut Context<Self>| match event {
                ComposerInputEvent::Edited => {
                    if let Some(dialog) = page.logo_dialog.as_mut() {
                        dialog.error = None;
                    }
                    cx.notify();
                }
                ComposerInputEvent::Submitted => page.save_logo_dialog(cx),
                _ => {}
            },
        );
        self.close_logo_dialog(cx);
        self.logo_dialog = Some(LogoDialog {
            provider: provider.id.to_string(),
            name: provider.name.clone().into(),
            abbreviation: provider.abbreviation.clone(),
            has_logo: provider.logo.is_some(),
            source: LogoSource::File,
            file: None,
            svg_input,
            saving: false,
            error: None,
            _svg_events,
        });
        cx.notify();
    }

    pub(super) fn close_logo_dialog(&mut self, cx: &mut Context<Self>) {
        if let Some(dialog) = self.logo_dialog.take() {
            dialog.release();
            cx.notify();
        }
    }

    fn set_logo_source(&mut self, source: LogoSource, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.logo_dialog.as_mut() else {
            return;
        };
        dialog.source = source;
        dialog.error = None;
        if source == LogoSource::Svg {
            let focus = dialog.svg_input.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    fn pick_logo_file(&mut self, cx: &mut Context<Self>) {
        // `runModal` must run outside every gpui borrow (see the backdrop
        // picker in Settings → Appearance).
        cx.spawn(async move |page, cx| {
            let Some(path) = crate::file_dialog::pick_logo() else {
                return;
            };
            page.update(cx, |page, cx| page.load_logo_file(&path, cx))
                .ok();
        })
        .detach();
    }

    pub(super) fn load_logo_file(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        let Some(dialog) = self.logo_dialog.as_mut() else {
            return;
        };
        let bytes = match read_logo_file(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                dialog.error = Some(error);
                cx.notify();
                return;
            }
        };
        let preview = match std::str::from_utf8(&bytes) {
            Ok(text) if provider_logos::looks_like_svg(text) => provider_logos::preview_svg(text),
            _ => crate::images::decode_to_render(&bytes, Some(256))
                .ok()
                .map(|(image, _, _)| BrandMark::Image(image)),
        };
        if preview.is_none() {
            dialog.error = Some("Not a readable image".into());
            cx.notify();
            return;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(PickedLogo {
            preview: Some(BrandMark::Image(image)),
            ..
        }) = dialog.file.replace(PickedLogo {
            name: name.into(),
            bytes,
            preview,
        }) {
            crate::images::discard(image);
        }
        dialog.error = None;
        cx.notify();
    }

    pub(super) fn save_logo_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(dialog) = self.logo_dialog.as_mut() else {
            return;
        };
        if dialog.saving {
            return;
        }
        let Some(bytes) = dialog.pending(cx) else {
            return;
        };
        dialog.saving = true;
        dialog.error = None;
        let provider = dialog.provider.clone();
        cx.notify();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = upload_logo_bytes(engine.client(), &provider, &bytes).await;
            this.update(cx, |page, cx| page.logo_changed(result, cx))
                .ok();
        }));
    }

    /// Drop a custom provider's logo; its rows fall back to the monogram.
    fn remove_provider_logo(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(dialog) = self.logo_dialog.as_mut() else {
            return;
        };
        if dialog.saving {
            return;
        }
        dialog.saving = true;
        let provider = dialog.provider.clone();
        cx.notify();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::REMOVE_PROVIDER_LOGO,
                    serde_json::json!({ "providerId": provider }),
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string());
            this.update(cx, |page, cx| page.logo_changed(result, cx))
                .ok();
        }));
    }

    fn logo_changed(&mut self, result: Result<(), String>, cx: &mut Context<Self>) {
        match result {
            Ok(()) => {
                self.close_logo_dialog(cx);
                crate::pickers::bump_provider_catalog(cx);
                self.load(cx);
            }
            Err(error) => {
                if let Some(dialog) = self.logo_dialog.as_mut() {
                    dialog.saving = false;
                    dialog.error = Some(error);
                    cx.notify();
                } else {
                    self.fail(format!("Logo not saved: {error}"), cx);
                }
            }
        }
    }
}

const PREVIEW_TILE: f32 = 64.0;
const SOURCE_AREA_HEIGHT: f32 = 132.0;

pub(super) fn logo_dialog(
    dialog: &LogoDialog,
    theme: &Theme,
    cx: &mut Context<ProvidersPage>,
) -> AnyElement {
    let preview = dialog.preview(cx);
    let has_preview = preview.is_some();
    let can_save = !dialog.saving && dialog.pending(cx).is_some();
    let status: SharedString = if has_preview {
        "Preview — not saved yet".into()
    } else if dialog.has_logo {
        "Current logo".into()
    } else {
        "No logo yet — showing the monogram".into()
    };
    let mark = preview
        .or_else(|| provider_logos::custom_mark(&dialog.provider))
        .map(|mark| mark.render(px(36.0), theme.text))
        .unwrap_or_else(|| {
            provider_logos::monogram(&dialog.abbreviation, px(36.0), theme).into_any_element()
        });

    let header = div()
        .flex()
        .items_center()
        .gap(px(14.0))
        .child(
            div()
                .size(px(PREVIEW_TILE))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(16.0))
                .bg(crate::theme::ink(0.05))
                .child(mark),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(popover::dialog_title(theme, "Provider logo"))
                .child(
                    div()
                        .truncate()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(format!("{} · {status}", dialog.name))),
                ),
        )
        .child(
            widgets::ghost_action(theme)
                .flex_none()
                .self_start()
                .px(px(6.0))
                .id("logo-dialog-close")
                .hover(move |style| widgets::ghost_hover(theme, style))
                .on_click(cx.listener(|page, _, _, cx| page.close_logo_dialog(cx)))
                .child(
                    crate::icons::icon(crate::icons::CLOSE)
                        .size(px(13.0))
                        .text_color(theme.text_muted),
                ),
        );

    let tab = |source: LogoSource, id: &'static str, label: &'static str| {
        let active = dialog.source == source;
        let hover_text = theme.text;
        let mut chip = div()
            .id(id)
            .debug_selector(move || id.into())
            .flex_1()
            .h(px(26.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .text_size(crate::typography::ui_rems(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(if active { theme.text } else { theme.text_muted });
        if active {
            chip = chip.bg(theme.ink(0.12));
        } else {
            chip =
                chip.cursor_pointer()
                    .hover(move |style| style.text_color(hover_text))
                    .on_click(cx.listener(move |page, _, window, cx| {
                        page.set_logo_source(source, window, cx)
                    }));
        }
        chip.child(label)
    };
    let tabs = div()
        .h(px(32.0))
        .p(px(3.0))
        .flex()
        .gap(px(2.0))
        .rounded(px(9.0))
        .bg(theme.ink(0.04))
        .child(tab(LogoSource::File, "logo-dialog-tab-file", "Upload file"))
        .child(tab(LogoSource::Svg, "logo-dialog-tab-svg", "Paste SVG"));

    let source_area = match dialog.source {
        LogoSource::File => {
            let (title, hint): (SharedString, SharedString) = match &dialog.file {
                Some(file) => (file.name.clone(), "Click to choose another".into()),
                None => (
                    "Choose an image".into(),
                    "PNG, JPG, WebP or SVG · up to 8 MB".into(),
                ),
            };
            let hover_bg = theme.ink(0.05);
            div()
                .id("logo-dialog-choose")
                .debug_selector(|| "logo-dialog-choose".into())
                .h(px(SOURCE_AREA_HEIGHT))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(6.0))
                .px(px(16.0))
                .rounded(px(12.0))
                .border_1()
                .border_dashed()
                .border_color(theme.border)
                .bg(theme.ink(0.02))
                .cursor_pointer()
                .hover(move |style| style.bg(hover_bg))
                .on_click(cx.listener(|page, _, _, cx| page.pick_logo_file(cx)))
                .child(
                    crate::icons::icon(crate::icons::ARCHIVE_UP_MINIMALISTIC)
                        .size(px(20.0))
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .max_w_full()
                        .truncate()
                        .text_size(crate::typography::ui_rems(13.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(11.5))
                        .text_color(theme.text_muted)
                        .child(hint),
                )
                .into_any_element()
        }
        LogoSource::Svg => div()
            .h(px(SOURCE_AREA_HEIGHT))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px(px(12.0))
                    .py(px(10.0))
                    .rounded(px(12.0))
                    .bg(theme.input_glass_bg())
                    .overflow_hidden()
                    .child(dialog.svg_input.clone()),
            )
            .child(
                div()
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.text_muted)
                    .child("Drawn as a single-colour mask tinted to the theme. Enter saves."),
            )
            .into_any_element(),
    };
    let svg_text = dialog.svg_input.read(cx).text().trim();
    let error = dialog.error.clone().or_else(|| {
        (dialog.source == LogoSource::Svg
            && !svg_text.is_empty()
            && !provider_logos::looks_like_svg(svg_text))
        .then(|| "That doesn\u{2019}t look like SVG markup".to_string())
    });

    let danger = theme.danger;
    let danger_muted = theme.danger_muted;
    let save_label = if dialog.saving { "Saving…" } else { "Save" };
    let save = popover::btn_primary(theme, save_label)
        .id("logo-dialog-save")
        .debug_selector(|| "logo-dialog-save".into());
    let save = if can_save {
        save.on_click(cx.listener(|page, _, _, cx| page.save_logo_dialog(cx)))
    } else {
        save.opacity(0.4).cursor_default()
    };
    let footer = div()
        .mt(px(2.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .children(dialog.has_logo.then(|| {
            widgets::ghost_action(theme)
                .ml(px(-10.0))
                .id("logo-dialog-remove")
                .debug_selector(|| "logo-dialog-remove".into())
                .hover(move |style| style.bg(danger.opacity(0.10)).text_color(danger_muted))
                .on_click(cx.listener(|page, _, _, cx| page.remove_provider_logo(cx)))
                .child(
                    crate::icons::icon(crate::icons::TRASH_BIN_MINIMALISTIC)
                        .size(px(13.0))
                        .text_color(theme.text_muted),
                )
                .child("Remove logo")
        }))
        .child(div().flex_1())
        .child(
            popover::btn_ghost(theme, "Cancel", "logo-dialog-cancel")
                .id("logo-dialog-cancel")
                .on_click(cx.listener(|page, _, _, cx| page.close_logo_dialog(cx))),
        )
        .child(save);

    popover::dialog_card(theme)
        .w(px(420.0))
        .gap(px(16.0))
        .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_logo_dialog(cx)))
        .child(header)
        .child(tabs)
        .child(source_area)
        .children(error.map(|message| {
            div()
                .mt(px(-6.0))
                .text_size(crate::typography::ui_rems(11.5))
                .text_color(theme.danger_muted.opacity(0.9))
                .child(SharedString::from(message))
        }))
        .child(footer)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::providers::test_support::*;

    fn custom_provider(logo: bool) -> Provider {
        Provider {
            id: "acme".into(),
            name: "Acme".into(),
            abbreviation: "AC".into(),
            configured: true,
            variants: Vec::new(),
            custom: true,
            logo: logo.then(|| holt_proto::ProviderLogo {
                format: holt_proto::ProviderLogoFormat::Svg,
                data: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "<svg/>"),
            }),
        }
    }

    /// Pasted SVG markup previews, then Save uploads the trimmed text and
    /// closes the dialog.
    #[gpui::test]
    fn pasted_svg_saves_through_set_provider_logo(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        harness.page.update(&mut *harness.visual, |page, cx| {
            page.open_logo_dialog(&custom_provider(false), cx)
        });
        harness.pump();
        harness.click("logo-dialog-tab-svg");
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\"/>";
        harness.page.update(&mut *harness.visual, |page, cx| {
            let input = page.logo_dialog.as_ref().unwrap().svg_input.clone();
            input.update(cx, |input, cx| input.set_text(format!("  {svg}\n"), cx));
        });
        harness.pump();
        harness.click("logo-dialog-save");
        let calls = harness.engine.logo_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].0, methods::SET_PROVIDER_LOGO);
        assert_eq!(calls[0].1["providerId"], "acme");
        let data = calls[0].1["data"].as_str().unwrap();
        assert_eq!(
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data).unwrap(),
            svg.as_bytes()
        );
        let open = harness
            .page
            .update(&mut *harness.visual, |page, _| page.logo_dialog.is_some());
        assert!(!open, "the dialog closes after a save");
    }

    /// A picked file previews before anything uploads; non-images are
    /// refused in place.
    #[gpui::test]
    fn picked_files_preview_and_junk_is_refused(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        let good = harness._dir.path().join("mark.svg");
        std::fs::write(&good, "<svg xmlns=\"http://www.w3.org/2000/svg\"/>").unwrap();
        let junk = harness._dir.path().join("junk.png");
        std::fs::write(&junk, "not an image").unwrap();
        harness.page.update(&mut *harness.visual, |page, cx| {
            page.open_logo_dialog(&custom_provider(true), cx);
            page.load_logo_file(&junk, cx);
            assert!(page.logo_dialog.as_ref().unwrap().error.is_some());
            page.load_logo_file(&good, cx);
            assert!(page.logo_dialog.as_ref().unwrap().error.is_none());
        });
        harness.pump();
        assert!(harness.engine.logo_calls.lock().unwrap().is_empty());
        harness.click("logo-dialog-save");
        let calls = harness.engine.logo_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "{calls:?}");
    }

    #[gpui::test]
    fn remove_logo_lives_in_the_dialog(cx: &mut gpui::TestAppContext) {
        let mut harness = providers_harness(cx);
        harness.page.update(&mut *harness.visual, |page, cx| {
            page.open_logo_dialog(&custom_provider(true), cx)
        });
        harness.pump();
        harness.click("logo-dialog-remove");
        let calls = harness.engine.logo_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].0, methods::REMOVE_PROVIDER_LOGO);
        let open = harness
            .page
            .update(&mut *harness.visual, |page, _| page.logo_dialog.is_some());
        assert!(!open);
    }
}
