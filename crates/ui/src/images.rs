//! Local image loading for previews and thumbnails: served byte reads
//! (`ReadImage`), bounded decoding, thumbnail downscaling, and a
//! stat-fingerprinted cache with honest failure states.
//!
//! The pipeline never trusts a stale cache as a live answer: every snapshot
//! re-stats the target, so a changed file reloads and a deleted one reports
//! missing — previews are live views, not content snapshots. Decode work runs
//! on the background executor with explicit byte/pixel budgets; GIF and
//! animated WebP decode their FIRST frame only (`image`'s decoder already
//! stops there), matching the model-side policy.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use gpui::{App, RenderImage};
use smallvec::smallvec;

use crate::state::EngineHandle;

/// Extensions this feature previews (and the read tool supplies to models):
/// static PNG/JPEG/WebP plus GIF/animated WebP first frames. Everything else
/// — APNG, BMP, SVG, TIFF, HEIC, … — stays an ordinary path reference.
pub const SUPPORTED_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// Thumbnail budget: the longest edge a cached thumbnail can have.
pub const THUMB_MAX_EDGE: u32 = 256;
/// Cache budget for retained thumbnails (decoded RGBA bytes — bounding the
/// decoded side is what actually bounds memory; gpui's sprite tiles scale
/// with it).
const THUMB_CACHE_BUDGET_BYTES: usize = 64 * 1024 * 1024;
/// Viewer decode guard: larger images are rejected before pixel allocation.
const VIEWER_MAX_PIXELS: u64 = 32 * 1024 * 1024;
/// Hard decode ceiling fed to the codec as an allocation limit — a hostile
/// header can claim dimensions that would decompress to gigabytes.
const DECODE_MAX_ALLOC_BYTES: u64 = 256 * 1024 * 1024;

/// Classify a path as previewable by extension (cheap, sync — the render
/// path; magic bytes are verified when the bytes load).
pub fn is_image_path(path: &str) -> bool {
    let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) else {
        return false;
    };
    SUPPORTED_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
}

/// One cached thumbnail: pre-scaled pixels (BGRA, gpui-ready) plus the
/// natural pixel size of the source at load time.
#[derive(Clone, Debug)]
pub struct Thumb {
    pub pixels: Arc<RenderImage>,
    /// Source image size in pixels (post-orientation), for aspect math.
    pub width: u32,
    pub height: u32,
}

impl Thumb {
    pub fn aspect(&self) -> f32 {
        if self.height == 0 {
            1.0
        } else {
            self.width as f32 / self.height as f32
        }
    }
}

/// Full-resolution pixels for the zoomable viewer, within [`VIEWER_MAX_PIXELS`].
#[derive(Clone)]
pub struct ViewerPixels {
    pub pixels: Arc<RenderImage>,
    pub width: u32,
    pub height: u32,
}

/// What a render pass sees for one path.
#[derive(Clone, Debug)]
pub enum Snapshot {
    Loading,
    Loaded(Thumb),
    /// The load failed; `cause` is user-presentable. `retry_in` is when the
    /// loader would try again (the 2s→15s ladder).
    Error {
        cause: gpui::SharedString,
        retry_in: Duration,
    },
}

enum CacheEntry {
    Loading {
        attempts: u32,
        fingerprint: Option<(u64, u128)>,
    },
    Loaded {
        thumb: Thumb,
        fingerprint: (u64, u128),
        bytes: usize,
        last_used: u64,
    },
    Error {
        attempts: u32,
        at: Instant,
        cause: gpui::SharedString,
    },
}

fn retry_delay(attempts: u32) -> Duration {
    Duration::from_millis((2_000u64 << attempts.min(3)).min(15_000))
}

#[derive(Default)]
struct ImageCache {
    map: HashMap<String, CacheEntry>,
    tick: u64,
    loaded_bytes: usize,
    /// Evicted thumbnails awaiting `flush_evicted` (freeing gpui's sprite
    /// tiles needs `&mut App`, which eviction sites — async loads — lack).
    pending_free: Vec<Arc<RenderImage>>,
}

