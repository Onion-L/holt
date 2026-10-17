//! Subagent chip identities: every agent call gets a random NAME and a
//! random matte pastel, drawn without replacement so a fan-out of concurrent
//! children never repeats either.
//!
//! The identity lands on the chip's first paint keyed by the tool CALL id
//! (the pre-spawn window), then adopts the subagent's doc id when the engine
//! stamps it — same identity, no mid-view swap. Doc-keyed identities persist
//! to `{data_dir}/agent-identities.json` (same family as
//! `composer-defaults.json`): reopening the app shows the same name and face
//! for every recorded subagent. Call-keyed draws never persist — a spawn
//! that dies before its doc exists leaves no record.
//!
//! The avatar is the chip's 22px leading face: a full-bleed squircle in the
//! identity color with two white pill eyes. Running subagents blink and
//! glance (phases ride the shared [`motion::pulse_delta`] clock, offset per
//! agent so siblings desync); done settles into happy-arch eyes; failed
//! freezes mid-look. Reduced motion pins every face static.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

const FILE_NAME: &str = "agent-identities.json";

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
/// side, drawn with the identity so siblings desync).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentIdentity {
    pub name: SharedString,
    pub color: Hsla,
    /// Blink/glance phase offset [0,1): siblings share the global clock, so
    /// this is what keeps a fan-out from blinking in unison.
    pub phase: f32,
    /// Glance side: -1.0 (left) or 1.0 (right).
    pub gaze_dir: f32,
}

/// The persisted record for one subagent doc.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredIdentity {
    name: String,
    /// "#rrggbb" — round-trips exactly, unlike float hsl conversions.
    color: String,
    phase: f32,
    gaze_dir: f32,
}

/// The on-disk shape: one map, doc id → identity.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct IdentityFile {
    by_doc: HashMap<String, StoredIdentity>,
}

fn hex_of(color: Hsla) -> String {
    let rgb = color.to_rgb();
    let byte = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u32;
    format!("#{:02x}{:02x}{:02x}", byte(rgb.r), byte(rgb.g), byte(rgb.b))
}

fn hsla_of(hex: &str) -> Option<Hsla> {
    let digits = hex.strip_prefix('#')?;
    if digits.len() != 6 {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    Some(Hsla::from(gpui::rgb(value)))
}

impl AgentIdentity {
    fn to_stored(&self) -> StoredIdentity {
        StoredIdentity {
            name: self.name.to_string(),
            color: hex_of(self.color),
            phase: self.phase,
            gaze_dir: self.gaze_dir,
        }
    }

    fn from_stored(stored: &StoredIdentity) -> Option<Self> {
        Some(Self {
            name: SharedString::from(stored.name.clone()),
            color: hsla_of(&stored.color)?,
            phase: stored.phase,
            gaze_dir: stored.gaze_dir,
        })
    }
}

/// One transcript's identity registry. `by_doc` is the persisted half (the
/// file loads into it and every insert saves); `by_call` is the transient
/// pre-spawn half, keyed by tool call id and adopted into `by_doc` when the
/// engine stamps the ref onto the same part.
pub(crate) struct AgentIdentities {
    rng: u64,
    names: Vec<&'static str>,
    colors: Vec<Hsla>,
    /// The full color deck, kept to refill `colors` from (the pool is hex
    /// consts; the deck deals the converted Hslas).
    color_pool: Vec<Hsla>,
    by_doc: HashMap<String, AgentIdentity>,
    by_call: HashMap<String, AgentIdentity>,
    data_dir: Option<PathBuf>,
}

impl AgentIdentities {
    pub fn new(seed: u64, data_dir: Option<PathBuf>) -> Self {
        let color_pool: Vec<Hsla> = COLORS
            .iter()
            .map(|&hex| Hsla::from(gpui::rgb(hex)))
            .collect();
        let by_doc = data_dir.as_deref().map(load).unwrap_or_default();
        let mut this = Self {
            rng: seed | 1,
            names: NAMES.to_vec(),
            colors: color_pool.clone(),
            color_pool,
            by_doc,
            by_call: HashMap::new(),
            data_dir,
        };
        shuffle(&mut this.names, &mut this.rng);
        shuffle(&mut this.colors, &mut this.rng);
        this
    }

    /// The identity for one agent chip. `doc_id` (the subagent's doc) beats
    /// `call_id` (the spawn tool call) — the doc is the persisted key, so a
    /// reloaded chat keeps its names; a live spawn wears its call-keyed draw
    /// until the ref lands, then adopts the doc key without changing face.
    pub fn identity_for(
        &mut self,
        call_id: Option<&str>,
        doc_id: Option<&str>,
    ) -> Option<AgentIdentity> {
        if let Some(doc) = doc_id
            && let Some(identity) = self.by_doc.get(doc)
        {
            return Some(identity.clone());
        }
        if let Some(call) = call_id
            && let Some(identity) = self.by_call.get(call).cloned()
        {
            if let Some(doc) = doc_id {
                self.by_doc.insert(doc.to_owned(), identity.clone());
                self.save();
            }
            return Some(identity);
        }
        let identity = AgentIdentity {
            name: SharedString::from(draw(&mut self.names, NAMES, &mut self.rng)),
            color: {
                let color_pool = self.color_pool.clone();
                draw(&mut self.colors, &color_pool, &mut self.rng)
            },
            phase: (next_u64(&mut self.rng) % 1000) as f32 / 1000.0,
            gaze_dir: if next_u64(&mut self.rng) & 1 == 0 {
                -1.0
            } else {
                1.0
            },
        };
        if let Some(doc) = doc_id {
            self.by_doc.insert(doc.to_owned(), identity.clone());
            self.save();
        } else {
            self.by_call.insert(call_id?.to_owned(), identity.clone());
        }
        Some(identity)
    }

    /// Persist the doc-keyed half (atomic temp + rename, like
    /// `composer-defaults.json`). Garnish must never break a render: a write
    /// failure is a warning, not an error.
    fn save(&self) {
        let Some(dir) = self.data_dir.as_deref() else {
            return;
        };
        let file = IdentityFile {
            by_doc: self
                .by_doc
                .iter()
                .map(|(doc, identity)| (doc.clone(), identity.to_stored()))
                .collect(),
        };
        if let Err(err) = save(dir, &file) {
            tracing::warn!(error = %err, "agent-identities save failed");
        }
    }
}

fn load(data_dir: &Path) -> HashMap<String, AgentIdentity> {
    let path = data_dir.join(FILE_NAME);
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<IdentityFile>(&text) {
            Ok(file) => file
                .by_doc
                .iter()
                .filter_map(|(doc, stored)| {
                    AgentIdentity::from_stored(stored).map(|identity| (doc.clone(), identity))
                })
                .collect(),
            Err(err) => {
                tracing::warn!(error = %err, "agent-identities corrupt; starting fresh");
                HashMap::new()
            }
        },
        Err(_) => HashMap::new(),
    }
}

