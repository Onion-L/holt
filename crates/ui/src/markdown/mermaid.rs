//! Mermaid fences → rendered diagram images, fully in-process.
//!
//! Pipeline: fence source → `merman` (pure-Rust mermaid parse/layout/SVG,
//! resvg-safe output) → gpui's `SvgRenderer::render_single_frame` →
//! `Arc<RenderImage>` → the block renders as an image instead of code.
//! Anything unsupported, still rendering, or failed falls back to the plain
//! code block — a diagram never blocks layout and never panics the UI
//! (merman is pinned on an alpha; its render runs unwind-guarded).
//!
//! The cache follows the transcript's `HighlightStore` shape: the owner
//! requests per `(row, block)` slot, a background task fills
//! `Option<Arc<RenderImage>>`, and completion calls `cx.notify()`; `None`
//! means "render the code block" (pending or failed alike).

use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, anyhow};
use gpui::{Context, Hsla, RenderImage, SharedString, SvgRenderer, Task};
use sha2::{Digest, Sha256};

use crate::theme::Theme;

/// Fence tag that selects the diagram path (```mermaid).
pub const MERMAID_LANG: &str = "mermaid";

/// Diagram families we render; everything else stays a code block. Mirrors
/// Zed's whitelist: merman can draw more, but these are the ones with
/// verified text legibility through the resvg-safe pipeline.
const SUPPORTED_PREFIXES: &[&str] = &[
    "flowchart",
    "graph",
    "sequenceDiagram",
    "classDiagram",
    "stateDiagram",
    "erDiagram",
    "gantt",
    "pie",
    "gitGraph",
    "mindmap",
    "timeline",
    "quadrantChart",
    "xychart-beta",
    "journey",
];

pub fn is_mermaid(language: Option<&str>) -> bool {
    language == Some(MERMAID_LANG)
}

/// Whether a fence body's diagram family is on the rendered whitelist.
pub fn supported_diagram(source: &str) -> bool {
    let trimmed = source.trim_start();
    SUPPORTED_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

/// Mermaid theme variables derived from the app theme, as CSS color strings.
///
/// Shape fills and strokes are translucent ink, not the opaque surface
/// tokens: the diagram composites onto a card that may sit on glass, and
/// opaque near-black plates read as holes punched through the frost.
/// `raised` stays opaque — mermaid derives pie/gantt/git palettes from
/// `primaryColor` and those must not inherit an alpha.
struct MermaidPalette {
    dark: bool,
    font_family: String,
    raised: String,
    node_fill: String,
    node_border: String,
    cluster_fill: String,
    cluster_border: String,
    line: String,
    label_bg: String,
    text: String,
    text_muted: String,
    accent_wash: String,
}

impl MermaidPalette {
    fn of(theme: &Theme) -> Self {
        Self {
            dark: theme.appearance.is_dark(),
            // The trailing sans-serif is load-bearing: merman's text
            // measurement is conservative for unknown fonts and picks a
            // visibly too-narrow width otherwise (same workaround Zed uses).
            font_family: format!("{}, sans-serif", theme.font_sans),
            raised: css(theme.surface_raised),
            node_fill: css(theme.ink(0.06)),
            node_border: css(theme.hairline(0.28)),
            cluster_fill: css(theme.ink(0.025)),
            cluster_border: css(theme.hairline(0.14)),
            line: css(theme.text_muted),
            label_bg: css(theme.bg.opacity(0.7)),
            text: css(theme.text),
            text_muted: css(theme.text_muted),
            accent_wash: css(theme.accent_wash),
        }
    }
}

fn css(color: Hsla) -> String {
    let rgba = color.to_rgb();
    let (r, g, b) = (
        (rgba.r * 255.0).round() as u8,
        (rgba.g * 255.0).round() as u8,
        (rgba.b * 255.0).round() as u8,
    );
    if rgba.a >= 1.0 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("rgba({r}, {g}, {b}, {:.3})", rgba.a)
    }
}