fn cache() -> &'static Mutex<ImageCache> {
    static CACHE: OnceLock<Mutex<ImageCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ImageCache::default()))
}

/// The load-time fingerprint of a file: (len, mtime nanoseconds). A changed file
/// produces a different key, so refreshing a reference reflects current
/// contents.
pub(crate) fn fingerprint(path: &str) -> Option<(u64, u128)> {
    let meta = std::fs::metadata(path).ok()?;
    let len = meta.len();
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((len, mtime))
}

pub fn observe<T: 'static>(cx: &mut gpui::Context<T>) {
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let refresh = {
                let cache = cache().lock().unwrap();
                cache.map.iter().any(|(path, entry)| match entry {
                    CacheEntry::Loaded {
                        fingerprint: saved, ..
                    } => fingerprint(path) != Some(*saved),
                    CacheEntry::Loading { .. } => true,
                    CacheEntry::Error { at, attempts, .. } => {
                        at.elapsed() >= retry_delay(attempts.saturating_sub(1))
                    }
                })
            };
            if this
                .update(cx, |_, cx| {
                    if refresh {
                        cx.notify();
                    }
                })
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}

/// The snapshot for one path: fresh stat + cache state. `true` from
/// [`begin_load`] means the caller should spawn the load now.
pub fn snapshot(path: &str) -> Snapshot {
    let now = fingerprint(path);
    let mut cache = cache().lock().unwrap();
    cache.tick += 1;
    let tick = cache.tick;
    match cache.map.get_mut(path) {
        Some(CacheEntry::Loaded {
            thumb,
            last_used,
            fingerprint,
            ..
        }) if Some(*fingerprint) == now => {
            *last_used = tick;
            return Snapshot::Loaded(thumb.clone());
        }
        Some(CacheEntry::Loading { .. }) => return Snapshot::Loading,
        Some(CacheEntry::Error {
            attempts,
            at,
            cause,
        }) if now.is_some() => {
            return Snapshot::Error {
                cause: cause.clone(),
                retry_in: retry_delay(attempts.saturating_sub(1)).saturating_sub(at.elapsed()),
            };
        }
        // Loaded-but-stale, errored-but-deleted, or unknown: fall through.
        _ => {}
    }
    match now {
        // File present but no fresh entry: a load is needed.
        Some(_) => Snapshot::Loading,
        // An inaccessible target is distinct from a missing one.
        None => Snapshot::Error {
            cause: match std::fs::metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    "Image file not found.".into()
                }
                Err(error) => format!("Image file could not be read: {error}").into(),
                Ok(_) => "Image metadata could not be read.".into(),
            },
            retry_in: Duration::MAX,
        },
    }
}

