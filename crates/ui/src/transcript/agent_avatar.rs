//! Subagent chip identities: every agent call gets a random NAME and a
//! random matte pastel, drawn without replacement so a fan-out of concurrent
//! children never repeats either. The identity keys on the tool CALL id —
//! present from the chip's first paint, before the engine stamps the
//! subagent's doc ref — and lives for the session (a reopened chat re-draws:
//! the name is garnish, not a record).
//!
//! The avatar is the chip's 22px leading face: a full-bleed squircle in the
//! identity color with two white pill eyes. Running subagents blink and
//! glance (phases ride the shared [`motion::pulse_delta`] clock, offset per
//! doc id so siblings desync); done settles into happy-arch eyes; failed
//! freezes mid-look. Reduced motion pins every face static.

use std::collections::HashMap;

use gpui::{App, Div, EntityId, Hsla, ParentElement, SharedString, Styled, div, px};

use crate::motion::{self, MotionSpec};

/// The name pool — famous programmers, first names only (they read as
/// nametags, not citations). 17 names: with a four-running semaphore the
/// deck essentially never recycles inside one fan-out.
const NAMES: &[&str] = &[
    "Ada", "Alan", "Dennis", "Ken", "Linus", "Grace", "Edsger", "Margaret", "Donald", "Brian",
    "Bjarne", "Guido", "Barbara", "Radia", "Claude", "Brendan", "Graydon",
];

/// The pastel pool: fixed matte fills, readable in both themes (white eyes
/// carry the contrast). Eight colors, one draw per subagent.
const COLORS: &[u32] = &[
    0x45c9a2, // mint
    0xf0a244, // amber
    0x7f84ee, // periwinkle
    0xa171f2, // purple
    0x4f8ef2, // blue
    0x3fb8d4, // teal
    0xef7470, // coral
    0xe87fb8, // pink
];

/// Blink cadence: one dip per period, offset per agent.
pub(crate) const AVATAR_BLINK: MotionSpec = MotionSpec::new(5200, motion::EASE);
/// Glance cadence: one sideways look per period, slower than the blink.
pub(crate) const AVATAR_GLANCE: MotionSpec = MotionSpec::new(7000, motion::EASE);

/// Eye openness (1 = fully open) for a blink phase: fully open except a
/// triangular dip to 0.15 across the period's last 4.5% (~230ms at 5.2s).
fn blink_openness(phase: f32) -> f32 {
    const START: f32 = 0.955;
    if phase <= START {
        return 1.0;
    }
    let t = (phase - START) / (1.0 - START);
    let dip = 1.0 - (2.0 * t - 1.0).abs();
    1.0 - 0.85 * dip
}

/// Horizontal gaze shift in px for a glance phase: one sine in-out to `dir`
/// across 55%–75% of the period (1.4s at 7s), rest at center.
fn gaze_shift(phase: f32, dir: f32) -> f32 {
    const START: f32 = 0.55;
    const END: f32 = 0.75;
    if !(START..=END).contains(&phase) {
        return 0.0;
    }
    let t = (phase - START) / (END - START);
    dir * (std::f32::consts::PI * t).sin()
}

/// A subagent's chip identity: the name the chip wears, the squircle's
/// fill, and the animation personality (blink/glance phase offset and glance
/// side, drawn with the identity so siblings desync). Copy — render sites
/// resolve per paint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AgentIdentity {
    pub name: &'static str,
    pub color: Hsla,
    /// Blink/glance phase offset [0,1): siblings share the global clock, so
    /// this is what keeps a fan-out from blinking in unison.
    pub phase: f32,
    /// Glance side: -1.0 (left) or 1.0 (right).
    pub gaze_dir: f32,
}

