//! Read-only, native GPUI rendering of a workflow's actual agent invocations.
//! The transcript owns the persisted inputs; this module owns graph topology,
//! deterministic layout, and the spatial canvas interaction.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, CursorStyle, EntityId, Hsla, MouseButton, PathBuilder, Pixels, Point,
    SharedString, Window, canvas, div, point, prelude::*, px, size,
};
use holt_doc::SubagentStatus;

use crate::i18n::t;
use crate::theme::Theme;
use crate::transcript::agent_avatar::{AgentIdentity, AvatarMood, agent_avatar};

const MAX_NODES: usize = 128;
const MAX_EDGES: usize = 1024;
const NODE_WIDTH: f32 = 232.0;
const NODE_HEIGHT: f32 = 64.0;
const LAYER_GAP: f32 = 72.0;
const NODE_GAP: f32 = 12.0;
const GRAPH_PADDING: f32 = 16.0;
const CANVAS_HEIGHT: f32 = 420.0;
const DOT_SPACING: f32 = 20.0;
const DEFAULT_PAN: f32 = 24.0;
pub(crate) const ZOOM_MIN: f32 = 0.6;
pub(crate) const ZOOM_MAX: f32 = 1.75;

/// One actual workflow `agent()` invocation, projected from its spawn part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentNode {
    pub call_id: SharedString,
    pub subagent_ref: Option<SharedString>,
    pub label: String,
    pub phase: Option<String>,
    pub status: Option<SubagentStatus>,
    pub tokens: Option<u64>,
}

/// Actual agents and the contiguous phase transitions visible in the spawn
/// record. Equal-phase agents stay independent; missing phases add no edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentGraph {
    nodes: Vec<AgentNode>,
    layers: Vec<Vec<usize>>,
    edges: Vec<(usize, usize)>,
}

