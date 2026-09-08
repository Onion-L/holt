//! Embedded icon assets + the gpui [`AssetSource`] that serves them.
//!
//! The set mirrors the original holt's icon usage exactly:
//! - Most glyphs come from the **Solar Icons** set (Linear weight) by 480 Design,
//!   the same set the Electron app used via `@solar-icons/react`. Solar Icons is
//!   licensed under CC BY 4.0 (https://creativecommons.org/licenses/by/4.0/);
//!   attribution: "Solar Icons by 480 Design".
//! - The terminal tab glyphs (`terminal`, `plus`, `close`) and the stop square
//!   are ports of the hand-drawn inline SVGs in holt's `terminal-panel.tsx` /
//!   `composer-actions.tsx`.
//!
//! Icons render via [`icon`]: `icon(icons::PAPERCLIP).size(px(16.)).text_color(…)`.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString, Styled as _, Svg, svg};

macro_rules! provider_assets {
    ($(($const_name:ident, $path:literal)),+ $(,)?) => {
        $(pub const $const_name: &str = concat!("providers/", $path, ".svg");)+

        const PROVIDER_ASSET_PATHS: &[&str] = &[
            $(concat!("providers/", $path, ".svg")),+
        ];

        fn load_provider_asset(path: &str) -> Option<Cow<'static, [u8]>> {
            match path {
                $(concat!("providers/", $path, ".svg") => Some(Cow::Borrowed(
                    include_bytes!(concat!("../assets/providers/", $path, ".svg")).as_slice(),
                )),)+
                _ => None,
            }
        }
    };
}

provider_assets![
    (PROVIDER_ANT_LING, "ant-ling"),
    (PROVIDER_ANTHROPIC, "anthropic"),
    (PROVIDER_ANTHROPIC_DARK, "anthropic-dark"),
    (PROVIDER_ANTHROPIC_LIGHT, "anthropic-light"),
    (PROVIDER_BASETEN, "baseten"),
    (PROVIDER_CEREBRAS, "cerebras"),
    (PROVIDER_DEEPSEEK, "deepseek"),
    (PROVIDER_FIREWORKS, "fireworks"),
    (PROVIDER_GITHUB_COPILOT, "github-copilot"),
    (PROVIDER_GOOGLE, "google"),
    (PROVIDER_GROQ, "groq"),
    (PROVIDER_HUGGINGFACE, "huggingface"),
    (PROVIDER_KIMI_CODING, "kimi-coding"),
    (PROVIDER_MINIMAX, "minimax"),
    (PROVIDER_MINIMAX_DARK, "minimax-dark"),
    (PROVIDER_MINIMAX_LIGHT, "minimax-light"),
    (PROVIDER_MISTRAL, "mistral"),
    (PROVIDER_MOONSHOTAI, "moonshotai"),
    (PROVIDER_NVIDIA, "nvidia"),
    (PROVIDER_OPENAI, "openai"),
    (PROVIDER_OPENAI_LIGHT, "openai-light"),
    (PROVIDER_OPENCODE, "opencode"),
    (PROVIDER_OPENCODE_DARK, "opencode-dark"),
    (PROVIDER_OPENCODE_LIGHT, "opencode-light"),
    (PROVIDER_OPENROUTER, "openrouter"),
    (PROVIDER_QWEN, "qwen"),
    (PROVIDER_TOGETHER, "together"),
    (PROVIDER_VERCEL_AI_GATEWAY, "vercel-ai-gateway"),
    (PROVIDER_XAI, "xai"),
    (PROVIDER_XIAOMI, "xiaomi"),
    (PROVIDER_ZAI, "zai"),
];

macro_rules! icon_assets {
    ($(($const_name:ident, $path:literal)),+ $(,)?) => {
        $(pub const $const_name: &str = concat!("icons/", $path, ".svg");)+

        /// Serves the embedded icons to gpui's SVG renderer.
        pub struct Assets;

        impl AssetSource for Assets {
            fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
                Ok(match path {
                    $(concat!("icons/", $path, ".svg") => Some(Cow::Borrowed(
                        include_bytes!(concat!("../assets/icons/", $path, ".svg")).as_slice(),
                    )),)+
                    _ => load_provider_asset(path),
                })
            }

            fn list(&self, path: &str) -> Result<Vec<SharedString>> {
                let all = [$(concat!("icons/", $path, ".svg")),+];
                Ok(all
                    .iter()
                    .copied()
                    .chain(PROVIDER_ASSET_PATHS.iter().copied())
                    .filter(|p| p.starts_with(path))
                    .map(SharedString::from)
                    .collect())
            }
        }
    };
}