fn save(data_dir: &Path, file: &IdentityFile) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join(FILE_NAME);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
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
    identity: &AgentIdentity,
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

    fn identities(seed: u64) -> AgentIdentities {
        AgentIdentities::new(seed, None)
    }

    #[test]
    fn draws_do_not_repeat_within_a_pool_cycle() {
        // One full name cycle from a fresh pool: no repeats.
        let mut registry = identities(42);
        let names: Vec<_> = (0..NAMES.len())
            .map(|i| {
                registry
                    .identity_for(None, Some(&format!("doc-{i}")))
                    .unwrap()
                    .name
                    .to_string()
            })
            .collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), NAMES.len(), "one full cycle, no repeats");
        // Same for colors (an identity draw takes one of each, so the color
        // cycle needs its own fresh pool to line up with the draws).
        let mut registry = identities(42);
        let colors: Vec<_> = (0..COLORS.len())
            .map(|i| {
                registry
                    .identity_for(None, Some(&format!("color-{i}")))
                    .unwrap()
                    .color
            })
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
    fn identity_is_stable_per_key() {
        let mut registry = identities(7);
        let first = registry.identity_for(None, Some("doc-a")).unwrap();
        assert_eq!(registry.identity_for(None, Some("doc-a")), Some(first));
    }

    #[test]
    fn refill_avoids_immediate_repeats() {
        let mut registry = identities(99);
        let names: Vec<_> = (0..NAMES.len() * 3)
            .map(|i| {
                registry
                    .identity_for(None, Some(&format!("doc-{i}")))
                    .unwrap()
                    .name
                    .to_string()
            })
            .collect();
        for pair in names.windows(2) {
            assert_ne!(pair[0], pair[1], "adjacent draws never repeat");
        }
    }

    #[test]
    fn doc_identities_survive_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let first = {
            let mut registry = AgentIdentities::new(1, Some(dir.path().to_path_buf()));
            registry.identity_for(None, Some("chat--sub--1")).unwrap()
        };
        let mut reloaded = AgentIdentities::new(2, Some(dir.path().to_path_buf()));
        assert_eq!(
            reloaded.identity_for(None, Some("chat--sub--1")),
            Some(first),
            "a doc-keyed identity restores across a restart"
        );
    }

    #[test]
    fn call_only_draws_never_persist() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = AgentIdentities::new(1, Some(dir.path().to_path_buf()));
        registry.identity_for(Some("call-1"), None).unwrap();
        assert!(registry.by_doc.is_empty());
        assert!(!dir.path().join(FILE_NAME).exists(), "no doc id, no file");
    }

    #[test]
    fn the_ref_landing_adopts_the_call_keyed_identity() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = AgentIdentities::new(1, Some(dir.path().to_path_buf()));
        let transient = registry.identity_for(Some("call-1"), None).unwrap();
        let adopted = registry
            .identity_for(Some("call-1"), Some("chat--sub--9"))
            .unwrap();
        assert_eq!(adopted, transient, "no mid-view name swap");
        let mut reloaded = AgentIdentities::new(2, Some(dir.path().to_path_buf()));
        assert_eq!(
            reloaded.identity_for(None, Some("chat--sub--9")),
            Some(transient)
        );
    }

    #[test]
    fn color_hex_round_trips_the_pool() {
        for &hex in COLORS {
            let color = Hsla::from(gpui::rgb(hex));
            assert_eq!(hex_of(color), format!("#{hex:06x}"));
        }
        assert_eq!(hsla_of("garbage"), None);
        assert_eq!(hsla_of("#12345"), None);
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
