//! "Open in external app" support for the titlebar workspace menu: the
//! candidate apps, their install detection, and the labels/bundle names
//! used to launch them.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExternalApp {
    Finder,
    VsCode,
    Cursor,
    Zed,
    PyCharm,
    Terminal,
    Ghostty,
}

impl ExternalApp {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Finder => "Finder",
            Self::VsCode => "VS Code",
            Self::Cursor => "Cursor",
            Self::Zed => "Zed",
            Self::PyCharm => "PyCharm",
            Self::Terminal => "Terminal",
            Self::Ghostty => "Ghostty",
        }
    }

    pub(super) fn bundle_name(self) -> Option<&'static str> {
        match self {
            Self::Finder => None,
            Self::VsCode => Some("Visual Studio Code"),
            Self::Cursor => Some("Cursor"),
            Self::Zed => Some("Zed"),
            Self::PyCharm => Some("PyCharm"),
            Self::Terminal => Some("Terminal"),
            Self::Ghostty => Some("Ghostty"),
        }
    }

    /// Full-colour brand mark (PNG under `assets/apps/`), rendered by the
    /// titlebar via `img` — not the tinted Solar SVG path. Cursor/Zed ship
    /// per-appearance variants (`-dark` is the light glyph for dark
    /// surfaces, same convention as the provider brand icons).
    pub(super) fn icon(self, appearance: crate::theme::Appearance) -> &'static str {
        let dark = appearance.is_dark();
        match self {
            Self::Finder => icons::APP_FINDER,
            Self::VsCode => icons::APP_VSCODE,
            Self::Cursor => {
                if dark {
                    icons::APP_CURSOR_DARK
                } else {
                    icons::APP_CURSOR_LIGHT
                }
            }
            Self::Zed => {
                if dark {
                    icons::APP_ZED_DARK
                } else {
                    icons::APP_ZED_LIGHT
                }
            }
            Self::PyCharm => icons::APP_PYCHARM,
            Self::Terminal => icons::APP_TERMINAL,
            Self::Ghostty => icons::APP_GHOSTTY,
        }
    }
}

pub(super) fn available_external_apps() -> Vec<ExternalApp> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    [
        ExternalApp::Finder,
        ExternalApp::VsCode,
        ExternalApp::Cursor,
        ExternalApp::Zed,
        ExternalApp::PyCharm,
        ExternalApp::Terminal,
        ExternalApp::Ghostty,
    ]
    .into_iter()
    .filter(|app| external_app_installed(*app))
    .collect()
}

pub(super) fn external_app_installed(app: ExternalApp) -> bool {
    let candidates: &[&str] = match app {
        ExternalApp::Finder => &["/System/Library/CoreServices/Finder.app"],
        ExternalApp::VsCode => &["/Applications/Visual Studio Code.app"],
        ExternalApp::Cursor => &["/Applications/Cursor.app"],
        ExternalApp::Zed => &["/Applications/Zed.app"],
        ExternalApp::PyCharm => &["/Applications/PyCharm.app", "/Applications/PyCharm CE.app"],
        ExternalApp::Terminal => &["/System/Applications/Utilities/Terminal.app"],
        ExternalApp::Ghostty => &["/Applications/Ghostty.app"],
    };
    candidates.iter().any(|path| Path::new(path).exists())
        || std::env::var_os("HOME").is_some_and(|home| {
            candidates.iter().any(|path| {
                Path::new(path)
                    .file_name()
                    .map(|name| Path::new(&home).join("Applications").join(name).exists())
                    .unwrap_or(false)
            })
        })
}