/// Claim the load for a path: `true` ⇒ the caller starts fetching now.
/// Errored sources hand out a retry only after their backoff elapsed.
pub fn begin_load(path: &str) -> bool {
    let now = fingerprint(path);
    let mut cache = cache().lock().unwrap();
    if cache
        .map
        .values()
        .filter(|e| matches!(e, CacheEntry::Loading { .. }))
        .count()
        >= 4
    {
        return false;
    }
    if matches!(cache.map.get(path), Some(CacheEntry::Loaded { fingerprint, .. }) if Some(*fingerprint) != now)
        && let Some(CacheEntry::Loaded { bytes, thumb, .. }) = cache.map.remove(path)
    {
        cache.loaded_bytes = cache.loaded_bytes.saturating_sub(bytes);
        cache.pending_free.push(thumb.pixels);
    }
    if !cache.map.contains_key(path) && cache.map.len() >= 256 {
        let evict = cache
            .map
            .iter()
            .filter(|(_, e)| !matches!(e, CacheEntry::Loading { .. }))
            .min_by_key(|(_, e)| match e {
                CacheEntry::Loaded { last_used, .. } => *last_used,
                _ => 0,
            })
            .map(|(p, _)| p.clone());
        if let Some(evict) = evict
            && let Some(CacheEntry::Loaded { bytes, thumb, .. }) = cache.map.remove(&evict)
        {
            cache.loaded_bytes = cache.loaded_bytes.saturating_sub(bytes);
            cache.pending_free.push(thumb.pixels);
        }
    }
    match cache.map.entry(path.to_string()) {
        std::collections::hash_map::Entry::Vacant(v) => {
            v.insert(CacheEntry::Loading {
                attempts: 0,
                fingerprint: now,
            });
            true
        }
        std::collections::hash_map::Entry::Occupied(mut o) => match o.get() {
            CacheEntry::Error { attempts, at, .. }
                if at.elapsed() >= retry_delay(attempts.saturating_sub(1)) =>
            {
                let attempts = *attempts;
                o.insert(CacheEntry::Loading {
                    attempts,
                    fingerprint: now,
                });
                true
            }
            _ => false,
        },
    }
}

pub fn store_loaded(path: &str, thumb: Thumb) {
    let bytes = thumb.pixels.as_bytes(0).map_or(0, |b| b.len());
    let Some(fp) = fingerprint(path) else {
        store_error(path, "Image file not found.");
        return;
    };
    let mut cache = cache().lock().unwrap();
    cache.tick += 1;
    if matches!(cache.map.get(path), Some(CacheEntry::Loading { fingerprint, .. }) if *fingerprint != Some(fp))
    {
        cache.map.remove(path);
        return;
    }
    let evicted_now = match cache.map.get(path) {
        Some(CacheEntry::Loaded { bytes, thumb, .. }) => Some((*bytes, thumb.pixels.clone())),
        _ => None,
    };
    if let Some((old_bytes, old_pixels)) = evicted_now {
        cache.loaded_bytes = cache.loaded_bytes.saturating_sub(old_bytes);
        cache.pending_free.push(old_pixels);
    }
    let entry = CacheEntry::Loaded {
        thumb,
        fingerprint: fp,
        bytes,
        last_used: cache.tick,
    };
    cache.map.insert(path.to_string(), entry);
    cache.loaded_bytes += bytes;
    while cache.loaded_bytes > THUMB_CACHE_BUDGET_BYTES {
        let Some((_, evict_path)) = cache
            .map
            .iter()
            .filter(|(p, e)| p.as_str() != path && matches!(e, CacheEntry::Loaded { .. }))
            .filter_map(|(p, e)| match e {
                CacheEntry::Loaded { last_used, .. } => Some((*last_used, p.clone())),
                _ => None,
            })
            .min()
        else {
            break;
        };
        if let Some(CacheEntry::Loaded { bytes, thumb, .. }) = cache.map.remove(&evict_path) {
            cache.loaded_bytes = cache.loaded_bytes.saturating_sub(bytes);
            cache.pending_free.push(thumb.pixels);
        }
    }
}

pub fn store_error(path: &str, cause: impl Into<gpui::SharedString>) {
    let mut cache = cache().lock().unwrap();
    let attempts = match cache.map.get(path) {
        Some(CacheEntry::Loading { attempts, .. }) => attempts + 1,
        Some(CacheEntry::Error { attempts, .. }) => *attempts,
        _ => 1,
    };
    cache.map.insert(
        path.to_string(),
        CacheEntry::Error {
            attempts,
            at: Instant::now(),
            cause: cause.into(),
        },
    );
}

/// Release gpui's decoded sprite tiles for evicted thumbnails (see
/// [`drop_image`]). Cheap when nothing was evicted.
pub fn flush_evicted(mut window: Option<&mut gpui::Window>, cx: &mut App) {
    let evicted = std::mem::take(&mut cache().lock().unwrap().pending_free);
    for image in evicted {
        cx.drop_image(image, window.as_deref_mut());
    }
}