pub(crate) fn agent_graph(agents: &[AgentNode]) -> Option<AgentGraph> {
    if agents.is_empty() || agents.len() > MAX_NODES {
        return None;
    }
    let mut layers: Vec<Vec<usize>> = Vec::new();
    for (ix, agent) in agents.iter().enumerate() {
        if ix == 0 || agent.phase != agents[ix - 1].phase {
            layers.push(Vec::new());
        }
        layers.last_mut()?.push(ix);
    }
    let mut edges = Vec::new();
    for pair in layers.windows(2) {
        let [from, to] = pair else { continue };
        for parent in from {
            for child in to {
                edges.push((*parent, *child));
            }
        }
    }
    if edges.len() > MAX_EDGES {
        return None;
    }
    Some(AgentGraph {
        nodes: agents.to_vec(),
        layers,
        edges,
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Placement {
    x: f32,
    y: f32,
}

#[derive(Debug, Clone, PartialEq)]
struct GraphLayout {
    width: f32,
    height: f32,
    node_width: f32,
    node_height: f32,
    placements: Vec<Placement>,
}

impl AgentGraph {
    pub(crate) fn nodes(&self) -> &[AgentNode] {
        &self.nodes
    }

    pub(crate) fn node(&self, ix: usize) -> Option<&AgentNode> {
        self.nodes.get(ix)
    }

    fn layout(&self, zoom: f32) -> GraphLayout {
        let node_width = NODE_WIDTH * zoom;
        let node_height = NODE_HEIGHT * zoom;
        let layer_gap = LAYER_GAP * zoom;
        let node_gap = NODE_GAP * zoom;
        let padding = GRAPH_PADDING * zoom;
        let rows = self.layers.iter().map(Vec::len).max().unwrap_or(0);
        let content_height = rows as f32 * node_height + rows.saturating_sub(1) as f32 * node_gap;
        let width = padding * 2.0
            + self.layers.len() as f32 * node_width
            + self.layers.len().saturating_sub(1) as f32 * layer_gap;
        let height = content_height + padding * 2.0;
        let mut placements = vec![Placement::default(); self.nodes.len()];
        for (layer_ix, layer) in self.layers.iter().enumerate() {
            let layer_height =
                layer.len() as f32 * node_height + layer.len().saturating_sub(1) as f32 * node_gap;
            let x = padding + layer_ix as f32 * (node_width + layer_gap);
            let top = padding + (content_height - layer_height) / 2.0;
            for (row, node) in layer.iter().enumerate() {
                placements[*node] = Placement {
                    x,
                    y: top + row as f32 * (node_height + node_gap),
                };
            }
        }
        GraphLayout {
            width,
            height,
            node_width,
            node_height,
            placements,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WorkflowViewport {
    pub zoom: f32,
    pub pan_x: f32,
    pub pan_y: f32,
}

impl Default for WorkflowViewport {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan_x: DEFAULT_PAN,
            pan_y: DEFAULT_PAN,
        }
    }
}

pub(crate) enum CanvasPanEvent {
    Start(Point<Pixels>),
    Move(Point<Pixels>),
    End,
    Reset,
}

pub(crate) struct WorkflowGraphDrag {
    pub call_id: SharedString,
    pub last_position: Point<Pixels>,
}

pub(crate) type ZoomHandler = Rc<dyn Fn(f32, &mut Window, &mut App)>;
pub(crate) type NodeOpenHandler = Rc<dyn Fn(usize, &mut Window, &mut App)>;
pub(crate) type PanHandler = Rc<dyn Fn(CanvasPanEvent, &mut App)>;
pub(crate) type ExpandHandler = Rc<dyn Fn(&mut App)>;

pub(crate) struct GraphInteraction {
    pub identities: Vec<Option<AgentIdentity>>,
    pub on_open: NodeOpenHandler,
    pub on_zoom: ZoomHandler,
    pub on_pan: PanHandler,
    pub on_expand: ExpandHandler,
    pub dragging: bool,
    /// The canvas fills its parent's height (the transcript-covering
    /// overlay) instead of keeping the inline row height.
    pub expanded: bool,
    pub view: EntityId,
}

/// Native spatial canvas: nodes, edges, and the dot grid share pan/zoom
/// coordinates. Blank-space drags pan; agent clicks never become drags.
pub(crate) fn render_agent_graph(
    id: &SharedString,
    graph: &AgentGraph,
    viewport: WorkflowViewport,
    theme: &Theme,
    interaction: GraphInteraction,
    cx: &mut App,
) -> AnyElement {
    let GraphInteraction {
        identities,
        on_open,
        on_zoom,
        on_pan,
        on_expand,
        dragging,
        expanded,
        view,
    } = interaction;
    let zoom = viewport.zoom;
    let layout = graph.layout(zoom);

    let dot_color = theme.text_muted.opacity(0.20);
    let dot_canvas = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            paint_dot_grid(
                bounds,
                viewport.pan_x,
                viewport.pan_y,
                zoom,
                dot_color,
                window,
            );
        },
    )
    .absolute()
    .inset_0();

    let edges = graph.edges.clone();
    let edge_layout = layout.clone();
    let edge_color = theme.text_muted.opacity(0.35);
    let edge_canvas = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            for (from, to) in &edges {
                let from = edge_layout.placements[*from];
                let to = edge_layout.placements[*to];
                let start = point(
                    bounds.origin.x + px(from.x + edge_layout.node_width),
                    bounds.origin.y + px(from.y + edge_layout.node_height / 2.0),
                );
                let end = point(
                    bounds.origin.x + px(to.x),
                    bounds.origin.y + px(to.y + edge_layout.node_height / 2.0),
                );
                let bend = px(f32::from(end.x - start.x) * 0.5);
                let mut builder = PathBuilder::stroke(px(1.0));
                builder.move_to(start);
                builder.cubic_bezier_to(
                    end,
                    point(start.x + bend, start.y),
                    point(end.x - bend, end.y),
                );
                if let Ok(path) = builder.build() {
                    window.paint_path(path, edge_color);
                }

                let mut arrow = PathBuilder::fill();
                arrow.move_to(end);
                arrow.line_to(point(end.x - px(5.0 * zoom), end.y - px(3.0 * zoom)));
                arrow.line_to(point(end.x - px(5.0 * zoom), end.y + px(3.0 * zoom)));
                arrow.close();
                if let Ok(path) = arrow.build() {
                    window.paint_path(path, edge_color);
                }
            }
        },
    )
    .absolute()
    .inset_0();

    let nodes =
        graph
            .nodes
            .iter()
            .zip(&layout.placements)
            .enumerate()
            .map(|(ix, (node, placement))| {
                render_agent_node(
                    ix,
                    node,
                    *placement,
                    &layout,
                    zoom,
                    NodePresentation {
                        theme,
                        identity: identities.get(ix).cloned().flatten(),
                        on_open: on_open.clone(),
                        view,
                    },
                    cx,
                )
            });
    let world = div()
        .absolute()
        .left(px(viewport.pan_x))
        .top(px(viewport.pan_y))
        .w(px(layout.width))
        .h(px(layout.height))
        .child(edge_canvas)
        .children(nodes);

    let pan_start = on_pan.clone();
    let pan_move = on_pan.clone();
    let pan_end = on_pan.clone();
    let pan_end_out = on_pan.clone();
    let body = div()
        .id(SharedString::from(format!("workflow-viewport-{id}")))
        .debug_selector(|| "workflow-viewport".to_string())
        .relative()
        .w_full()
        .when(expanded, |body| body.flex_1().min_h_0())
        .when(!expanded, |body| body.h(px(CANVAS_HEIGHT)))
        .overflow_hidden()
        .cursor(if dragging {
            CursorStyle::ClosedHand
        } else {
            CursorStyle::OpenHand
        })
        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
            window.prevent_default();
            pan_start(CanvasPanEvent::Start(event.position), cx);
            cx.stop_propagation();
        })
        .on_mouse_move(move |event: &gpui::MouseMoveEvent, _, cx| {
            if event.pressed_button == Some(MouseButton::Left) {
                pan_move(CanvasPanEvent::Move(event.position), cx);
                cx.stop_propagation();
            }
        })
        .on_mouse_up(MouseButton::Left, move |_, _, cx| {
            pan_end(CanvasPanEvent::End, cx)
        })
        .on_mouse_up_out(MouseButton::Left, move |_, _, cx| {
            pan_end_out(CanvasPanEvent::End, cx)
        })
        .child(dot_canvas)
        .child(world);

    let controls = canvas_controls(
        id,
        viewport,
        expanded,
        theme,
        on_zoom.clone(),
        on_pan,
        on_expand,
    );
    div()
        .relative()
        .w_full()
        .when(expanded, |frame| frame.flex_1().min_h_0().flex().flex_col())
        .when(!expanded, |frame| {
            frame.border_1().border_color(theme.border)
        })
        .bg(theme.bg)
        .overflow_hidden()
        .debug_selector(|| "workflow-graph".to_string())
        .on_scroll_wheel(move |event: &gpui::ScrollWheelEvent, window, cx| {
            if !(event.modifiers.control || event.modifiers.platform) {
                return;
            }
            let lines: f32 = match event.delta {
                gpui::ScrollDelta::Lines(lines) => lines.y,
                gpui::ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / 18.0,
            };
            on_zoom((1.0 + lines * 0.15).clamp(0.5, 2.0), window, cx);
            cx.stop_propagation();
        })
        .child(body)
        .child(
            div()
                .absolute()
                .left(px(12.0))
                .top(px(12.0))
                .text_size(px(10.0))
                .font_family(theme.font_mono.clone())
                .text_color(theme.text_faint)
                .child(format!("DRAG TO PAN / {:.0}%", zoom * 100.0)),
        )
        .child(controls)
        .into_any_element()
}