fn merman_config(palette: &MermaidPalette) -> merman::MermaidConfig {
    let MermaidPalette {
        dark,
        font_family,
        raised,
        node_fill,
        node_border,
        cluster_fill,
        cluster_border,
        line,
        label_bg,
        text,
        text_muted,
        accent_wash,
    } = palette;
    merman::MermaidConfig::from_value(serde_json::json!({
        "theme": "base",
        "darkMode": dark,
        "fontFamily": font_family,
        "htmlLabels": true,
        "flowchart": { "htmlLabels": true, "padding": 16 },
        "themeVariables": {
            "background": "transparent",
            "primaryColor": raised,
            "primaryTextColor": text,
            "primaryBorderColor": node_border,
            "lineColor": line,
            "secondaryColor": accent_wash,
            "secondaryTextColor": text,
            "tertiaryColor": cluster_fill,
            "tertiaryTextColor": text_muted,
            "mainBkg": node_fill,
            "nodeBorder": node_border,
            "nodeTextColor": text,
            "clusterBkg": cluster_fill,
            "clusterBorder": cluster_border,
            "titleColor": text_muted,
            "edgeLabelBackground": label_bg,
            "textColor": text,
            "noteBkgColor": accent_wash,
            "noteBorderColor": node_border,
            "noteTextColor": text,
            "actorBkg": node_fill,
            "actorBorder": node_border,
            "actorTextColor": text,
            "actorLineColor": cluster_border,
            "labelBoxBkgColor": node_fill,
            "labelBoxBorderColor": node_border,
            "labelTextColor": text,
            "loopTextColor": text_muted,
            "signalColor": line,
            "signalTextColor": text,
            "classText": text,
            "labelColor": text,
        },
    }))
}

/// merman → resvg-safe SVG. Sync, owned data only — runs on the background
/// executor.
fn render_svg(source: &str, palette: &MermaidPalette) -> anyhow::Result<String> {
    static DIAGRAM_COUNTER: AtomicU64 = AtomicU64::new(0);
    let diagram_id = format!(
        "holt-mermaid-{}",
        DIAGRAM_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let renderer = merman::svg::HeadlessRenderer::new()
        .with_site_config(merman_config(palette))
        .with_vendored_text_measurer()
        .with_diagram_id(&diagram_id);
    // resvg can't rasterize `<foreignObject>` labels and drops CSS at-rules;
    // this pipeline replaces them with wrapped native SVG text before we
    // rasterize.
    // Upstream mermaid hardcodes `background-color: white` on the root <svg>
    // style and resvg paints it — rewrite to transparent so the diagram
    // composites onto the host surface (the code-block-style card) instead.
    let pipeline = merman::svg::SvgPipeline::resvg_safe()
        .with_postprocessor(merman::svg::CssOverridePostprocessor::strip_existing_important())
        .with_postprocessor(merman::svg::RootBackgroundPostprocessor::new("transparent"))
        .with_postprocessor(ClusterTitleLeft);
    renderer
        .render_svg_with_pipeline_sync(source, &pipeline)
        .context("merman render failed")?
        .ok_or_else(|| anyhow!("merman returned no SVG for the given source"))
}

/// Left-aligns flowchart subgraph titles. Upstream centers them over the
/// cluster, and dagre routinely drops a cross-cluster edge label onto the
/// middle of a cluster's top border — exactly where a centered title sits.
struct ClusterTitleLeft;

/// Title inset from the cluster's left edge, in SVG units.
const CLUSTER_TITLE_INSET: f64 = 12.0;

impl merman::svg::SvgPostprocessor for ClusterTitleLeft {
    fn name(&self) -> &'static str {
        "holt-cluster-title-left"
    }

    fn process<'a>(
        &self,
        svg: std::borrow::Cow<'a, str>,
        ctx: &merman::svg::SvgPostprocessContext<'_>,
    ) -> merman::svg::RenderResult<std::borrow::Cow<'a, str>> {
        if !ctx
            .diagram_type()
            .is_some_and(|t| t.starts_with("flowchart"))
        {
            return Ok(svg);
        }
        Ok(left_align_cluster_titles(&svg)
            .map(std::borrow::Cow::Owned)
            .unwrap_or(svg))
    }
}

