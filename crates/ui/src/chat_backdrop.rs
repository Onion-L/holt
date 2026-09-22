//! The chat column's optional backdrop: a local image washed under the
//! conversation (settings → Appearance → Chat backdrop).
//!
//! The picture reads at the top of the pane and sinks into the theme surface
//! toward the bottom, so the transcript's glass bubbles and text always land
//! on a known tone. Two recipes carry that:
//!
//! - a two-half vertical wash (clear-ish at the top, near-opaque surface at
//!   the bottom — [`gpui::linear_gradient`] takes exactly two stops, so the
//!   mid stop splits the column into stacked halves, the shell canvas's
//!   trick);
//! - a luminance-adaptive multiplier: the decode measures the image's mean
//!   luma, and the further it sits from the surface tone, the heavier every
//!   stop runs ([`wash`]). A white screenshot buries itself under the dark
//!   theme; a moody photo keeps more of its presence. Same contract as the
//!   theme's contrast-checked tints — effect, never broken text.
//!
//! Dark mode also pulls the decode's saturation down slightly: the accent
//! stays the hero, the picture only sets a mood. Light keeps the photo
//! honest — its wash ends in the surface tone, not flat white.
//!
//! Load pipeline: path → bytes → capped decode ([`MAX_EDGE`]) → treat → BGRA
//! [`RenderImage`], cached per (path, appearance) in a small session cache —
//! the pane re-renders constantly and must never re-decode. A cache miss
//! renders nothing and kicks one background task; completion refreshes the
//! windows and the picture fades in, keyed by the config so a swap re-fades.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use gpui::{
    AnyElement, App, Div, Hsla, ObjectFit, RenderImage, SharedString, div, img, prelude::*,
};

use crate::theme::Theme;

/// Decode ceiling: the GPU cover-scales from here; larger sources only cost
/// memory.
const MAX_EDGE: u32 = 2048;
/// Allocation ceiling for a hostile header, mirroring `images.rs`.
const DECODE_MAX_ALLOC_BYTES: u64 = 256 * 1024 * 1024;
/// Session cache cap. One pane shows one picture; slack covers appearance
/// switches and edits without ever re-decoding on a repaint.
const CACHE_CAP: usize = 4;
/// Dark mode blends saturation toward luma by this much at load.
const DARK_DESATURATE: f32 = 0.25;
/// Thumbnail edge the mean luma is measured on.
const LUMA_SAMPLE_EDGE: u32 = 64;

/// A decoded, appearance-treated backdrop plus its measured mean luma.
pub struct LoadedImage {
    pub image: Arc<RenderImage>,
    /// Mean Rec.709 luma in 0..1, sampled at load time.
    pub luminance: f32,
}

#[derive(Clone, PartialEq, Eq)]
struct Key {
    path: String,
    dark: bool,
}

#[derive(Default)]
struct BackdropCache {
    /// Insertion order doubles as eviction order (cap [`CACHE_CAP`]).
    entries: Vec<(Key, Arc<LoadedImage>)>,
    pending: Vec<Key>,
    failed: Vec<Key>,
}

fn cache() -> &'static Mutex<BackdropCache> {
    static CACHE: OnceLock<Mutex<BackdropCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BackdropCache::default()))
}

/// The cached backdrop for `path` under `dark`, if decoded this session.
/// Lookup refreshes recency.
pub fn cached(path: &str, dark: bool) -> Option<Arc<LoadedImage>> {
    let mut cache = cache().lock().unwrap();
    let ix = cache
        .entries
        .iter()
        .position(|(key, _)| key.path == path && key.dark == dark)?;
    let entry = cache.entries.remove(ix);
    cache.entries.push(entry.clone());
    Some(entry.1)
}

/// Extensions the native picker offers and the drop targets accept. The
/// decoder guesses by content; this list is only the UX filter.
pub const SUPPORTED_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif"];

/// Does `path` name a file the picker/drop targets should accept (by
/// extension, case-insensitive)? The decoder itself guesses by content.
pub fn is_supported_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            SUPPORTED_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