fn paint_dot_grid(
    bounds: Bounds<Pixels>,
    pan_x: f32,
    pan_y: f32,
    zoom: f32,
    color: Hsla,
    window: &mut Window,
) {
    let spacing = DOT_SPACING * zoom;
    let dot = (1.2 * zoom).clamp(1.0, 2.0);
    let mut x = bounds.origin.x + px(pan_x.rem_euclid(spacing));
    let right = bounds.origin.x + bounds.size.width;
    let bottom = bounds.origin.y + bounds.size.height;
    while x < right {
        let mut y = bounds.origin.y + px(pan_y.rem_euclid(spacing));
        while y < bottom {
            window.paint_quad(gpui::fill(
                Bounds::new(point(x, y), size(px(dot), px(dot))),
                color,
            ));
            y += px(spacing);
        }
        x += px(spacing);
    }
}

/// One floating group of icon buttons, vertically centered on the canvas's
/// right edge: zoom in/out, reset view, and expand/collapse. The group
/// occludes the canvas so a button press never starts a pan.
fn canvas_controls(
    id: &SharedString,
    viewport: WorkflowViewport,
    expanded: bool,
    theme: &Theme,
    on_zoom: ZoomHandler,
    on_pan: PanHandler,
    on_expand: ExpandHandler,
) -> gpui::Div {
    let button = |name: &'static str, path: &'static str, hint: SharedString| {
        let group = SharedString::from(format!("workflow-graph-{name}-{id}"));
        let hover_fg = theme.text;
        div()
            .id(group.clone())
            .group(group.clone())
            .debug_selector(move || format!("workflow-{name}"))
            .size(px(26.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.0))
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::ink(0.08)))
            .tooltip(move |_, cx| cx.new(|_| crate::popover::TextTooltip(hint.clone())).into())
            .child(
                crate::icons::icon(path)
                    .size(px(14.0))
                    .text_color(theme.text_muted)
                    .group_hover(group, move |style| style.text_color(hover_fg)),
            )
    };
    let zoom_in = on_zoom.clone();
    let zoom_out = on_zoom.clone();
    let fit_zoom = on_zoom;
    let (expand_icon, expand_hint) = if expanded {
        (
            crate::icons::COLLAPSE_ARROWS,
            t!("conversation_transcript.workflow.collapse"),
        )
    } else {
        (
            crate::icons::EXPAND_ARROWS,
            t!("conversation_transcript.workflow.expand"),
        )
    };
    let group = div()
        .id(SharedString::from(format!("workflow-graph-controls-{id}")))
        .occlude()
        .p(px(3.0))
        .flex()
        .flex_col()
        .gap(px(2.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface_raised)
        .shadow_sm()
        .child(
            button(
                "zoom-in",
                crate::icons::PLUS,
                t!("conversation_transcript.workflow.zoom_in").into(),
            )
            .on_click(move |_, window, cx| zoom_in(1.15, window, cx)),
        )
        .child(
            button(
                "zoom-out",
                crate::icons::MINUS,
                t!("conversation_transcript.workflow.zoom_out").into(),
            )
            .on_click(move |_, window, cx| zoom_out(1.0 / 1.15, window, cx)),
        )
        .child(
            button(
                "fit",
                crate::icons::RECENTER,
                t!("conversation_transcript.workflow.fit").into(),
            )
            .on_click(move |_, window, cx| {
                fit_zoom(1.0 / viewport.zoom, window, cx);
                on_pan(CanvasPanEvent::Reset, cx);
            }),
        )
        .child(div().mx(px(4.0)).h(px(1.0)).bg(theme.border))
        .child(
            button("expand", expand_icon, expand_hint.into())
                .on_click(move |_, _, cx| on_expand(cx)),
        );
    // A pass-through column (no id or listeners) centers the group, so the
    // canvas above and below it still pans.
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .right(px(12.0))
        .flex()
        .items_center()
        .child(group)
}