/// `<g class="cluster" …><rect x="X" …/><g class="cluster-label"
/// transform="translate(TX,TY)">` → TX = X + inset. `None` when nothing
/// matched, so the pass stays a no-op on unexpected markup.
fn left_align_cluster_titles(svg: &str) -> Option<String> {
    const CLUSTER: &str = r#"<g class="cluster""#;
    const LABEL: &str = r#"<g class="cluster-label" transform="translate("#;
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg;
    let mut changed = false;
    while let Some(at) = rest.find(LABEL) {
        let (head, tail) = rest.split_at(at + LABEL.len());
        out.push_str(head);
        rest = tail;
        let group = &head[head.rfind(CLUSTER).unwrap_or(0)..];
        let rect_x = group.find("<rect").and_then(|r| attr_f64(&group[r..], "x"));
        if let (Some(x), Some(comma)) = (rect_x, rest.find(',')) {
            out.push_str(&format!("{}", x + CLUSTER_TITLE_INSET));
            rest = &rest[comma..];
            changed = true;
        }
    }
    out.push_str(rest);
    changed.then_some(out)
}

/// Numeric attribute from the first tag in `tag` (`name="…"`).
fn attr_f64(tag: &str, name: &str) -> Option<f64> {
    let tag = &tag[..tag.find('>')?];
    let needle = format!(" {name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let len = tag[start..].find('"')?;
    tag[start..start + len].parse().ok()
}

/// SVG bytes → rasterized image at `zoom` (logical size = natural × zoom,
/// still 2× supersampled). The displayed scale is fit × zoom with fit ≤ 1,
/// so rasterizing at `zoom` never undersamples. `None` on failure.
/// Background executor.
fn rasterize(svg: &str, svg_renderer: &SvgRenderer, zoom: f32) -> Option<Arc<RenderImage>> {
    svg_renderer.render_single_frame(svg.as_bytes(), zoom).ok()
}

/// Cache identity: diagram source + appearance (colors are baked into the
/// SVG at render time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MermaidKey {
    content_hash: [u8; 32],
    dark: bool,
}

impl MermaidKey {
    fn new(code: &str, dark: bool) -> Self {
        Self {
            content_hash: Sha256::digest(code.as_bytes()).into(),
            dark,
        }
    }
}

struct MermaidEntry {
    key: MermaidKey,
    /// Resvg-safe SVG kept for re-rasterization at other zoom levels.
    /// `None` after a render failure — permanent code-block fallback.
    svg: Option<Arc<str>>,
    /// Rasterized at `raster_zoom`; `None` while pending or failed.
    image: Option<Arc<RenderImage>>,
    raster_zoom: f32,
    /// User zoom (wheel), relative to the fit width; clamped [0.5, 6].
    zoom: f32,
    _task: Option<Task<()>>,
    raster_task: Option<Task<()>>,
}

/// Bounded per-owner cache of rendered diagrams, keyed by (row key, block).
const MAX_ENTRIES: usize = 48;

/// Cache of rendered mermaid diagrams following the `HighlightStore` shape:
/// owned by the rendering entity, filled by background tasks.
#[derive(Default)]
pub struct MermaidStore {
    entries: HashMap<(SharedString, usize), MermaidEntry>,
    recency: VecDeque<(SharedString, usize)>,
}

/// Zoom bounds for the wheel control (1.0 = fit width).
const ZOOM_MIN: f32 = 0.5;
const ZOOM_MAX: f32 = 6.0;
/// Re-rasterize once the user zoom drifts this far from the raster's scale —
/// below the threshold the 2× supersampled bitmap upscales cleanly.
const RASTER_DRIFT: f32 = 0.2;

