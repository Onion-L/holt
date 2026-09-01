//! The interface font/size pickers: menu toggling, keyboard navigation, and the pure stepping helpers.

use super::*;

impl AppearancePage {
    pub(super) fn commit_font(&mut self, cx: &mut Context<Self>) {
        if typography::is_available(&self.selected_font, cx) {
            typography::set_family(self.selected_font.clone(), cx);
            self.selected_font = typography::effective(cx);
            self.close_font_menu(cx);
            cx.notify();
        }
    }

    pub(super) fn commit_size(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        typography::set_font_size(self.selected_size, window, cx);
        self.selected_size = typography::font_size(cx);
        self.close_size_menu(cx);
        cx.notify();
    }

    pub(super) fn close_font_menu(&mut self, cx: &mut Context<Self>) {
        if self.font_menu.begin_close() {
            popover::reap_popup(cx, |page| &mut page.font_menu);
        }
    }

    pub(super) fn close_size_menu(&mut self, cx: &mut Context<Self>) {
        if self.size_menu.begin_close() {
            popover::reap_popup(cx, |page| &mut page.size_menu);
        }
    }

    pub(super) fn dismiss_font_menu(&mut self, cx: &mut Context<Self>) {
        self.font_menu_dismissed_at = Some(std::time::Instant::now());
        self.close_font_menu(cx);
    }

    pub(super) fn dismiss_size_menu(&mut self, cx: &mut Context<Self>) {
        self.size_menu_dismissed_at = Some(std::time::Instant::now());
        self.close_size_menu(cx);
    }

    pub(super) fn toggle_font_menu(&mut self, cx: &mut Context<Self>) {
        self.close_size_menu(cx);
        let just_dismissed = self
            .font_menu_dismissed_at
            .take()
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(400));
        if self.font_menu.is_open() {
            self.close_font_menu(cx);
        } else if !just_dismissed {
            self.selected_font = typography::effective(cx);
            self.font_menu.open(());
        }
        cx.notify();
    }

    pub(super) fn toggle_size_menu(&mut self, cx: &mut Context<Self>) {
        self.close_font_menu(cx);
        let just_dismissed = self
            .size_menu_dismissed_at
            .take()
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(400));
        if self.size_menu.is_open() {
            self.close_size_menu(cx);
        } else if !just_dismissed {
            self.selected_size = typography::font_size(cx);
            self.size_menu.open(());
        }
        cx.notify();
    }

    pub(super) fn on_font_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let availability = typography::availability(cx);
        match event.keystroke.key.as_str() {
            "up" | "left" => {
                if !self.font_menu.is_open() {
                    self.font_menu_dismissed_at = None;
                    self.toggle_font_menu(cx);
                }
                self.selected_font = step_font(&self.selected_font, -1, &availability);
                cx.notify();
            }
            "down" | "right" => {
                if !self.font_menu.is_open() {
                    self.font_menu_dismissed_at = None;
                    self.toggle_font_menu(cx);
                }
                self.selected_font = step_font(&self.selected_font, 1, &availability);
                cx.notify();
            }
            "home" => {
                if !self.font_menu.is_open() {
                    self.font_menu_dismissed_at = None;
                    self.toggle_font_menu(cx);
                }
                self.selected_font = first_available(&availability);
                cx.notify();
            }
            "end" => {
                if !self.font_menu.is_open() {
                    self.font_menu_dismissed_at = None;
                    self.toggle_font_menu(cx);
                }
                self.selected_font = last_available(&availability);
                cx.notify();
            }
            "enter" | "space" => {
                if self.font_menu.is_open() {
                    self.commit_font(cx);
                } else {
                    self.font_menu_dismissed_at = None;
                    self.toggle_font_menu(cx);
                }
            }
            "escape" => {
                self.selected_font = typography::effective(cx);
                self.close_font_menu(cx);
                cx.notify();
            }
            _ => {}
        }
    }

    pub(super) fn on_size_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = UiFontSize::ALL
            .iter()
            .position(|size| *size == self.selected_size)
            .unwrap_or(4);
        match event.keystroke.key.as_str() {
            "up" | "left" => {
                if !self.size_menu.is_open() {
                    self.size_menu_dismissed_at = None;
                    self.toggle_size_menu(cx);
                }
                self.selected_size = UiFontSize::ALL[current.saturating_sub(1)];
                cx.notify();
            }
            "down" | "right" => {
                if !self.size_menu.is_open() {
                    self.size_menu_dismissed_at = None;
                    self.toggle_size_menu(cx);
                }
                self.selected_size = UiFontSize::ALL[(current + 1).min(UiFontSize::ALL.len() - 1)];
                cx.notify();
            }
            "home" => {
                if !self.size_menu.is_open() {
                    self.size_menu_dismissed_at = None;
                    self.toggle_size_menu(cx);
                }
                self.selected_size = UiFontSize::ALL[0];
                cx.notify();
            }
            "end" => {
                if !self.size_menu.is_open() {
                    self.size_menu_dismissed_at = None;
                    self.toggle_size_menu(cx);
                }
                self.selected_size = UiFontSize::ALL[UiFontSize::ALL.len() - 1];
                cx.notify();
            }
            "enter" | "space" => {
                if self.size_menu.is_open() {
                    self.commit_size(window, cx);
                } else {
                    self.size_menu_dismissed_at = None;
                    self.toggle_size_menu(cx);
                }
            }
            "escape" => {
                self.selected_size = typography::font_size(cx);
                self.close_size_menu(cx);
                cx.notify();
            }
            _ => {}
        }
    }
}

