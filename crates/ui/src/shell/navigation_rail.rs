//! Global navigation rail and the rounded desktop frame. This is separate
//! from `crate::rail`, the transcript's message minimap.

use super::*;

pub(super) const SHELL_INSET: f32 = 0.0;
pub(super) const SHELL_RADIUS: f32 = 26.0;
pub(super) const APP_RADIUS: f32 = 12.0;
pub(super) const NAVIGATION_WIDTH: f32 = 44.0;
pub(super) const NAVIGATION_OVERLAP: f32 = 0.0;

/// Width available to the application after the navigation rail overlap.
pub(super) fn app_content_width(window_width: f32) -> f32 {
    (window_width - 2.0 * SHELL_INSET - NAVIGATION_WIDTH + NAVIGATION_OVERLAP).max(0.0)
}

/// The opaque result of the project sidebar's wash over the shell frost.
/// Using the resolved color prevents dark backing from showing at rounded seams.
pub(super) fn sidebar_frame_color(cx: &App) -> gpui::Hsla {
    crate::theme::flatten(crate::theme::wash(0.05), Theme::of(cx).glass())
}

/// Paint a 1px outline without a filled ring beneath the transcript.
pub(super) fn app_border(cx: &App) -> AnyElement {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left(px(NAVIGATION_WIDTH - NAVIGATION_OVERLAP))
        .right_0()
        .rounded(px(APP_RADIUS))
        .border_1()
        .border_color(Theme::of(cx).border)
        .into_any_element()
}

/// When the chat sidebar is collapsed its wash no longer covers these two
/// corner pockets. Repaint only the area outside the app's rounded outline.
pub(super) fn collapsed_corner_backing(color: gpui::Hsla) -> AnyElement {
    gpui::canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let left = NAVIGATION_WIDTH - NAVIGATION_OVERLAP;
            let radius = APP_RADIUS;
            let handle = radius * 0.552_284_8;
            let height = f32::from(bounds.size.height);
            let at = |x: f32, y: f32| gpui::point(bounds.origin.x + px(x), bounds.origin.y + px(y));

            let mut top = gpui::PathBuilder::fill();
            top.move_to(at(left, 0.0));
            top.line_to(at(left + radius, 0.0));
            top.cubic_bezier_to(
                at(left, radius),
                at(left + radius - handle, 0.0),
                at(left, radius - handle),
            );
            top.close();
            if let Ok(path) = top.build() {
                window.paint_path(path, color);
            }

            let mut bottom = gpui::PathBuilder::fill();
            bottom.move_to(at(left, height - radius));
            bottom.cubic_bezier_to(
                at(left + radius, height),
                at(left, height - radius + handle),
                at(left + radius - handle, height),
            );
            bottom.line_to(at(left, height));
            bottom.close();
            if let Ok(path) = bottom.build() {
                window.paint_path(path, color);
            }
        },
    )
    .absolute()
    .inset_0()
    .into_any_element()
}

#[derive(Clone, Copy)]
enum Destination {
    Home,
    Settings(SettingsSection),
    Scheduled,
    ChatManager,
}

impl Destination {
    fn id(self) -> &'static str {
        match self {
            Self::Home => "navigation-home",
            Self::Settings(section) => match section {
                SettingsSection::General => "navigation-general",
                SettingsSection::Providers => "navigation-providers",
                SettingsSection::Appearance => "navigation-appearance",
                SettingsSection::Shortcuts => "navigation-shortcuts",
                SettingsSection::Skills => "navigation-skills",
                SettingsSection::Mcp => "navigation-mcp",
                SettingsSection::Usage => "navigation-usage",
                SettingsSection::Archived => "navigation-archived",
            },
            Self::Scheduled => "navigation-scheduled",
            Self::ChatManager => "navigation-chat-manager",
        }
    }

    fn label(self) -> SharedString {
        match self {
            Self::Home => t!("shell_chrome.navigation_rail.home").into(),
            Self::Settings(section) => SharedString::from(section.label().into_owned()),
            Self::Scheduled => t!("shell_chrome.titlebar.nav_scheduled").into(),
            Self::ChatManager => t!("shell_chrome.titlebar.nav_chat_manager").into(),
        }
    }

    fn icon(self) -> (&'static str, f32) {
        match self {
            Self::Home => (icons::HOME, 18.0),
            Self::Settings(SettingsSection::General) => (icons::SETTINGS_MINIMALISTIC, 18.0),
            Self::Settings(SettingsSection::Usage) => (icons::NAVIGATION_CHART, 18.0),
            Self::Scheduled => (icons::CLOCK_CIRCLE, 16.0),
            Self::ChatManager => (icons::INBOX, 16.0),
            _ => unreachable!("only mounted destinations render navigation icons"),
        }
    }

    fn is_active(self, route: Route) -> bool {
        match self {
            Self::Home => matches!(route, Route::Chat),
            Self::Settings(section) => {
                matches!(route, Route::Settings(current) if current == section)
            }
            Self::Scheduled => matches!(route, Route::Scheduled),
            Self::ChatManager => matches!(route, Route::ChatManager),
        }
    }

    fn open(self, shell: &mut Shell, cx: &mut Context<Shell>) {
        match self {
            Self::Home => shell.close_settings(cx),
            Self::Settings(section) => shell.open_settings(section, cx),
            Self::Scheduled => shell.open_scheduled(cx),
            Self::ChatManager => shell.open_chat_manager(cx),
        }
    }
}

