//! Provider brand marks: the compiled icons for catalog providers plus the
//! user-set logos of custom providers. Custom logos arrive inlined in
//! `ListProviders` rows; [`sync`] mirrors them into a process-wide registry
//! so every surface that only knows a provider id (chat tabs, the sidebar,
//! transcript cards) resolves the same mark.
//!
//! SVG logos render like the compiled marks — a mask tinted by the caller's
//! colour — and are served to gpui's SVG renderer through [`load_svg_asset`]
//! under a content-hashed path, so a replaced logo never hits a stale
//! rasterization. PNG logos render as-is.

use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash as _, Hasher as _};
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;
use gpui::{
    AnyElement, App, Hsla, IntoElement as _, ObjectFit, ParentElement as _, Pixels, RenderImage,
    SharedString, Styled as _, StyledImage as _, img, svg,
};
use holt_proto::{Provider, ProviderId, ProviderLogoFormat};

const SVG_ASSET_PREFIX: &str = "custom-logos/";
/// Unsaved SVGs shown by the logo dialog. Provider ids never contain `/`,
/// so this never collides with a stored logo's path.
const PREVIEW_PREFIX: &str = "custom-logos/preview/";

/// A provider mark ready to render at any size.
#[derive(Clone)]
pub enum BrandMark {
    /// An SVG asset path (compiled or custom), tinted by the caller.
    Icon(SharedString),
    /// A custom raster logo, drawn in its own colours.
    Image(Arc<RenderImage>),
}

impl BrandMark {
    pub fn icon(path: &'static str) -> Self {
        Self::Icon(path.into())
    }

    /// The mark at `size`; `color` tints icon marks and is ignored by
    /// raster logos.
    pub fn render(self, size: Pixels, color: Hsla) -> AnyElement {
        match self {
            Self::Icon(path) => svg()
                .path(path)
                .flex_none()
                .size(size)
                .text_color(color)
                .into_any_element(),
            Self::Image(image) => img(image)
                .flex_none()
                .size(size)
                .rounded(size * 0.2)
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
        }
    }
}

struct Entry {
    hash: u64,
    mark: BrandMark,
}

#[derive(Default)]
struct Registry {
    marks: HashMap<String, Entry>,
    /// Custom SVG bytes by asset path, served to the SVG renderer.
    svgs: HashMap<String, Arc<[u8]>>,
}

/// Process-wide: gpui's asset source asks for SVG bytes without an `App`.
#[cfg(not(test))]
fn with_registry<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    f(&mut REGISTRY.get_or_init(Default::default).lock().unwrap())
}

/// Per test thread, so parallel tests syncing their own provider lists
/// never evict each other's marks.
#[cfg(test)]
fn with_registry<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    thread_local!(static REGISTRY: std::cell::RefCell<Registry> = Default::default());
    REGISTRY.with(|registry| f(&mut registry.borrow_mut()))
}

/// The mark for a provider: the compiled brand icon for catalog ids, else a
/// custom provider's stored logo.
pub fn brand_mark(provider: &ProviderId) -> Option<BrandMark> {
    if let Some(path) = crate::pickers::builtin_brand_icon(provider) {
        return Some(BrandMark::icon(path));
    }
    custom_mark(provider.as_str())
}

pub fn custom_mark(provider_id: &str) -> Option<BrandMark> {
    with_registry(|registry| {
        registry
            .marks
            .get(provider_id)
            .map(|entry| entry.mark.clone())
    })
}

/// Mirror the logos of a full `ListProviders` answer into the registry.
/// Repaints every window when a mark changed; unchanged rows keep their
/// decoded image.
pub fn sync(providers: &[Provider], cx: &mut App) {
    let changed = with_registry(|registry| {
        let mut changed = false;
        let mut seen = Vec::new();
        for row in providers.iter().filter(|row| row.custom) {
            let Some(logo) = row.logo.as_ref() else {
                continue;
            };
            let id = row.id.to_string();
            let hash = {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                logo.format.hash(&mut hasher);
                logo.data.hash(&mut hasher);
                hasher.finish()
            };
            seen.push(id.clone());
            if registry
                .marks
                .get(&id)
                .is_some_and(|entry| entry.hash == hash)
            {
                continue;
            }
            let Some(mark) = decode(registry, &id, hash, logo.format, &logo.data) else {
                continue;
            };
            if let Some(old) = registry.marks.insert(id, Entry { hash, mark }) {
                release(registry, old.mark);
            }
            changed = true;
        }
        let stale: Vec<String> = registry
            .marks
            .keys()
            .filter(|id| !seen.contains(id))
            .cloned()
            .collect();
        for id in stale {
            if let Some(old) = registry.marks.remove(&id) {
                release(registry, old.mark);
                changed = true;
            }
        }
        changed
    });
    if changed {
        cx.refresh_windows();
    }
}