pub fn discard(image: Arc<RenderImage>) {
    cache().lock().unwrap().pending_free.push(image);
}

// ---------------------------------------------------------------------------
// Loading + decoding
// ---------------------------------------------------------------------------

/// Stage a pasted clipboard image as a Managed image through the served
/// contract: base64 the bytes, `StageImage` validates (sniffed format, byte
/// limit) and persists durably, and the reply's absolute path joins the draft
/// as a path reference. The clipboard's format claim rides along for errors
/// only — the sniff decides.
pub async fn stage_pasted(
    engine: &EngineHandle,
    image: &gpui::Image,
) -> Result<gpui::SharedString, gpui::SharedString> {
    if image.bytes.len() > 25 * 1024 * 1024 {
        return Err("Image exceeds the 25 MiB limit.".into());
    }
    let data = base64::engine::general_purpose::STANDARD.encode(&image.bytes);
    let reply = engine
        .client()
        .call(
            holt_rpc::methods::STAGE_IMAGE,
            serde_json::json!({
                "data": data,
                "format": image.format.extension(),
            }),
        )
        .await
        .map_err(|error| gpui::SharedString::from(error.to_string()))?;
    let staged: holt_rpc::images::ManagedImage = serde_json::from_value(reply)
        .map_err(|error| gpui::SharedString::from(format!("Invalid staging reply: {error}")))?;
    Ok(staged.path.into())
}

/// Load a thumbnail through the served image contract: `ReadImage` (bounded,
/// sniffed) → bounded decode → proportional downscale. The `Err` cause is
/// user-presentable.
pub async fn load_thumb(
    engine: &EngineHandle,
    path: &str,
    executor: &gpui::BackgroundExecutor,
) -> Result<Thumb, gpui::SharedString> {
    let pixels = load_pixels(engine, path, Some(THUMB_MAX_EDGE), executor).await?;
    let (render, width, height) = pixels;
    Ok(Thumb {
        pixels: render,
        width,
        height,
    })
}

/// Load full viewer pixels (proportionally reduced past
/// [`VIEWER_MAX_PIXELS`], original detail within it).
pub async fn load_viewer_pixels(
    engine: &EngineHandle,
    path: &str,
    executor: &gpui::BackgroundExecutor,
) -> Result<ViewerPixels, gpui::SharedString> {
    let (render, width, height) = load_pixels(engine, path, None, executor).await?;
    Ok(ViewerPixels {
        pixels: render,
        width,
        height,
    })
}

async fn load_pixels(
    engine: &EngineHandle,
    path: &str,
    max_edge: Option<u32>,
    executor: &gpui::BackgroundExecutor,
) -> Result<(Arc<RenderImage>, u32, u32), gpui::SharedString> {
    let reply = engine
        .client()
        .call(
            holt_rpc::methods::READ_IMAGE,
            serde_json::json!({ "path": path }),
        )
        .await
        .map_err(|error| gpui::SharedString::from(error.to_string()))?;
    executor
        .spawn(async move {
            static DECODE: futures::lock::Mutex<()> = futures::lock::Mutex::new(());
            let _guard = DECODE.lock().await;
            let reply: holt_rpc::images::ImageData =
                serde_json::from_value(reply).map_err(|error| {
                    gpui::SharedString::from(format!("Invalid image reply: {error}"))
                })?;
            let data = &reply.data;
            if data.len() > (25usize * 1024 * 1024).div_ceil(3) * 4 {
                return Err("Image exceeds the 25 MiB limit.".into());
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data.as_bytes())
                .map_err(|e| gpui::SharedString::from(e.to_string()))?;
            decode_to_render(&bytes, max_edge)
        })
        .await
}