impl Shell {
    fn navigation_button(
        &self,
        destination: Destination,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let label = destination.label();
        let active = destination.is_active(self.route);
        let theme = Theme::of(cx);
        let live = if matches!(destination, Destination::Scheduled) {
            let state = self.state.read(cx);
            let outcomes = || {
                state
                    .routines
                    .iter()
                    .filter_map(|view| crate::scheduled::live_run(&view.routine))
                    .map(|run| run.outcome)
            };
            if outcomes().any(|outcome| outcome == holt_proto::RunOutcome::Waiting) {
                Some(theme.warning)
            } else if outcomes().next().is_some() {
                Some(theme.busy)
            } else {
                None
            }
        } else {
            None
        };
        let (path, size) = destination.icon();
        let content = icon(path)
            .size(px(size))
            .text_color(if active { theme.text } else { theme.text_muted })
            .into_any_element();
        div()
            .id(destination.id())
            .debug_selector(move || destination.id().to_string())
            .relative()
            .role(gpui::Role::Button)
            .aria_label(label.clone())
            .size(px(32.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(8.0))
            .cursor_pointer()
            // The rail is chrome on glass: the sidebar-row token pair. An
            // active destination keeps its selected wash on hover — light's
            // hover sits below its selected fill and would dim it (chat_list
            // row lesson).
            .when(active, |el| el.bg(crate::theme::glass_selected_bg()))
            .when(!active, |el| {
                el.hover(|style| style.bg(theme.glass_hover()))
            })
            .child(content)
            .tooltip(move |_, cx| {
                cx.new(|_| crate::popover::TextTooltip(label.clone()))
                    .into()
            })
            .on_click(cx.listener(move |this, _, _, cx| destination.open(this, cx)))
            .when_some(live, |el, color| {
                el.child(
                    div()
                        .debug_selector(|| "navigation-scheduled-live".into())
                        .absolute()
                        .top_0()
                        .right_0()
                        .size(px(5.0))
                        .rounded_full()
                        .bg(color),
                )
            })
    }

    pub(super) fn render_navigation_rail(&self, cx: &mut Context<Self>) -> AnyElement {
        let top = [
            Destination::Home,
            Destination::Scheduled,
            Destination::ChatManager,
        ];
        let bottom = [
            Destination::Settings(SettingsSection::Usage),
            Destination::Settings(SettingsSection::General),
        ];
        div()
            .w(px(NAVIGATION_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(14.0))
            .pb(px(8.0))
            .bg(sidebar_frame_color(cx))
            .rounded_tl(px(SHELL_RADIUS))
            .rounded_bl(px(SHELL_RADIUS))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(13.0))
                    .children(
                        top.into_iter()
                            .map(|destination| self.navigation_button(destination, cx)),
                    ),
            )
            .child(
                div()
                    .mt_auto()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(13.0))
                    .children(
                        bottom
                            .into_iter()
                            .map(|destination| self.navigation_button(destination, cx)),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_geometry_leaves_the_expected_app_viewport() {
        assert_eq!(app_content_width(1320.0), 1276.0);
        assert_eq!(app_content_width(0.0), 0.0);
    }

    #[test]
    fn active_destination_follows_the_existing_route() {
        assert!(Destination::Home.is_active(Route::Chat));
        assert!(Destination::Scheduled.is_active(Route::Scheduled));
        assert!(Destination::ChatManager.is_active(Route::ChatManager));
        assert!(
            Destination::Settings(SettingsSection::Usage)
                .is_active(Route::Settings(SettingsSection::Usage))
        );
        assert!(
            !Destination::Settings(SettingsSection::Usage)
                .is_active(Route::Settings(SettingsSection::Providers))
        );
    }
}
