//! Staged references and diff comments: the per-chat stash plus the strip UI
//! rendered inside the pill.
//!
//! Everything the user attaches becomes a path reference: picker/drop files
//! bind to their live targets, and pasted screenshots are saved by the engine
//! as Managed images (durable before any send can reference them) and join
//! the same chips. Supported images render as thumbnails; everything else
//! keeps its badge-style chip.

use super::Composer;
use super::layout::{REF_CHIP_LABEL_MAX, STRIP_GAP, STRIP_PAD_TOP, STRIP_PAD_X, STRIP_THUMB};

use std::path::PathBuf;

use gpui::{
    App, Context, ObjectFit, PathPromptOptions, SharedString, StyledImage as _, div, img,
    prelude::*, px,
};

use crate::image_viewer::ViewerTarget;
use crate::images as image_store;
use crate::path_refs::{self, PathRef};
use crate::theme::Theme;

impl Composer {
    pub(super) fn draft_image_targets(&self, cx: &gpui::App) -> Vec<ViewerTarget> {
        let mut targets: Vec<_> = self
            .staged_refs()
            .iter()
            .filter(|r| !r.is_dir && image_store::is_image_path(&r.full_path()))
            .map(|r| ViewerTarget {
                path: r.full_path().into(),
                label: r.name().into(),
            })
            .collect();
        for mention in super::mentions::file_mention_links(self.input.read(cx).text()) {
            if !mention.is_dir
                && image_store::is_image_path(&mention.path)
                && !targets.iter().any(|t| t.path.as_ref() == mention.path)
            {
                targets.push(ViewerTarget {
                    path: mention.path.clone().into(),
                    label: std::path::Path::new(&mention.path)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                        .into(),
                });
            }
        }
        targets
    }
    /// Staged path references for the chat the composer is showing.
    pub(super) fn staged_refs(&self) -> &[PathRef] {
        self.path_refs
            .get(&self.current_key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Stage path references (picker / drop / pasted paths): every file or
    /// folder — images included — binds to its absolute target as a chip.
    /// Nothing is uploaded or read; a path that fails to bind surfaces in the
    /// failure notice while the rest of the selection still attaches.
    pub(crate) fn add_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.add_paths_to_draft(paths, self.current_key.clone(), cx);
    }

    fn add_paths_to_draft(
        &mut self,
        paths: Vec<PathBuf>,
        draft_key: String,
        cx: &mut Context<Self>,
    ) {
        for path in &paths {
            match path_refs::bind(path) {
                Ok(reference) => {
                    let list = self.path_refs.entry(draft_key.clone()).or_default();
                    path_refs::push_unique(list, reference);
                }
                Err(message) => {
                    self.failure = Some(message.into());
                    self.failure_key = Some(draft_key.clone());
                }
            }
        }
        cx.notify();
    }

    /// Paste intake: each clipboard image is saved by the engine as a Managed
    /// image — durably, before any message can reference it — and joins the
    /// draft as a path reference. A failed save reports its cause and leaves
    /// the draft (text and earlier images) untouched; nothing is staged
    /// through the retired upload pipeline.
    pub(super) fn stage_pasted_images(&mut self, images: Vec<gpui::Image>, cx: &mut Context<Self>) {
        if images.is_empty() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.failure = Some("Engine not connected — couldn't save the pasted image.".into());
            self.failure_key = None;
            cx.notify();
            return;
        };
        let draft_key = self.current_key.clone();
        *self.pasting.entry(draft_key.clone()).or_default() += images.len();
        cx.notify();
        cx.spawn(async move |this, cx| {
            for image in images {
                let staged = crate::images::stage_pasted(&engine, &image).await;
                this.update(cx, |composer, cx| {
                    if let Some(count) = composer.pasting.get_mut(&draft_key) {
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            composer.pasting.remove(&draft_key);
                        }
                    }
                    match staged {
                        Ok(path) => {
                            let reference = PathRef {
                                id: uuid::Uuid::new_v4().to_string(),
                                path: PathBuf::from(path.as_str()),
                                is_dir: false,
                                managed: true,
                            };
                            composer
                                .path_refs
                                .entry(draft_key.clone())
                                .or_default()
                                .push(reference);
                            cx.notify();
                        }
                        Err(message) => {
                            composer.failure = Some(message);
                            composer.failure_key = Some(draft_key.clone());
                            cx.notify();
                        }
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// A staged chip was removed. Managed images are released best-effort
    /// through the engine, which keeps the file if any durable store still
    /// references it — removing one chip never invalidates another use.
    pub(super) fn remove_path_ref(&mut self, id: &str, cx: &mut Context<Self>) {
        let mut released: Option<PathBuf> = None;
        if let Some(list) = self.path_refs.get_mut(&self.current_key) {
            if let Some(at) = list.iter().position(|r| r.id == id) {
                let removed = list.remove(at);
                if removed.managed {
                    released = Some(removed.path);
                }
            }
            if list.is_empty() {
                self.path_refs.remove(&self.current_key);
            }
        }
        cx.notify();
        if let Some(path) = released
            && !self.sending
            && !self.path_refs.values().flatten().any(|r| r.path == path)
            && !self.input.read(cx).text().contains(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .as_ref(),
            )
            && !self.drafts.values().any(|text| {
                text.contains(
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .as_ref(),
                )
            })
            && !self.failed_submissions.values().any(|(_, payload)| {
                payload
                    .to_string()
                    .contains(path.to_string_lossy().as_ref())
            })
            && let Some(engine) = self.state.read(cx).engine().cloned()
        {
            cx.spawn(async move |_, _cx| {
                let result = engine
                    .client()
                    .call(
                        holt_rpc::methods::RELEASE_IMAGE,
                        serde_json::json!({ "path": path.to_string_lossy() }),
                    )
                    .await;
                if let Err(error) = result {
                    tracing::debug!(%error, "managed image release failed; boot cleanup will retry");
                }
            })
            .detach();
        }
    }

    /// Drop a deleted chat's per-chat composer state — a deleted chat's stage
    /// could never be sent again (its managed files are reclaimed by the
    /// engine's next startup cleanup, which reconciles durable references).
    pub fn purge_chat(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        self.path_refs.remove(chat_id);
        self.state.update(cx, |state, _| {
            state.purge_diff_comments(chat_id);
        });
    }

    /// Staged in `AppState` because the changes pane writes them.
    pub(super) fn staged_comments(&self, cx: &App) -> Vec<crate::comments::DiffComment> {
        self.state
            .read(cx)
            .diff_comments(&self.current_key)
            .to_vec()
    }

    pub(super) fn render_comments_chip(&self, theme: &Theme, cx: &App) -> Option<gpui::Div> {
        let count = self.staged_comments(cx).len();
        if count == 0 {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_row()
                .px(px(STRIP_PAD_X))
                .pt(px(STRIP_PAD_TOP))
                .child(crate::badges::render(
                    "composer-comments",
                    &crate::badges::MessageBadge {
                        icon: crate::icons::CHAT_ROUND_LINE,
                        label: crate::comments::chip_label(count).into(),
                        // The staged set is already on screen in the changes
                        // pane, so a hover card would only repeat it.
                        details: Vec::new(),
                    },
                    theme,
                )),
        )
    }

    /// The staged strip: supported images render as 56px thumbnails, other
    /// files and folders as badge chips. Clicking a thumbnail opens the
    /// zoomable viewer scoped to the draft's images.
    pub(super) fn render_path_ref_strip(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        let refs = self.staged_refs();
        if refs.is_empty() {
            return None;
        }
        let image_paths = self.draft_image_targets(cx);
        let mut image_strip = div().flex().flex_wrap().gap(px(STRIP_GAP));
        let mut file_strip = div().flex().flex_wrap().gap(px(STRIP_GAP));
        let mut has_images = false;
        let mut has_files = false;
        for (ix, reference) in refs.iter().enumerate() {
            let full_path = reference.full_path();
            let remove_id = reference.id.clone();
            let is_image =
                !reference.is_dir && image_store::is_image_path(&reference.path.to_string_lossy());
            let frame = if is_image {
                let image_index = image_paths
                    .iter()
                    .position(|target| {
                        target.path.as_ref() == reference.path.to_string_lossy().as_ref()
                    })
                    .unwrap_or(0);
                let targets = image_paths.clone();
                let full_path = full_path.clone();
                let thumb = self.thumb_snapshot(&reference.path.to_string_lossy(), cx);
                let thumb_frame = div()
                    .id(("composer-ref-thumb", ix))
                    .size(px(STRIP_THUMB))
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .border_1()
                    .border_color(crate::theme::hairline(0.10))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_viewer(targets.clone(), image_index, window, cx);
                    }))
                    .tooltip(move |_, cx| {
                        cx.new(|_| super::queue::ActionTooltip(full_path.clone().into()))
                            .into()
                    });
                let thumb_frame = match thumb {
                    image_store::Snapshot::Loaded(thumb) => thumb_frame.child(
                        img(thumb.pixels.clone())
                            // EXPLICIT dims: img layout honors the image's
                            // intrinsic aspect ratio over a percent height
                            // (gpui f8d8a90 repoint), so the frame's aspect
                            // must be fixed by explicit w/h (56−2 = frame
                            // minus its 1px borders; own 7px radii because
                            // the frame's rounding clips rectangularly).
                            .w(px(STRIP_THUMB - 2.0))
                            .h(px(STRIP_THUMB - 2.0))
                            .rounded(px(7.0))
                            .object_fit(ObjectFit::Cover),
                    ),
                    image_store::Snapshot::Loading => {
                        thumb_frame.bg(crate::theme::ink(0.055)).opacity(
                            0.35 + 0.4
                                * crate::motion::pulse_wave(crate::motion::pulse_delta(
                                    &crate::motion::HOLT_PULSE,
                                    cx.entity_id(),
                                    cx,
                                )),
                        )
                    }
                    image_store::Snapshot::Error { cause, .. } => thumb_frame
                        .tooltip(move |_, cx| {
                            cx.new(|_| super::queue::ActionTooltip(cause.clone()))
                                .into()
                        })
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(crate::icons::icon(crate::icons::DANGER_TRIANGLE).size(px(18.0)))
                        .border_dashed()
                        .border_color(crate::theme::hairline(0.14))
                        .bg(crate::theme::ink(0.025)),
                };
                Some(thumb_frame)
            } else {
                None
            };
            let chip = div()
                .id(("composer-ref", ix))
                .h(px(crate::badges::BADGE_HEIGHT))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .px(px(8.0))
                .rounded(px(8.0))
                .bg(crate::theme::ink(0.06))
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text_muted)
                .tooltip(move |_, cx| {
                    cx.new(|_| super::queue::ActionTooltip(full_path.clone().into()))
                        .into()
                });
            let chip = if let Some(thumb_frame) = frame {
                // Image references render as a thumbnail tile with the
                // remove affordance beside it, matching the attachment-strip
                // idiom (an overhanging hover button on its own frost layer).
                let group: SharedString = format!("composer-ref-{}", reference.id).into();
                div()
                    .group(group.clone())
                    .relative()
                    .child(thumb_frame)
                    .child(crate::frost::layered(
                        div()
                            .id(("composer-ref-remove", ix))
                            .absolute()
                            .top(px(-6.0))
                            .right(px(-6.0))
                            .size(px(18.0))
                            .rounded_full()
                            .bg(theme.bg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .shadow_sm()
                            .opacity(0.0)
                            .group_hover(group, |s| s.opacity(1.0))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                // The button overhangs the thumbnail, whose
                                // hitbox is right underneath — don't let the
                                // same click also open the preview.
                                cx.stop_propagation();
                                this.remove_path_ref(&remove_id, cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::CLOSE_CIRCLE)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            ),
                    ))
                    .into_any_element()
            } else {
                chip.child(
                    crate::icons::icon(if reference.is_dir {
                        crate::icons::FOLDER
                    } else {
                        crate::icons::DOCUMENT
                    })
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.7)),
                )
                .child(
                    div()
                        .max_w(px(REF_CHIP_LABEL_MAX))
                        .overflow_hidden()
                        .truncate()
                        .child(reference.name()),
                )
                .child(
                    div()
                        .id(("composer-ref-remove", ix))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.remove_path_ref(&remove_id, cx);
                        }))
                        .child(
                            crate::icons::icon(crate::icons::CLOSE_CIRCLE)
                                .size(px(12.0))
                                .text_color(theme.text_muted.opacity(0.7)),
                        ),
                )
                .into_any_element()
            };
            if is_image {
                has_images = true;
                image_strip = image_strip.child(chip);
            } else {
                has_files = true;
                file_strip = file_strip.child(chip);
            }
        }
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(STRIP_GAP))
                .px(px(STRIP_PAD_X))
                .pt(px(STRIP_PAD_TOP))
                .children(has_images.then_some(image_strip))
                .children(has_files.then_some(file_strip)),
        )
    }

    /// The cached thumbnail for a path, claiming a load when none is in
    /// flight. Results land in the shared image cache and wake the composer
    /// through the tracked task.
    pub(super) fn thumb_snapshot(
        &self,
        path: &str,
        cx: &mut Context<Self>,
    ) -> image_store::Snapshot {
        if image_store::begin_load(path) {
            let Some(engine) = self.state.read(cx).engine().cloned() else {
                image_store::store_error(path, "Engine not connected.");
                return image_store::snapshot(path);
            };
            let path = path.to_string();
            let executor = cx.background_executor().clone();
            cx.spawn(async move |this, cx| {
                let result = image_store::load_thumb(&engine, &path, &executor).await;
                match result {
                    Ok(thumb) => image_store::store_loaded(&path, thumb),
                    Err(cause) => image_store::store_error(&path, cause),
                }
                this.update(cx, |_, cx| cx.notify()).ok();
            })
            .detach();
        }
        image_store::snapshot(path)
    }

    /// The paperclip: the native file/folder picker. Everything selected becomes
    /// a path reference (images included — bytes stay on disk).
    pub(super) fn open_file_picker(&mut self, cx: &mut Context<Self>) {
        let draft_key = self.current_key.clone();
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        self.picker_task = Some(cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update(cx, |composer, cx| {
                    composer.add_paths_to_draft(paths, draft_key, cx)
                })
                .ok();
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    /// Ticket 09's composer seam: a dragged/attached tree entry stages as a
    /// path reference — bound to its live target, deduplicated per draft —
    /// and never sends anything (no queue touch, no failure notice for a
    /// healthy path).
    #[gpui::test]
    fn tree_entries_stage_deduplicated_without_sending(cx: &mut gpui::TestAppContext) {
        let cx = cx.add_empty_window();
        cx.update(|_, cx| cx.set_global(Theme::default()));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a file.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let folder = dir.path().join("pkg");
        std::fs::create_dir_all(&folder).unwrap();

        let state = cx.new(|_| AppState::new());
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));
        // The same drop landing twice stays one chip; a folder joins as a
        // folder reference; a file inside it coexists.
        composer.update(cx, |composer, cx| {
            composer.add_paths(vec![file.clone()], cx);
            composer.add_paths(vec![file.clone()], cx);
            composer.add_paths(vec![folder.clone()], cx);
        });
        composer.update(cx, |composer, _| {
            // bind canonicalizes (macOS tempdirs live behind /var →
            // /private/var), so compare against the resolved targets.
            let file = std::fs::canonicalize(&file).unwrap();
            let folder = std::fs::canonicalize(&folder).unwrap();
            let refs = composer.staged_refs();
            assert_eq!(refs.len(), 2, "duplicate attachments dedup: {refs:?}");
            assert!(refs.iter().any(|r| r.path == file && !r.is_dir));
            assert!(refs.iter().any(|r| r.path == folder && r.is_dir));
            assert!(composer.failure.is_none());
            assert!(!composer.sending);
        });
        // Nothing was queued or sent: the empty AppState still has no queue.
        state.read_with(cx, |state, _| assert!(state.message_queue.is_none()));
    }
}