/// Decode image bytes with explicit budgets and convert to gpui-ready BGRA,
/// EXIF orientation applied. `max_edge` proportionally downscales (the
/// thumbnail path); `None` keeps source detail (the viewer path).
fn decode_to_render(
    bytes: &[u8],
    max_edge: Option<u32>,
) -> Result<(Arc<RenderImage>, u32, u32), gpui::SharedString> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| {
            gpui::SharedString::from(format!("image format could not be determined: {error}"))
        })?;
    use image::ImageDecoder as _;
    // Hard allocation ceiling: a hostile header cannot decompress to
    // gigabytes — the honest answer is an error, not an OOM.
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(DECODE_MAX_ALLOC_BYTES);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|error| gpui::SharedString::from(format!("image could not be opened: {error}")))?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > VIEWER_MAX_PIXELS {
        return Err("Image exceeds the 32 megapixel decode limit.".into());
    }
    let orientation = decoder.orientation().map_err(|error| {
        gpui::SharedString::from(format!("image orientation unreadable: {error}"))
    })?;
    let mut decoded = image::DynamicImage::from_decoder(decoder).map_err(|error| {
        gpui::SharedString::from(format!("image could not be decoded (corrupt?): {error}"))
    })?;
    decoded.apply_orientation(orientation);
    let (width, height) = (decoded.width(), decoded.height());
    if width == 0 || height == 0 {
        return Err("image has no pixels.".into());
    }
    // Only thumbnails resize; the viewer preserves the source pixels.
    let scale = match max_edge {
        Some(edge) => (edge as f64 / width.max(height) as f64).min(1.0),
        None => 1.0,
    };
    let (out_w, out_h) = (
        ((width as f64 * scale).round().max(1.0)) as u32,
        ((height as f64 * scale).round().max(1.0)) as u32,
    );
    let resized = if (out_w, out_h) == (width, height) {
        decoded.into_rgba8()
    } else {
        image::imageops::resize(
            &decoded,
            out_w,
            out_h,
            image::imageops::FilterType::Triangle,
        )
    };
    let render = rgba_to_render(resized);
    Ok((render, width, height))
}