/// One transcript's identity registry: doc-id-keyed assignments plus the two
/// decks the draw comes from. A deck refills (reshuffled) only when empty, so
/// repeats need the whole pool cycled first; a refill that would hand back
/// the just-drawn card swaps it away.
pub(crate) struct AgentIdentities {
    rng: u64,
    names: Vec<&'static str>,
    colors: Vec<Hsla>,
    /// The full color deck, kept to refill `colors` from (the pool is hex
    /// consts; the deck deals the converted Hslas).
    color_pool: Vec<Hsla>,
    by_doc: HashMap<SharedString, AgentIdentity>,
}

impl AgentIdentities {
    pub fn new(seed: u64) -> Self {
        let color_pool: Vec<Hsla> = COLORS
            .iter()
            .map(|&hex| Hsla::from(gpui::rgb(hex)))
            .collect();
        let mut this = Self {
            rng: seed | 1,
            names: NAMES.to_vec(),
            colors: color_pool.clone(),
            color_pool,
            by_doc: HashMap::new(),
        };
        shuffle(&mut this.names, &mut this.rng);
        shuffle(&mut this.colors, &mut this.rng);
        this
    }

    /// The identity for one subagent doc, assigning on first sight.
    pub fn for_doc(&mut self, doc_id: &str) -> AgentIdentity {
        if let Some(identity) = self.by_doc.get(doc_id) {
            return *identity;
        }
        let color_pool = self.color_pool.clone();
        let identity = AgentIdentity {
            name: draw(&mut self.names, NAMES, &mut self.rng),
            color: draw(&mut self.colors, &color_pool, &mut self.rng),
            phase: (next_u64(&mut self.rng) % 1000) as f32 / 1000.0,
            gaze_dir: if next_u64(&mut self.rng) & 1 == 0 {
                -1.0
            } else {
                1.0
            },
        };
        self.by_doc
            .insert(SharedString::from(doc_id.to_owned()), identity);
        identity
    }
}

fn next_u64(state: &mut u64) -> u64 {
    // xorshift64* — naming garnish, not security.
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fn shuffle<T>(deck: &mut [T], rng: &mut u64) {
    for i in (1..deck.len()).rev() {
        let j = (next_u64(rng) % (i as u64 + 1)) as usize;
        deck.swap(i, j);
    }
}

/// Pop the top card; on an empty deck rebuild it full and reshuffle, keeping
/// the redraw from repeating the card just handed out when the pool has a
/// choice.
fn draw<T: Copy + PartialEq>(deck: &mut Vec<T>, pool: &[T], rng: &mut u64) -> T {
    if deck.is_empty() {
        deck.extend_from_slice(pool);
        shuffle(deck, rng);
    }
    let card = deck.pop().expect("deck refilled");
    if deck.is_empty() && pool.len() > 1 {
        deck.extend_from_slice(pool);
        shuffle(deck, rng);
        if deck.last() == Some(&card) {
            let last = deck.len() - 1;
            deck.swap(0, last);
        }
    }
    card
}

/// Which face the avatar wears: eyes open and wandering while the subagent
/// runs, happy arches when it settled, a frozen open look on failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AvatarMood {
    Running,
    Done,
    Failed,
}