icon_assets![
    // Solar Icons (Linear), CC BY 4.0 — 480 Design.
    (MONITOR, "monitor"),
    (LAPTOP, "laptop"),
    (PEN_NEW_SQUARE, "pen-new-square"),
    (SORT, "sort"),
    (SORT_VERTICAL, "sort-vertical"),
    (CLOCK_CIRCLE, "clock-circle"),
    // Context checkpoint glyph in the same linear style.
    (CONTEXT_COMPACT, "context-compact"),
    (CALENDAR, "calendar"),
    (LIST, "list"),
    (FOLDER_WITH_FILES, "folder-with-files"),
    (FOLDER, "folder"),
    // Hand-drawn git-branch glyph in the Solar Linear style (like the
    // terminal/plus/return ports) — the set has no branch icon.
    (GIT_BRANCH, "git-branch"),
    // Provider-neutral pull-request glyph, drawn in the same linear family.
    (PULL_REQUEST, "pull-request"),
    // Compact history-ref glyphs, drawn in the same linear style.
    (CLOUD, "cloud"),
    (TAG, "tag"),
    (SIDEBAR_MINIMALISTIC, "sidebar-minimalistic"),
    (PROGRAMMING_OUTLINE, "programming-outline"),
    // Mirrored variant (holt window-controls.tsx `-scale-x-100`): the LEFT
    // sidebar toggle shows the panel line on the left; gpui divs have no
    // scale transform at the pinned rev, so the flip is baked into the asset.
    (SIDEBAR_MINIMALISTIC_LEFT, "sidebar-minimalistic-left"),
    (KEY_MINIMALISTIC, "key-minimalistic"),
    (KEYBOARD, "keyboard"),
    (ARROW_LEFT, "arrow-left"),
    (ARROW_RIGHT, "arrow-right"),
    (ARROW_UP, "arrow-up"),
    // arrow-up mirrored (like the sidebar flip) — the Solar Linear set here
    // has no plain arrow-down.
    (ARROW_DOWN, "arrow-down"),
    // arrow-up rotated 45° — the "opens elsewhere" glyph on spawn chips;
    // the set has no diagonal arrow.
    (ARROW_UP_RIGHT, "arrow-up-right"),
    // Hand-drawn return/enter arrow in the Solar Linear style (like the
    // terminal/plus/close ports) — the set has no return glyph.
    (RETURN, "return"),
    (ALT_ARROW_DOWN, "alt-arrow-down"),
    // Hand-drawn expand/maximize arrows in the Solar Linear style (like the
    // terminal/plus/return ports) — the set has no expand glyph.
    (EXPAND_ARROWS, "expand-arrows"),
    // Inward-pointing companion used to restore an expanded pane.
    (COLLAPSE_ARROWS, "collapse-arrows"),
    // Hand-drawn fold-all chevrons, drawn as a family with EXPAND_ARROWS
    // (same stroke, caps, 90° joints) — Solar has no unfold-less either.
    (FOLD_VERTICAL, "fold-vertical"),
    // The changes pane's unified/split toggle: a rounded frame halved by a
    // centre rule (Solar Linear weight).
    (SPLIT_COLUMNS, "split-columns"),
    (ALT_ARROW_LEFT, "alt-arrow-left"),
    (ALT_ARROW_RIGHT, "alt-arrow-right"),
    (SMARTPHONE, "smartphone"),
    (ARCHIVE_UP_MINIMALISTIC, "archive-up-minimalistic"),
    (DOWNLOAD_MINIMALISTIC, "download-minimalistic"),
    (REFRESH, "refresh"),
    (RESTART, "restart"),
    (ADD_CIRCLE, "add-circle"),
    (TUNING, "tuning"),
    (PAPERCLIP, "paperclip"),
    (PEN, "pen"),
    (ARCHIVE_MINIMALISTIC, "archive-minimalistic"),
    (TRASH_BIN_MINIMALISTIC, "trash-bin-minimalistic"),
    (SETTINGS_MINIMALISTIC, "settings-minimalistic"),
    (LOGOUT_2, "logout-2"),
    (MAGNIFER, "magnifer"),
    (COMMAND, "command"),
    (DOCUMENT, "document"),
    (DOCUMENT_ADD, "document-add"),
    (GLOBAL, "global"),
    (CHECKLIST, "checklist"),
    (WIDGET, "widget"),
    // Hand-drawn isometric cube in the Solar Linear style (like the
    // terminal/plus/return ports; the embedded set has no cube/box glyph).
    // The skill chip's identity glyph.
    (CUBE, "cube"),
    // Hand-drawn file-tree glyph (spine + indented rows) in the same linear
    // family — the File sidebar's toggle.
    (TREE_SIDEBAR, "tree-sidebar"),
    (WIFI_OFF, "wifi-off"),
    (CLOSE_CIRCLE, "close-circle"),
    // Hand-drawn info glyph in the Solar Linear style (like the terminal/
    // plus/return ports) — the embedded set has no info-circle.
    (INFO_CIRCLE, "info-circle"),
    (DANGER_TRIANGLE, "danger-triangle"),
    (CHAT_ROUND_LINE, "chat-round-line"),
    // Hand-drawn bot head (antenna + eyes + ears) in the Solar Linear style
    // — the embedded set has no bot/robot glyph. Subagent tabs.
    (BOT, "bot"),
    // Hand-drawn bell + speaker in the Solar Linear style (like the terminal/
    // plus/return ports) — the embedded set has neither.
    (BELL, "bell"),
    (VOLUME_LOUD, "volume-loud"),
    // Hand-drawn holt glyphs (terminal-panel.tsx / composer-actions.tsx /
    // menu-check.tsx / logo.tsx).
    (TERMINAL, "terminal"),
    (PLUS, "plus"),
    (CLOSE, "close"),
    // Hand-drawn Linux caption glyphs (minimize dash, maximize square,
    // restore stacked squares) in the same style as `close` — drawn for the
    // client-side-decoration window controls; no system glyph font exists on
    // Linux the way Segoe Fluent Icons does on Windows.
    (WINDOW_MINIMIZE, "window-minimize"),
    (WINDOW_MAXIMIZE, "window-maximize"),
    (WINDOW_RESTORE, "window-restore"),
    // Hand-drawn hard-drive + home glyphs in the Solar Linear style (like the
    // terminal/plus/return ports) — drawn for the add-space palette's
    // Locations rail; the set has neither.
    (HARD_DRIVE, "hard-drive"),
    (HOME, "home"),
    (STOP, "stop"),
    (CHECK, "check"),
    (COPY, "copy"),
    // Hand-drawn eye glyphs in the Solar Linear style (like the terminal/
    // plus/close ports) — the embedded set has neither. The slashed variant
    // marks the currently-visible state of a secret input's toggle.
    (EYE, "eye"),
    (EYE_SLASH, "eye-slash"),
    // Hand-drawn shield + open-lock glyphs in the Solar Linear style (like
    // the terminal/plus/return ports) — the embedded set has neither. The
    // permission-mode tier marks (ADR-0014).
    (SHIELD, "shield"),
    (LOCK_OPEN, "lock-open"),
];

/// An icon element for an embedded asset path. Size and colour are set by the
/// caller (`.size(..)`, `.text_color(..)`), matching the web app's
/// `[&_svg]:size-4` idiom.
pub fn icon(path: &'static str) -> Svg {
    svg().path(path).flex_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_asset_loads_and_parses() {
        let assets = Assets;
        for path in assets.list("").unwrap() {
            let bytes = assets
                .load(&path)
                .unwrap()
                .unwrap_or_else(|| panic!("missing asset {path}"));
            let text = std::str::from_utf8(&bytes).expect("icon svg is utf-8");
            assert!(text.contains("<svg"), "{path} is not an svg");
            assert!(text.contains("viewBox"), "{path} lacks a viewBox");
        }
    }

    #[test]
    fn unknown_paths_are_none() {
        assert!(Assets.load("icons/nope.svg").unwrap().is_none());
    }

    #[test]
    fn list_filters_by_prefix() {
        assert!(!Assets.list("icons/").unwrap().is_empty());
        assert_eq!(Assets.list("providers/").unwrap().len(), 31);
        assert!(Assets.list("fonts/").unwrap().is_empty());
    }
}