fn step_font(
    current: &UiFontFamily,
    delta: isize,
    availability: &FontAvailability,
) -> UiFontFamily {
    let choices = availability.choices();
    let current = choices
        .iter()
        .position(|family| family == current)
        .unwrap_or_default() as isize;
    let mut ix = current + delta.signum();
    while (0..choices.len() as isize).contains(&ix) {
        let candidate = &choices[ix as usize];
        if availability.is_available(candidate) {
            return candidate.clone();
        }
        ix += delta.signum();
    }
    choices[current as usize].clone()
}

fn first_available(availability: &FontAvailability) -> UiFontFamily {
    availability
        .choices()
        .iter()
        .find(|family| availability.is_available(family))
        .cloned()
        .unwrap_or(UiFontFamily::System)
}

fn last_available(availability: &FontAvailability) -> UiFontFamily {
    availability
        .choices()
        .iter()
        .rev()
        .find(|family| availability.is_available(family))
        .cloned()
        .unwrap_or(UiFontFamily::System)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_options_appear_once_in_stable_order() {
        let catalog = FontAvailability::all();
        let labels: Vec<_> = catalog.choices().iter().map(UiFontFamily::label).collect();
        assert_eq!(labels.len(), 5);
        assert_eq!(
            labels,
            ["Geist", "Geist Mono", "System UI", "Arial", "Menlo"]
        );
        let unique = labels.into_iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), 5);
    }

    #[test]
    fn font_keyboard_navigation_stops_at_edges_and_skips_unavailable() {
        let all = FontAvailability::all();
        assert_eq!(
            step_font(&UiFontFamily::Geist, -1, &all),
            UiFontFamily::Geist
        );
        assert_eq!(
            step_font(&UiFontFamily::Installed("Menlo".into()), 1, &all),
            UiFontFamily::Installed("Menlo".into())
        );
        let without_arial = all.without(&UiFontFamily::Installed("Arial".into()));
        assert_eq!(
            step_font(&UiFontFamily::System, 1, &without_arial),
            UiFontFamily::Installed("Menlo".into())
        );
    }

    #[test]
    fn font_size_options_are_ordered_and_include_the_default() {
        let values = UiFontSize::ALL.map(UiFontSize::pixels);
        assert!(values.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(UiFontSize::ALL.contains(&UiFontSize::default()));
    }
}