/// Has a decode for `path` under `dark` already failed this session? The
/// settings page surfaces this — a file that exists but will never paint.
pub fn failed(path: &str, dark: bool) -> bool {
    let cache = cache().lock().unwrap();
    cache
        .failed
        .iter()
        .any(|key| key.path == path && key.dark == dark)
}

/// Kick one background decode for `path` if none is running. Completion
/// refreshes the windows, so callers that render from [`cached`] repaint into
/// the picture without owning any state.
pub fn preload(path: &str, dark: bool, cx: &mut App) {
    let key = Key {
        path: path.to_owned(),
        dark,
    };
    {
        let mut cache = cache().lock().unwrap();
        if cache.pending.contains(&key)
            || cache.failed.contains(&key)
            || cache.entries.iter().any(|(known, _)| *known == key)
        {
            return;
        }
        cache.pending.push(key.clone());
    }
    let expanded = expand_path(path);
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move { load_file(expanded, dark) })
            .await;
        cx.update(|cx| {
            let mut cache = cache().lock().unwrap();
            cache.pending.retain(|known| *known != key);
            match result {
                Ok(loaded) => {
                    cache.failed.retain(|known| *known != key);
                    cache.entries.push((key, Arc::new(loaded)));
                    let excess = cache.entries.len().saturating_sub(CACHE_CAP);
                    let evicted: Vec<_> = cache.entries.drain(..excess).collect();
                    drop(cache);
                    // Dropping the Arc alone strands gpui's decoded sprite
                    // tiles; release them like `images.rs::flush_evicted`.
                    for (_, loaded) in evicted {
                        cx.drop_image(loaded.image.clone(), None);
                    }
                    cx.refresh_windows();
                }
                Err(error) => {
                    tracing::warn!(path = %key.path, %error, "chat backdrop failed to load");
                    cache.failed.push(key);
                    drop(cache);
                    // A watching surface (the settings preview) repaints
                    // into the failure hint instead of a silent blank.
                    cx.refresh_windows();
                }
            }
        });
    })
    .detach();
}

/// `~`-expanded path, so the settings field accepts `~/...` home-relative
/// forms (same convenience as the file viewers).
pub fn expand_path(path: &str) -> PathBuf {
    crate::path_refs::expand_home(path.trim())
}

/// The backdrop element for the chat column, or `None` when unset or still
/// loading (a miss starts the load). Painted as the column's first child, so
/// transcript, glass chrome, and composer all composite above it.
pub fn element(theme: &Theme, cx: &mut App) -> Option<AnyElement> {
    let (path, presence) = crate::settings::chat_backdrop(cx);
    let path = path.as_deref()?.trim();
    if path.is_empty() {
        return None;
    }
    let dark = !theme.appearance.is_light();
    let Some(loaded) = cached(path, dark) else {
        preload(path, dark, cx);
        return None;
    };
    let wash = wash(presence, loaded.luminance, theme.surface.l);
    Some(
        // Opacity-only fade: `fade_in`'s rise sets `relative` + `top` every
        // frame, which would stomp this layer's absolute positioning and
        // collapse it to zero height (the picture never painted).
        crate::motion::fade_quick(
            fade_id(path, dark),
            paint_layers(loaded.image.clone(), theme.surface, wash),
        )
        .into_any_element(),
    )
}

/// The picture plus wash, absolutely positioned — shared by the live pane and
/// the settings preview card. Paint into any sized relative parent. Only the
/// vertical sink is painted: the side edges stay honest (user direction —
/// feathers read as grime on both light and dark surfaces).
pub(crate) fn paint_layers(image: Arc<RenderImage>, surface: Hsla, wash: Wash) -> Div {
    let stop = |alpha: f32, at: f32| gpui::linear_color_stop(surface.opacity(alpha), at);
    div()
        .absolute()
        .inset_0()
        .child(img(image).w_full().h_full().object_fit(ObjectFit::Cover))
        .child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .child(div().flex_1().bg(gpui::linear_gradient(
                    180.0,
                    stop(wash.top, 0.0),
                    stop(wash.mid, 1.0),
                )))
                .child(div().flex_1().bg(gpui::linear_gradient(
                    180.0,
                    stop(wash.mid, 0.0),
                    stop(wash.bottom, 1.0),
                ))),
        )
}