/// The 22px squircle face leading a spawn chip. Paint-local: eyes breathe
/// inside fixed slots, the row never relays out.
pub(crate) fn agent_avatar(
    identity: AgentIdentity,
    mood: AvatarMood,
    view: EntityId,
    cx: &mut App,
) -> Div {
    const SIZE: f32 = 22.0;
    /// One 64-grid unit in px — the face is drawn on a 64×64 grid and scaled.
    const U: f32 = SIZE / 64.0;
    const EYE_W: f32 = 9.0 * U;
    const EYE_H: f32 = 14.0 * U;
    const EYE_TOP: f32 = 22.5 * U;
    const EYE_LEFTS: [f32; 2] = [20.5 * U, 34.5 * U];
    const SQUIRCLE_R: f32 = 18.0 * U;

    let running = mood == AvatarMood::Running && !motion::reduced_motion(cx);
    let (openness, gaze) = if running {
        let blink = (motion::pulse_delta(&AVATAR_BLINK, view, cx) + identity.phase).rem_euclid(1.0);
        let glance =
            (motion::pulse_delta(&AVATAR_GLANCE, view, cx) + identity.phase).rem_euclid(1.0);
        (blink_openness(blink), gaze_shift(glance, identity.gaze_dir))
    } else {
        (1.0, 0.0)
    };
    let eye_color = gpui::hsla(0.0, 0.0, 1.0, 1.0);
    div()
        .size(px(SIZE))
        .flex_none()
        .relative()
        .rounded(px(SQUIRCLE_R))
        .bg(identity.color)
        .children(EYE_LEFTS.into_iter().map(move |left| {
            let slot = div()
                .absolute()
                .left(px(left + gaze))
                .top(px(EYE_TOP))
                .w(px(EYE_W))
                .h(px(EYE_H))
                .flex()
                .justify_center();
            match mood {
                AvatarMood::Done => {
                    // Happy arch: a white dome (half-pill) centered in the
                    // eye slot — closed smiling eyes.
                    slot.items_end().child(
                        div()
                            .w(px(EYE_W))
                            .h(px(6.0 * U))
                            .rounded_tl(px(EYE_W / 2.0))
                            .rounded_tr(px(EYE_W / 2.0))
                            .bg(eye_color),
                    )
                }
                AvatarMood::Running | AvatarMood::Failed => slot.items_center().child(
                    div()
                        .w(px(EYE_W))
                        .h(px((EYE_H * openness).max(1.0)))
                        .rounded(px(EYE_W / 2.0))
                        .bg(eye_color),
                ),
            }
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_do_not_repeat_within_a_pool_cycle() {
        // One full name cycle from a fresh pool: no repeats.
        let mut identities = AgentIdentities::new(42);
        let names: Vec<_> = (0..NAMES.len())
            .map(|i| identities.for_doc(&format!("doc-{i}")).name)
            .collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), NAMES.len(), "one full cycle, no repeats");
        // Same for colors (a for_doc draw takes one of each, so the color
        // cycle needs its own fresh pool to line up with the draws).
        let mut identities = AgentIdentities::new(42);
        let colors: Vec<_> = (0..COLORS.len())
            .map(|i| identities.for_doc(&format!("color-{i}")).color)
            .collect();
        let mut unique_colors: Vec<Hsla> = Vec::new();
        for color in colors {
            assert!(
                !unique_colors.contains(&color),
                "one full cycle, no repeats"
            );
            unique_colors.push(color);
        }
    }

    #[test]
    fn identity_is_stable_per_doc() {
        let mut identities = AgentIdentities::new(7);
        let first = identities.for_doc("doc-a");
        assert_eq!(identities.for_doc("doc-a"), first);
    }

    #[test]
    fn refill_avoids_immediate_repeats() {
        let mut identities = AgentIdentities::new(99);
        let names: Vec<_> = (0..NAMES.len() * 3)
            .map(|i| identities.for_doc(&format!("doc-{i}")).name)
            .collect();
        for pair in names.windows(2) {
            assert_ne!(pair[0], pair[1], "adjacent draws never repeat");
        }
    }

    #[test]
    fn blink_dips_once_per_period() {
        assert_eq!(blink_openness(0.0), 1.0);
        assert_eq!(blink_openness(0.5), 1.0);
        assert!((blink_openness(0.9775) - 0.15).abs() < 1e-4, "dip bottom");
        assert!(blink_openness(0.97) < 0.5, "falling edge");
        assert_eq!(blink_openness(1.0), 1.0);
    }

    #[test]
    fn gaze_holds_center_outside_the_window() {
        assert_eq!(gaze_shift(0.0, 1.0), 0.0);
        assert_eq!(gaze_shift(0.9, 1.0), 0.0);
        assert!(
            (gaze_shift(0.65, 1.0) - 1.0).abs() < 1e-4,
            "mid-window peak"
        );
        assert!((gaze_shift(0.65, -1.0) + 1.0).abs() < 1e-4, "mirror side");
    }
}