struct NodePresentation<'a> {
    theme: &'a Theme,
    identity: Option<AgentIdentity>,
    on_open: NodeOpenHandler,
    view: EntityId,
}

fn render_agent_node(
    ix: usize,
    node: &AgentNode,
    placement: Placement,
    layout: &GraphLayout,
    zoom: f32,
    presentation: NodePresentation<'_>,
    cx: &mut App,
) -> gpui::Stateful<gpui::Div> {
    let NodePresentation {
        theme,
        identity,
        on_open,
        view,
    } = presentation;
    let (status, status_color) = match node.status {
        Some(SubagentStatus::Running) => {
            (t!("conversation_transcript.workflow.running"), theme.busy)
        }
        Some(SubagentStatus::Done) => (t!("conversation_transcript.workflow.done"), theme.success),
        Some(SubagentStatus::Failed) => {
            (t!("conversation_transcript.workflow.failed"), theme.danger)
        }
        None => (
            t!("conversation_transcript.workflow.queued"),
            theme.text_faint,
        ),
    };
    let mut detail = vec![status.into_owned()];
    if let Some(phase) = &node.phase {
        detail.insert(0, phase.clone());
    }
    if let Some(tokens) = node.tokens.filter(|tokens| *tokens > 0) {
        detail.push(
            t!(
                "conversation_transcript.tool.tokens",
                count = crate::token_display::compact_tokens(tokens)
            )
            .into_owned(),
        );
    }
    let border = match node.status {
        Some(SubagentStatus::Running) => theme.busy.opacity(0.65),
        Some(SubagentStatus::Failed) => theme.danger.opacity(0.65),
        _ => theme.border,
    };
    let mood = match node.status {
        Some(SubagentStatus::Failed) => AvatarMood::Failed,
        Some(SubagentStatus::Done) => AvatarMood::Done,
        Some(SubagentStatus::Running) | None => AvatarMood::Running,
    };
    let avatar = identity
        .as_ref()
        .map(|identity| agent_avatar(identity, mood, view, cx));
    let name = identity.map(|identity| identity.name);
    let openable = node.subagent_ref.is_some();
    div()
        .id(SharedString::from(format!(
            "workflow-agent-{}",
            node.call_id
        )))
        .absolute()
        .left(px(placement.x))
        .top(px(placement.y))
        .w(px(layout.node_width))
        .h(px(layout.node_height))
        .p(px(10.0 * zoom))
        .rounded(px(8.0 * zoom))
        .bg(theme.surface_raised)
        .border_1()
        .border_color(border)
        .flex()
        .flex_col()
        .justify_between()
        .debug_selector(move || format!("workflow-agent-node-{ix}"))
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
        })
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(6.0 * zoom))
                .children(avatar)
                .when_some(name, |row, name| {
                    row.child(
                        div()
                            .flex_none()
                            .text_size(px(10.5 * zoom))
                            .text_color(theme.text_muted)
                            .child(name),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.0 * zoom))
                        .line_height(px(16.0 * zoom))
                        .text_color(theme.text)
                        .child(node.label.clone()),
                ),
        )
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(5.0 * zoom))
                .child(
                    div()
                        .flex_none()
                        .size(px(4.0 * zoom))
                        .rounded_full()
                        .bg(status_color),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(10.0 * zoom))
                        .line_height(px(13.0 * zoom))
                        .text_color(theme.text_muted)
                        .child(detail.join(" · ")),
                ),
        )
        .when(openable, |card| {
            card.cursor_pointer()
                .hover(move |style| style.bg(theme.surface_raised_hover))
                .on_click(move |_, window, cx| on_open(ix, window, cx))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(label: &str, phase: Option<&str>) -> AgentNode {
        AgentNode {
            call_id: label.into(),
            subagent_ref: None,
            label: label.into(),
            phase: phase.map(str::to_owned),
            status: None,
            tokens: None,
        }
    }

    #[test]
    fn graph_links_only_adjacent_phase_layers() {
        let graph = agent_graph(&[
            agent("a", Some("Find")),
            agent("b", Some("Find")),
            agent("check", Some("Verify")),
            agent("fix", Some("Find")),
        ])
        .unwrap();
        assert_eq!(graph.edges, [(0, 2), (1, 2), (2, 3)]);
        assert_eq!(graph.layers, [vec![0, 1], vec![2], vec![3]]);
    }

    #[test]
    fn unphased_agents_stay_independent_and_oversized_graphs_fall_back() {
        assert!(
            agent_graph(&[agent("a", None), agent("b", None)])
                .unwrap()
                .edges
                .is_empty()
        );
        assert!(agent_graph(&[]).is_none());
        let too_many = (0..=MAX_NODES)
            .map(|ix| agent(&format!("agent-{ix}"), None))
            .collect::<Vec<_>>();
        assert!(agent_graph(&too_many).is_none());
    }

    #[test]
    fn layout_centers_smaller_layers_and_scales_as_one_surface() {
        let graph = agent_graph(&[
            agent("a", Some("Find")),
            agent("b", Some("Find")),
            agent("check", Some("Verify")),
        ])
        .unwrap();
        let layout = graph.layout(1.0);
        assert_eq!(layout.placements.len(), 3);
        assert!(layout.placements[0].y < layout.placements[2].y);
        assert_eq!(layout.placements[0].x, layout.placements[1].x);
        assert!(layout.placements[2].x > layout.placements[0].x);
        assert_eq!(
            layout.height,
            GRAPH_PADDING * 2.0 + NODE_HEIGHT * 2.0 + NODE_GAP
        );

        let zoomed = graph.layout(1.5);
        assert_eq!(zoomed.width, layout.width * 1.5);
        assert_eq!(zoomed.height, layout.height * 1.5);
        assert_eq!(zoomed.node_width, layout.node_width * 1.5);
    }

    struct GraphPreview {
        graph: AgentGraph,
        identities: Vec<Option<AgentIdentity>>,
        on_open: NodeOpenHandler,
        on_expand: ExpandHandler,
        on_pan: PanHandler,
    }

    impl gpui::Render for GraphPreview {
        fn render(
            &mut self,
            _: &mut Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            let theme = Theme::of(cx).clone();
            render_agent_graph(
                &"preview".into(),
                &self.graph,
                WorkflowViewport::default(),
                &theme,
                GraphInteraction {
                    identities: self.identities.clone(),
                    on_open: self.on_open.clone(),
                    on_zoom: Rc::new(|_, _, _| {}),
                    on_pan: self.on_pan.clone(),
                    on_expand: self.on_expand.clone(),
                    dragging: false,
                    expanded: false,
                    view: cx.entity_id(),
                },
                cx,
            )
        }
    }

    #[gpui::test]
    fn native_graph_paints_real_nodes(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let graph = agent_graph(&[
            agent("a", Some("Find")),
            agent("b", Some("Find")),
            agent("check", Some("Verify")),
        ])
        .unwrap();
        let window = cx.add_window(|_, _| GraphPreview {
            graph,
            identities: Vec::new(),
            on_open: Rc::new(|_, _, _| {}),
            on_expand: Rc::new(|_| {}),
            on_pan: Rc::new(|_, _| {}),
        });
        let mut visual = gpui::VisualTestContext::from_window(*window, cx);

        assert!(visual.debug_bounds("workflow-graph").is_some());
        assert!(visual.debug_bounds("workflow-viewport").is_some());
        let first = visual.debug_bounds("workflow-agent-node-0").unwrap();
        let third = visual.debug_bounds("workflow-agent-node-2").unwrap();
        assert_eq!(first.size.width, px(NODE_WIDTH));
        assert_eq!(first.size.height, px(NODE_HEIGHT));
        assert!(third.origin.x > first.origin.x);
    }

    #[gpui::test]
    fn only_doc_bound_nodes_open_their_subagent(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let mut first = agent("a", Some("Find"));
        first.subagent_ref = Some("doc-a".into());
        first.status = Some(SubagentStatus::Done);
        let graph = agent_graph(&[first, agent("b", Some("Find"))]).unwrap();
        let opened = Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = opened.clone();
        let on_open =
            Rc::new(move |ix: usize, _: &mut Window, _: &mut App| sink.borrow_mut().push(ix));
        let mut registry = crate::transcript::agent_avatar::AgentIdentities::new(1, None);
        let identity = registry.identity_for(Some("a"), Some("doc-a"));
        let window = cx.add_window(|_, _| GraphPreview {
            graph,
            identities: vec![identity, None],
            on_open,
            on_expand: Rc::new(|_| {}),
            on_pan: Rc::new(|_, _| {}),
        });
        let mut visual = gpui::VisualTestContext::from_window(*window, cx);

        let first = visual.debug_bounds("workflow-agent-node-0").unwrap();
        visual.simulate_click(first.center(), Default::default());
        assert_eq!(opened.borrow().as_slice(), [0]);
        let second = visual.debug_bounds("workflow-agent-node-1").unwrap();
        visual.simulate_click(second.center(), Default::default());
        assert_eq!(opened.borrow().as_slice(), [0], "unbound nodes stay inert");
    }

    #[gpui::test]
    fn expand_is_a_canvas_control(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let graph = agent_graph(&[agent("a", Some("Find"))]).unwrap();
        let expanded = Rc::new(std::cell::Cell::new(false));
        let sink = expanded.clone();
        let window = cx.add_window(|_, _| GraphPreview {
            graph,
            identities: Vec::new(),
            on_open: Rc::new(|_, _, _| {}),
            on_expand: Rc::new(move |_| sink.set(true)),
            on_pan: Rc::new(|_, _| {}),
        });
        let mut visual = gpui::VisualTestContext::from_window(*window, cx);
        let button = visual.debug_bounds("workflow-expand").unwrap();
        visual.simulate_click(button.center(), Default::default());
        assert!(expanded.get());
    }

    #[gpui::test]
    fn blank_canvas_drags_pan_but_agent_clicks_do_not(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(Theme::default()));
        let graph = agent_graph(&[agent("a", Some("Find"))]).unwrap();
        let events = Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = events.clone();
        let on_pan = Rc::new(move |event: CanvasPanEvent, _: &mut App| {
            let name = match event {
                CanvasPanEvent::Start(_) => "start",
                CanvasPanEvent::Move(_) => "move",
                _ => return,
            };
            sink.borrow_mut().push(name);
        });
        let window = cx.add_window(|_, _| GraphPreview {
            graph,
            identities: Vec::new(),
            on_open: Rc::new(|_, _, _| {}),
            on_expand: Rc::new(|_| {}),
            on_pan,
        });
        let mut visual = gpui::VisualTestContext::from_window(*window, cx);
        let start = visual.debug_bounds("workflow-viewport").unwrap().center();
        visual.simulate_mouse_down(start, MouseButton::Left, Default::default());
        visual.simulate_mouse_move(
            start + point(px(30.0), px(18.0)),
            MouseButton::Left,
            Default::default(),
        );
        visual.simulate_mouse_up(start, MouseButton::Left, Default::default());
        let node = visual.debug_bounds("workflow-agent-node-0").unwrap();
        visual.simulate_click(node.center(), Default::default());
        assert_eq!(events.borrow().as_slice(), ["start", "move"]);
    }
}