/// Wash alphas at the top / mid / bottom of the column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Wash {
    pub top: f32,
    pub mid: f32,
    pub bottom: f32,
}

/// The wash recipe. `presence` (the settings slider) sets how much of the
/// picture survives at the top; the bottom always sinks deep — the composer
/// and status strip need a real shadow to sit on, so the sink is NOT
/// presence's business. `adapt` runs every stop heavier the further the
/// image luma sits from the surface luma, so the composite never strands
/// text on an adversarial tone.
pub(crate) fn wash(presence: f32, image_luminance: f32, surface_luminance: f32) -> Wash {
    let presence = presence.clamp(0.05, 0.95);
    let adapt = (1.0 + (image_luminance - surface_luminance).abs() * 0.9).min(1.8);
    let top = ((1.0 - presence) * 0.35 * adapt).min(0.6);
    let bottom = (0.88 * adapt).min(0.97).max(top + 0.3);
    // The ramp concentrates in the bottom half: the mid stop stays closer to
    // the top value, so the shadow reads as a distinct zone, not a haze.
    let mid = top + (bottom - top) * 0.38;
    Wash { top, mid, bottom }
}

fn fade_id(path: &str, dark: bool) -> SharedString {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    dark.hash(&mut hasher);
    SharedString::from(format!("chat-backdrop-{:016x}", hasher.finish()))
}

fn load_file(path: PathBuf, dark: bool) -> Result<LoadedImage, String> {
    let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("image format could not be determined: {error}"))?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(DECODE_MAX_ALLOC_BYTES);
    reader.limits(limits);
    let mut rgba = reader
        .decode()
        .map_err(|error| format!("decode failed: {error}"))?
        .to_rgba8();
    let longest = rgba.width().max(rgba.height());
    if longest > MAX_EDGE {
        let scale = MAX_EDGE as f32 / longest as f32;
        let width = ((rgba.width() as f32 * scale).max(1.0)) as u32;
        let height = ((rgba.height() as f32 * scale).max(1.0)) as u32;
        rgba = image::imageops::thumbnail(&rgba, width, height);
    }
    if dark {
        desaturate(&mut rgba, DARK_DESATURATE);
    }
    let luminance = mean_luminance(&rgba);
    Ok(LoadedImage {
        image: to_render(rgba),
        luminance,
    })
}

/// Blend each channel toward Rec.709 luma by `blend` (0..1).
fn desaturate(rgba: &mut image::RgbaImage, blend: f32) {
    for pixel in rgba.pixels_mut() {
        let [r, g, b, _] = pixel.0;
        let luma = 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b);
        let mix = |channel: u8| {
            (f32::from(channel) * (1.0 - blend) + luma * blend)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        pixel.0[0] = mix(r);
        pixel.0[1] = mix(g);
        pixel.0[2] = mix(b);
    }
}