/// Owner of a [`MermaidStore`]. The background completion needs to write the
/// rendered image back through the owning entity.
pub trait MermaidHost: 'static {
    fn mermaid_store(&mut self) -> &mut MermaidStore;
}

impl MermaidStore {
    /// Current image if ready; kicks a background render when stale/missing.
    /// `None` renders the code block (pending, failed, or evicted).
    pub fn request<H: MermaidHost>(
        &mut self,
        row_key: SharedString,
        block_ix: usize,
        code: &str,
        cx: &mut Context<H>,
    ) -> Option<Arc<RenderImage>> {
        let palette = MermaidPalette::of(Theme::of(cx));
        let key = MermaidKey::new(code, palette.dark);
        let slot = (row_key, block_ix);
        if let Some(entry) = self.entries.get(&slot)
            && entry.key == key
        {
            return entry.image.clone();
        }
        let source = code.to_string();
        let svg_renderer = cx.svg_renderer();
        let task_slot = slot.clone();
        let task = cx.spawn(async move |this, cx| {
            // merman is pinned on an alpha: a panic inside a diagram degrades
            // to the code block, never takes the app down.
            let rendered = cx
                .background_executor()
                .spawn(async move {
                    let svg = catch_unwind(AssertUnwindSafe(|| render_svg(&source, &palette)))
                        .ok()
                        .and_then(|r| r.ok())
                        .map(Arc::<str>::from);
                    let image = svg
                        .as_ref()
                        .and_then(|svg| rasterize(svg, &svg_renderer, 1.0));
                    (svg, image)
                })
                .await;
            this.update(cx, |host, cx| {
                if let Some(entry) = host.mermaid_store().entries.get_mut(&task_slot)
                    && entry.key == key
                {
                    let (svg, image) = rendered;
                    entry.svg = svg;
                    entry.image = image;
                    entry.raster_zoom = 1.0;
                    entry._task = None;
                    cx.notify();
                }
            })
            .ok();
        });
        self.entries.insert(
            slot.clone(),
            MermaidEntry {
                key,
                svg: None,
                image: None,
                raster_zoom: 1.0,
                zoom: 1.0,
                _task: Some(task),
                raster_task: None,
            },
        );
        self.recency.push_back(slot);
        while self.entries.len() > MAX_ENTRIES {
            let Some(oldest) = self.recency.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
        None
    }

    /// Current user zoom for a slot (1.0 = fit width).
    pub fn zoom_for(&self, row_key: &SharedString, block_ix: usize) -> f32 {
        self.entries
            .get(&(row_key.clone(), block_ix))
            .map(|entry| entry.zoom)
            .unwrap_or(1.0)
    }

    /// Scale the slot's current image was rasterized at — its pixel size is
    /// natural × this (× 2 supersampling), which lags `zoom` by up to
    /// [`RASTER_DRIFT`] or while a re-raster is in flight.
    pub fn raster_zoom_for(&self, row_key: &SharedString, block_ix: usize) -> f32 {
        self.entries
            .get(&(row_key.clone(), block_ix))
            .map(|entry| entry.raster_zoom)
            .unwrap_or(1.0)
    }

    /// Multiply a slot's zoom by `factor` (wheel step or +/− button).
    /// Re-rasterizes in the background once the drift exceeds
    /// [`RASTER_DRIFT`]; until it lands the existing bitmap upscales — no
    /// flicker, just a soft frame or two. Returns whether anything changed.
    pub fn zoom_step<H: MermaidHost>(
        &mut self,
        row_key: SharedString,
        block_ix: usize,
        factor: f32,
        cx: &mut Context<H>,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(&(row_key.clone(), block_ix)) else {
            return false;
        };
        let (Some(_), Some(_)) = (&entry.svg, &entry.image) else {
            return false;
        };
        let target = (entry.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        if target == entry.zoom {
            return false;
        }
        entry.zoom = target;
        cx.notify();
        self.ensure_raster(row_key, block_ix, cx);
        true
    }

    /// Reset a slot to fit (zoom 1.0). Returns whether changed.
    pub fn zoom_reset<H: MermaidHost>(
        &mut self,
        row_key: SharedString,
        block_ix: usize,
        cx: &mut Context<H>,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(&(row_key.clone(), block_ix)) else {
            return false;
        };
        let (Some(_), Some(_)) = (&entry.svg, &entry.image) else {
            return false;
        };
        if entry.zoom == 1.0 {
            return false;
        }
        entry.zoom = 1.0;
        cx.notify();
        self.ensure_raster(row_key, block_ix, cx);
        true
    }

    /// Spawn a re-rasterization when the slot's zoom drifted from the
    /// raster's scale; the existing bitmap stays visible until it lands.
    fn ensure_raster<H: MermaidHost>(
        &mut self,
        row_key: SharedString,
        block_ix: usize,
        cx: &mut Context<H>,
    ) {
        let Some(entry) = self.entries.get_mut(&(row_key.clone(), block_ix)) else {
            return;
        };
        if (entry.zoom / entry.raster_zoom - 1.0).abs() <= RASTER_DRIFT
            || entry.raster_task.is_some()
        {
            return;
        }
        let key = entry.key;
        let target = entry.zoom;
        let Some(svg) = entry.svg.clone() else {
            return;
        };
        let svg_renderer = cx.svg_renderer();
        let task_slot = (row_key, block_ix);
        let writeback_slot = task_slot.clone();
        let task = cx.spawn(async move |this, cx| {
            let image = cx
                .background_executor()
                .spawn(async move { rasterize(&svg, &svg_renderer, target) })
                .await;
            this.update(cx, |host, cx| {
                if let Some(entry) = host.mermaid_store().entries.get_mut(&task_slot)
                    && entry.key == key
                {
                    if entry.zoom == target {
                        if let Some(image) = image {
                            entry.image = Some(image);
                            entry.raster_zoom = target;
                        }
                        entry.raster_task = None;
                        cx.notify();
                    } else {
                        // Superseded by a newer step: free the slot so the
                        // next step can re-rasterize at the current zoom.
                        entry.raster_task = None;
                    }
                }
            })
            .ok();
        });
        if let Some(entry) = self.entries.get_mut(&writeback_slot) {
            entry.raster_task = Some(task);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mermaid_language_gate() {
        assert!(is_mermaid(Some("mermaid")));
        assert!(!is_mermaid(Some("rust")));
        assert!(!is_mermaid(None));
    }

    #[test]
    fn diagram_whitelist() {
        for source in [
            "flowchart TD\n A --> B",
            "\n  graph LR\n A --> B",
            "sequenceDiagram\n Alice->>Bob: hi",
            "stateDiagram-v2\n [*] --> s",
            "erDiagram\n USER ||--o{ POST : writes",
            "gitGraph\n commit",
        ] {
            assert!(supported_diagram(source), "should render: {source:?}");
        }
        for source in [
            "sankey-beta\n\nA,1",
            "block-beta\n columns 1",
            "just some prose",
            "",
        ] {
            assert!(!supported_diagram(source), "should stay code: {source:?}");
        }
    }

    #[test]
    fn key_tracks_source_and_appearance() {
        assert_eq!(MermaidKey::new("a", true), MermaidKey::new("a", true));
        assert_ne!(MermaidKey::new("a", true), MermaidKey::new("b", true));
        assert_ne!(MermaidKey::new("a", true), MermaidKey::new("a", false));
    }

    #[test]
    fn flowchart_renders_raster_safe_svg() {
        let palette = test_palette();
        let svg = render_svg("flowchart TD\n    A[Open] --> B[Close]", &palette)
            .expect("flowchart should render");
        assert!(svg.contains("<svg"), "got: {svg}");
        assert!(!svg.contains("<foreignObject"), "got: {svg}");
        assert!(!svg.contains("@keyframes"), "got: {svg}");
        assert!(!svg.contains("!important"), "got: {svg}");
        // Upstream hardcodes a white root background for browsers; it must be
        // rewritten, or resvg paints an opaque white canvas.
        assert!(
            !svg.to_lowercase().contains("background-color:white"),
            "got: {svg}"
        );
    }

    #[test]
    fn cluster_titles_move_to_the_left_edge() {
        let svg = r#"<g class="cluster" id="a"><rect x="100.5" y="0" width="400" height="80"/><g class="cluster-label" transform="translate(300,6)"></g></g><g class="cluster" id="b"><rect x="7" y="90" width="50" height="50"/><g class="cluster-label" transform="translate(32,96)"></g></g>"#;
        let out = left_align_cluster_titles(svg).expect("clusters matched");
        assert!(out.contains("translate(112.5,6)"), "got: {out}");
        assert!(out.contains("translate(19,96)"), "got: {out}");
        // No clusters: untouched.
        assert!(left_align_cluster_titles("<svg><rect x=\"1\"/></svg>").is_none());
    }

    #[test]
    fn flowchart_subgraph_title_is_left_aligned() {
        let svg = render_svg(
            "flowchart LR\n    subgraph s[Title]\n        A --> B\n    end",
            &test_palette(),
        )
        .expect("render");
        let rect_x = svg
            .find(r#"<g class="cluster""#)
            .and_then(|at| {
                let group = &svg[at..];
                attr_f64(&group[group.find("<rect")?..], "x")
            })
            .expect("cluster rect");
        let label = format!(
            r#"<g class="cluster-label" transform="translate({},"#,
            rect_x + CLUSTER_TITLE_INSET
        );
        assert!(svg.contains(&label), "got: {svg}");
    }

    #[test]
    fn unsupported_and_garbage_sources_fail_soft() {
        let palette = test_palette();
        // Whitelisted prefix, invalid body: merman yields no SVG — the error
        // path the code-block fallback exists for.
        assert!(render_svg("flowchart ??? ]][", &palette).is_err());
    }

    #[test]
    fn rasterized_canvas_is_transparent_not_white() {
        let palette = test_palette();
        let svg = render_svg("flowchart TD\n    A[Open] --> B[Close]", &palette)
            .expect("flowchart should render");
        // Empty asset source suffices — diagram SVGs carry no external refs.
        let renderer = gpui::SvgRenderer::new(std::sync::Arc::new(()));
        let image = renderer
            .render_single_frame(svg.as_bytes(), 1.0)
            .expect("rasterize");
        let size = image.size(0);
        let (w, h) = (size.width.0 as usize, size.height.0 as usize);
        let data = image.as_bytes(0).expect("frame bytes");
        let px = |x: usize, y: usize| {
            let i = (y * w + x) * 4;
            (data[i + 3], data[i + 2], data[i + 1], data[i]) // a, r, g, b
        };
        // Canvas corner: fully transparent (composites onto the host card).
        let (a, _, _, _) = px(0, 0);
        assert_eq!(a, 0, "canvas corner must stay transparent");
        // Somewhere in the middle a node stroke/fill is actually drawn.
        assert!(
            (0..h)
                .step_by(h / 16 + 1)
                .any(|y| (0..w).step_by(w / 16 + 1).any(|x| px(x, y).0 > 0)),
            "diagram content should paint opaque pixels"
        );
    }

    fn test_palette() -> MermaidPalette {
        MermaidPalette {
            dark: true,
            font_family: "Geist, sans-serif".into(),
            raised: "#161616".into(),
            node_fill: "rgba(255, 255, 255, 0.060)".into(),
            node_border: "rgba(255, 255, 255, 0.280)".into(),
            cluster_fill: "rgba(255, 255, 255, 0.025)".into(),
            cluster_border: "rgba(255, 255, 255, 0.140)".into(),
            line: "#989898".into(),
            label_bg: "rgba(6, 6, 6, 0.700)".into(),
            text: "#e7e7e7".into(),
            text_muted: "#989898".into(),
            accent_wash: "rgba(99, 102, 241, 0.22)".into(),
        }
    }
}