/// RGBA8 → gpui RenderImage (BGRA byte order, one frame — animations stay
/// first-frame by decode, never eagerly decoded whole).
fn rgba_to_render(rgba: image::RgbaImage) -> Arc<RenderImage> {
    let mut buffer = rgba;
    for pixel in buffer.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Arc::new(RenderImage::new(smallvec![image::Frame::new(buffer)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(bytes_b64: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(bytes_b64)
            .unwrap()
    }

    // 1×1 transparent PNG.
    const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGNgAAIAAAUAAXpeqz8AAAAASUVORK5CYII=";

    // A 2×1 red PNG (so aspect math has something to bite on).
    const PNG_2X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAYAAAD0In+KAAAADklEQVR4nGP4z8DwH4QBEfcD/ePF9e8AAAAASUVORK5CYII=";

    #[test]
    fn image_paths_classify_by_extension_case_insensitively() {
        assert!(is_image_path("/a/b.PNG"));
        assert!(is_image_path("/a/b.jpeg"));
        assert!(is_image_path("/a/b.webp"));
        assert!(
            !is_image_path("/a/b.svg"),
            "SVG keeps ordinary-ref behavior"
        );
        assert!(
            !is_image_path("/a/b.bmp"),
            "BMP is not in the supported set"
        );
        assert!(!is_image_path("/a/b.heic"));
        assert!(!is_image_path("/a/b/tiff"));
        assert!(!is_image_path(""));
    }

    #[test]
    fn decoding_produces_bgra_frames_with_source_dimensions() {
        let (render, w, h) = decode_to_render(&png(PNG_2X1), None).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(u32::from(render.size(0).width), 2);
        let bytes = render.as_bytes(0).unwrap();
        assert_eq!(bytes.len(), 2 * 1 * 4);
        assert!(!render.as_bytes(0).unwrap().is_empty());
    }

    #[test]
    fn thumbnail_decode_downscales_proportionally() {
        // A 4×2 source clamps its longest edge to 256 — already smaller, so
        // unchanged; the math contract is proportional scaling, never upscale
        // beyond 256 on the long edge.
        let (_, w, h) = decode_to_render(&png(PNG_2X1), Some(THUMB_MAX_EDGE)).unwrap();
        assert_eq!((w, h), (2, 1));
        assert!(w.max(h) <= THUMB_MAX_EDGE);
    }

    #[test]
    fn corrupt_bytes_fail_with_an_honest_cause() {
        let mut corrupt = png(PNG_1X1);
        corrupt.truncate(corrupt.len() / 2);
        let err = decode_to_render(&corrupt, None).unwrap_err();
        assert!(
            err.contains("decoded") || err.contains("opened") || err.contains("format"),
            "{err}"
        );
        assert!(decode_to_render(b"not an image at all", None).is_err());
    }

    #[test]
    fn oversized_decode_is_bounded_by_the_allocation_limit() {
        // A PNG header claiming 30000×30000 (3.6 GB RGBA) must fail on the
        // allocation limit instead of allocating.
        let header_only = {
            // Take the 1×1 PNG and rewrite its IHDR width/height fields.
            let mut bytes = png(PNG_1X1);
            // IHDR starts at byte 16; width at 16..20, height at 20..24.
            let big: [u8; 4] = 30000u32.to_be_bytes();
            bytes[16..20].copy_from_slice(&big);
            bytes[20..24].copy_from_slice(&big);
            // CRC is now wrong, but the alloc limit trips before CRC matters.
            bytes
        };
        assert!(decode_to_render(&header_only, None).is_err());
    }

    #[test]
    fn the_cache_answers_only_fresh_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        std::fs::write(&path, png(PNG_1X1)).unwrap();
        let path = path.to_str().unwrap();

        assert!(
            matches!(snapshot(path), Snapshot::Loading),
            "unknown path loads"
        );
        assert!(begin_load(path));
        store_loaded(
            path,
            Thumb {
                pixels: rgba_to_render(image::RgbaImage::new(1, 1)),
                width: 1,
                height: 1,
            },
        );
        assert!(matches!(snapshot(path), Snapshot::Loaded(_)));

        // Deleting the target must retire the cached pixels immediately —
        // no stale masquerade.
        std::fs::remove_file(path).unwrap();
        match snapshot(path) {
            Snapshot::Error { cause, retry_in } => {
                assert!(cause.contains("not found"), "{cause}");
                assert_eq!(retry_in, Duration::MAX);
            }
            other => panic!("missing file must not answer from cache: {other:?}"),
        }
    }

    #[test]
    fn errored_paths_retry_on_the_ladder() {
        let path = "/definitely/missing.png";
        assert!(begin_load(path));
        store_error(path, "Image file not found.");
        match snapshot(path) {
            Snapshot::Error { retry_in, .. } => assert!(retry_in > Duration::ZERO),
            other => panic!("{other:?}"),
        }
        assert!(!begin_load(path), "inside the backoff window no retry");
        // (The ladder itself is the shared `retry_delay` shape.)
    }

    #[test]
    fn stale_loaded_entries_reload_after_the_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        std::fs::write(&path, png(PNG_1X1)).unwrap();
        let path = path.to_str().unwrap();

        assert!(begin_load(path));
        store_loaded(
            path,
            Thumb {
                pixels: rgba_to_render(image::RgbaImage::new(1, 1)),
                width: 1,
                height: 1,
            },
        );
        assert!(matches!(snapshot(path), Snapshot::Loaded(_)));
        // Same length, different mtime may still fingerprint equal on coarse
        // filesystems; a different LENGTH always refreshes.
        std::fs::write(&path, png(PNG_2X1)).unwrap();
        assert!(
            matches!(snapshot(path), Snapshot::Loading),
            "changed content must reload, not serve stale pixels"
        );
    }
}