fn mean_luminance(rgba: &image::RgbaImage) -> f32 {
    let thumb = image::imageops::thumbnail(rgba, LUMA_SAMPLE_EDGE, LUMA_SAMPLE_EDGE);
    let mut total = 0.0;
    for pixel in thumb.pixels() {
        let [r, g, b, _] = pixel.0;
        total += (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0;
    }
    total / (thumb.width() * thumb.height()).max(1) as f32
}

/// RGBA8 → gpui BGRA frame (same swap as `images.rs`).
fn to_render(mut rgba: image::RgbaImage) -> Arc<RenderImage> {
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(
        rgba
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wash_is_monotonic_and_bounded() {
        for presence in [0.05, 0.25, 0.5, 0.75, 0.95] {
            for image_luma in [0.05, 0.3, 0.5, 0.8] {
                for surface_luma in [0.1, 0.55, 0.9] {
                    let wash = wash(presence, image_luma, surface_luma);
                    assert!(wash.top >= 0.0, "{wash:?}");
                    assert!(wash.top <= 0.6, "{wash:?}");
                    assert!(wash.bottom <= 0.97, "{wash:?}");
                    assert!(wash.top <= wash.mid && wash.mid <= wash.bottom, "{wash:?}");
                }
            }
        }
    }

    #[test]
    fn wash_presence_buys_top_visibility_but_the_bottom_always_sinks() {
        let timid = wash(0.25, 0.5, 0.5);
        let bold = wash(0.9, 0.5, 0.5);
        assert!(bold.top < timid.top);
        // Presence is the top's knob only — the composer shadow stays deep.
        assert!(timid.bottom >= 0.85, "{:?}", timid);
        assert!(bold.bottom >= 0.85, "{:?}", bold);
    }

    #[test]
    fn wash_buries_images_far_from_the_surface_tone() {
        let near = wash(0.5, 0.2, 0.15);
        let far = wash(0.5, 0.95, 0.15);
        assert!(far.top > near.top);
        assert!(far.bottom > near.bottom);
    }

    #[test]
    fn mean_luminance_reads_a_uniform_field() {
        let image = image::RgbaImage::from_pixel(32, 32, image::Rgba([128, 128, 128, 255]));
        let luma = mean_luminance(&image);
        let expected = (0.2126 + 0.7152 + 0.0722) * 128.0 / 255.0;
        assert!((luma - expected).abs() < 0.01, "{luma}");
    }

    #[test]
    fn dark_desaturation_pulls_channels_toward_luma() {
        let mut image = image::RgbaImage::from_pixel(4, 4, image::Rgba([220, 30, 30, 255]));
        let chroma_before = {
            let p = image.get_pixel(0, 0).0;
            i32::from(p[0]) - i32::from(p[2])
        };
        desaturate(&mut image, DARK_DESATURATE);
        let p = image.get_pixel(0, 0).0;
        let chroma_after = i32::from(p[0]) - i32::from(p[2]);
        assert!(chroma_after < chroma_before);
        assert!(chroma_after > 0, "a 25% blend must not gray out fully");
    }

    #[test]
    fn supported_files_match_the_picker_extensions_case_insensitively() {
        assert!(is_supported_file(Path::new("/tmp/wall.png")));
        assert!(is_supported_file(Path::new("/tmp/wall.JPEG")));
        assert!(is_supported_file(Path::new("wall.webp")));
        assert!(!is_supported_file(Path::new("/tmp/notes.txt")));
        assert!(!is_supported_file(Path::new("/tmp/no_extension")));
    }

    #[test]
    fn failed_marks_a_path_per_appearance() {
        let path = "definitely-unique-failed-backdrop-test-path.png";
        cache().lock().unwrap().failed.push(Key {
            path: path.into(),
            dark: true,
        });
        assert!(failed(path, true));
        assert!(!failed(path, false));
        assert!(!failed("definitely-unique-other-path.png", true));
        cache()
            .lock()
            .unwrap()
            .failed
            .retain(|key| key.path != path);
    }

    #[test]
    fn expand_path_maps_home_forms_and_keeps_the_rest() {
        assert_eq!(
            expand_path(" ~/pics/wall.png "),
            expand_path("~/pics/wall.png")
        );
        if let Some(home) = std::env::home_dir() {
            assert_eq!(expand_path("~/wall.png"), home.join("wall.png"));
            assert_eq!(expand_path("~"), home);
        }
        assert_eq!(expand_path("/tmp/wall.png"), PathBuf::from("/tmp/wall.png"));
    }

    #[test]
    fn backdrop_settings_roundtrip_and_clamp() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = crate::settings::UiSettings::default();
        settings.chat_backdrop_path = Some("~/wall.png".into());
        settings.chat_backdrop_presence = 9.0;
        settings.save(dir.path()).unwrap();

        let loaded = crate::settings::UiSettings::load(dir.path());
        assert_eq!(loaded.chat_backdrop_path.as_deref(), Some("~/wall.png"));
        assert!((loaded.chat_backdrop_presence - 0.95).abs() < 1e-4);

        // A file written before the feature existed loads with the default.
        let mut legacy = crate::settings::UiSettings::default();
        legacy.chat_backdrop_path = None;
        legacy.chat_backdrop_presence = 0.5;
        legacy.save(dir.path()).unwrap();
        let loaded = crate::settings::UiSettings::load(dir.path());
        assert_eq!(loaded.chat_backdrop_path, None);
        assert!((loaded.chat_backdrop_presence - 0.5).abs() < 1e-4);
    }
}