fn decode(
    registry: &mut Registry,
    provider_id: &str,
    hash: u64,
    format: ProviderLogoFormat,
    data: &str,
) -> Option<BrandMark> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .ok()?;
    match format {
        ProviderLogoFormat::Svg => {
            let path = format!("{SVG_ASSET_PREFIX}{provider_id}-{hash:016x}.svg");
            registry.svgs.insert(path.clone(), bytes.into());
            Some(BrandMark::Icon(path.into()))
        }
        ProviderLogoFormat::Png => match crate::images::decode_to_render(&bytes, None) {
            Ok((image, _, _)) => Some(BrandMark::Image(image)),
            Err(error) => {
                tracing::warn!(
                    provider = provider_id,
                    "custom provider logo unreadable: {error}"
                );
                None
            }
        },
    }
}

fn release(registry: &mut Registry, mark: BrandMark) {
    match mark {
        BrandMark::Icon(path) => {
            registry.svgs.remove(path.as_ref());
        }
        BrandMark::Image(image) => crate::images::discard(image),
    }
}

/// Cheap sniff for SVG markup (mirrors the engine's check).
pub fn looks_like_svg(text: &str) -> bool {
    let text = text.trim_start_matches('\u{feff}').trim_start();
    text.starts_with('<') && text.contains("<svg")
}

/// A tinted mark for SVG markup that is not saved yet. Only the latest
/// preview stays registered.
pub fn preview_svg(text: &str) -> Option<BrandMark> {
    if !looks_like_svg(text) {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    let path = format!("{PREVIEW_PREFIX}{:016x}.svg", hasher.finish());
    with_registry(|registry| {
        if !registry.svgs.contains_key(&path) {
            registry
                .svgs
                .retain(|key, _| !key.starts_with(PREVIEW_PREFIX));
            registry
                .svgs
                .insert(path.clone(), Arc::from(text.as_bytes()));
        }
    });
    Some(BrandMark::Icon(path.into()))
}

pub fn clear_previews() {
    with_registry(|registry| {
        registry
            .svgs
            .retain(|key, _| !key.starts_with(PREVIEW_PREFIX))
    });
}

/// The asset-source hook for custom SVG logos.
pub fn load_svg_asset(path: &str) -> Option<Cow<'static, [u8]>> {
    if !path.starts_with(SVG_ASSET_PREFIX) {
        return None;
    }
    with_registry(|registry| {
        registry
            .svgs
            .get(path)
            .map(|bytes| Cow::Owned(bytes.to_vec()))
    })
}

/// The placeholder tile for a custom provider without a logo: its
/// abbreviation, capped at two glyphs.
pub fn monogram(abbreviation: &str, size: Pixels, theme: &crate::theme::Theme) -> gpui::Div {
    gpui::div()
        .size(size)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(size * 0.23)
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .text_size(crate::typography::ui_rems(10.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text_muted)
        .child(SharedString::from(
            abbreviation.chars().take(2).collect::<String>(),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use holt_proto::ProviderLogo;

    fn custom(id: &str, logo: Option<ProviderLogo>) -> Provider {
        Provider {
            id: ProviderId::from(id),
            name: id.into(),
            abbreviation: "X".into(),
            configured: true,
            variants: Vec::new(),
            custom: true,
            logo,
        }
    }

    fn svg_logo(body: &str) -> ProviderLogo {
        ProviderLogo {
            format: ProviderLogoFormat::Svg,
            data: base64::engine::general_purpose::STANDARD.encode(body),
        }
    }

    fn asset_path(id: &str) -> Option<String> {
        match custom_mark(id)? {
            BrandMark::Icon(path) => Some(path.to_string()),
            BrandMark::Image(_) => None,
        }
    }

    #[gpui::test]
    fn sync_mirrors_custom_logos_and_drops_removed_ones(cx: &mut gpui::TestAppContext) {
        let id = "sync-test-provider";
        cx.update(|cx| sync(&[custom(id, Some(svg_logo("<svg/>")))], cx));
        let first = asset_path(id).expect("the svg logo registered");
        assert_eq!(load_svg_asset(&first).as_deref(), Some(&b"<svg/>"[..]));

        // A replaced logo gets a fresh asset path; the old bytes go.
        cx.update(|cx| sync(&[custom(id, Some(svg_logo("<svg></svg>")))], cx));
        let second = asset_path(id).unwrap();
        assert_ne!(first, second);
        assert!(load_svg_asset(&first).is_none());

        cx.update(|cx| sync(&[custom(id, None)], cx));
        assert!(custom_mark(id).is_none());
        assert!(load_svg_asset(&second).is_none());
    }

    #[test]
    fn previews_keep_only_the_latest_svg() {
        assert!(preview_svg("not svg").is_none());
        let Some(BrandMark::Icon(first)) = preview_svg("<svg/>") else {
            panic!("an svg preview");
        };
        let Some(BrandMark::Icon(second)) = preview_svg("<svg></svg>") else {
            panic!("an svg preview");
        };
        assert!(load_svg_asset(&first).is_none());
        assert!(load_svg_asset(&second).is_some());
        clear_previews();
        assert!(load_svg_asset(&second).is_none());
    }
}
