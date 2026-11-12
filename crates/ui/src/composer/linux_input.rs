//! Holt-side mitigations for the Linux input pipeline (issue #85).
//!
//! The Linux platform delivers a key's action before it inserts the
//! character, and an IME commit already queued behind Enter can land after
//! the draft has been sent. It can also drop a printable key that reached
//! the window while no input handler was installed (the new-session focus
//! handoff). The platform crate is owned by gpui-kit — Holt does not vendor
//! it (ADR-linux-input-mitigations-stay-in-holt) — so the decisions live
//! here and the call sites are Linux-only. The functions are pure so the
//! regression tests run on every platform.

use std::time::Duration;

/// How long a Linux submit waits after the last edit before the draft is
/// sent. Long enough for a commit already queued behind Enter to land,
/// short enough that an intentional send does not feel late.
pub const SUBMIT_QUIET: Duration = Duration::from_millis(40);
/// How many extra quiet periods a burst of late commits may chain after
/// the first. The draft sends on the ninth wake at the latest, about 360ms.
pub const SUBMIT_MAX_WAITS: u32 = 8;
/// After a new session, watch this long for printable keys the platform
/// accepted but did not insert. The reporter's "wait ~3s" window sits
/// inside this.
pub const NEW_SESSION_CAPTURE: Duration = Duration::from_secs(4);

/// One quiet-timer wake while a Linux submit is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitWake {
    /// An edit arrived during this wait and the cap is not exhausted.
    /// The caller records one more wait and sleeps [`SUBMIT_QUIET`] again.
    Wait,
    /// The draft has been quiet for a whole wait, or the cap is exhausted.
    Fire,
}

/// `edits_now` is the count at wake time, `seen_edits` the count when this
/// wait was scheduled, `waits` how many waits have already been chained.
pub fn submit_on_wake(edits_now: u64, seen_edits: u64, waits: u32) -> SubmitWake {
    if edits_now != seen_edits && waits < SUBMIT_MAX_WAITS {
        SubmitWake::Wait
    } else {
        SubmitWake::Fire
    }
}

/// A printable insert the platform would apply itself when an input handler
/// is installed: shift-only modifiers, a non-control `key_char`, and not a
/// key the composer binds.
pub fn printable_insert(keystroke: &gpui::Keystroke) -> Option<&str> {
    let text = keystroke.key_char.as_deref()?;
    if text.is_empty() || text.chars().any(|c| c.is_control()) {
        return None;
    }
    if !keystroke.modifiers.is_subset_of(&gpui::Modifiers::shift()) {
        return None;
    }
    if matches!(
        keystroke.key.as_str(),
        "enter" | "tab" | "escape" | "backspace" | "delete"
    ) {
        return None;
    }
    Some(text)
}

/// One printable key observed while the composer might still drop it.
/// `epoch` is the input's edit epoch at dispatch, before the platform's
/// own insert.
pub struct CapturedKey<'a> {
    pub epoch: u64,
    pub text: &'a str,
}

/// Keys the platform did not apply, in order. A key landed when the epoch
/// advanced before the next observation — including a replace whose text
/// equals what was already there. Keys that did not land are returned;
/// keys that did are left out, so one landed key does not drop the rest
/// and a same-text replace is not inserted twice.
pub fn replay_unapplied(keys: &[CapturedKey<'_>], final_epoch: u64) -> Option<String> {
    if keys.is_empty() {
        return None;
    }
    let mut missing = String::new();
    for (index, key) in keys.iter().enumerate() {
        let next_epoch = keys
            .get(index + 1)
            .map(|key| key.epoch)
            .unwrap_or(final_epoch);
        if next_epoch == key.epoch {
            missing.push_str(key.text);
        }
    }
    if missing.is_empty() {
        None
    } else {
        Some(missing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(modifiers: gpui::Modifiers, key: &str, key_char: Option<&str>) -> gpui::Keystroke {
        gpui::Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(str::to_string),
        }
    }

    #[test]
    fn a_quiet_submit_fires_and_a_late_edit_waits_until_the_cap() {
        assert_eq!(submit_on_wake(0, 0, 0), SubmitWake::Fire);
        assert_eq!(submit_on_wake(1, 0, 0), SubmitWake::Wait);
        assert_eq!(submit_on_wake(3, 2, SUBMIT_MAX_WAITS - 1), SubmitWake::Wait);
        assert_eq!(submit_on_wake(4, 3, SUBMIT_MAX_WAITS), SubmitWake::Fire);
        // Quiet again after the bump: the wait that observed the new count fires.
        assert_eq!(submit_on_wake(1, 1, 1), SubmitWake::Fire);
    }

    #[test]
    fn printable_insert_is_shift_only_text() {
        assert_eq!(
            printable_insert(&key(gpui::Modifiers::default(), "a", Some("a"))),
            Some("a")
        );
        assert_eq!(
            printable_insert(&key(gpui::Modifiers::shift(), "A", Some("A"))),
            Some("A")
        );
        let control = gpui::Modifiers {
            control: true,
            ..gpui::Modifiers::default()
        };
        assert_eq!(
            printable_insert(&key(control, "a", Some("a"))),
            None,
            "a chord is not text"
        );
        assert_eq!(
            printable_insert(&key(gpui::Modifiers::default(), "enter", Some("\n"))),
            None
        );
        assert_eq!(
            printable_insert(&key(gpui::Modifiers::default(), "a", None)),
            None,
            "a key the platform did not echo cannot be recovered"
        );
    }

    #[test]
    fn replay_returns_only_the_keys_the_platform_did_not_apply() {
        let dropped = [captured(0, "/"), captured(0, "w")];
        assert_eq!(replay_unapplied(&dropped, 0).as_deref(), Some("/w"));
        // Each key's insert advanced the epoch, including a replace whose
        // text equals the selection it wrote over.
        let applied = [captured(0, "a")];
        assert_eq!(replay_unapplied(&applied, 1), None);
        // The first key landed. The other two did not, and are not dropped
        // with it.
        let partial = [captured(0, "m"), captured(1, "e"), captured(1, "m")];
        assert_eq!(replay_unapplied(&partial, 1).as_deref(), Some("em"));
        assert_eq!(replay_unapplied(&[], 0), None);
    }

    fn captured(epoch: u64, text: &str) -> CapturedKey<'_> {
        CapturedKey { epoch, text }
    }
}
