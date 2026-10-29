//! Read-only, native GPUI rendering of a workflow's actual agent invocations.
//! The transcript owns the persisted inputs; this module owns only graph
//! topology, deterministic layout, and paint.

use std::rc::Rc;

use gpui::{
    AnyElement, App, EntityId, PathBuilder, SharedString, Window, canvas, div, point, prelude::*,
    px,
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

pub(crate) type ZoomHandler = Rc<dyn Fn(f32, &mut Window, &mut App)>;
pub(crate) type NodeOpenHandler = Rc<dyn Fn(usize, &mut Window, &mut App)>;

pub(crate) struct GraphInteraction {
    pub identities: Vec<Option<AgentIdentity>>,
    pub on_open: NodeOpenHandler,
    pub on_zoom: ZoomHandler,
    pub view: EntityId,
}

/// Native node/edge canvas. Zoom scales layout and typography together;
/// horizontal overflow pans in GPUI's own scroll container.
pub(crate) fn render_agent_graph(
    id: &SharedString,
    graph: &AgentGraph,
    zoom: f32,
    theme: &Theme,
    interaction: GraphInteraction,
    cx: &mut App,
) -> AnyElement {
    let GraphInteraction {
        identities,
        on_open,
        on_zoom,
        view,
    } = interaction;
    let layout = graph.layout(zoom);
    let edges = graph.edges.clone();
    let edge_layout = layout.clone();
    let arrow_scale = zoom;
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
                arrow.line_to(point(
                    end.x - px(5.0 * arrow_scale),
                    end.y - px(3.0 * arrow_scale),
                ));
                arrow.line_to(point(
                    end.x - px(5.0 * arrow_scale),
                    end.y + px(3.0 * arrow_scale),
                ));
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
    let canvas_body = div()
        .relative()
        .w(px(layout.width))
        .h(px(layout.height))
        .child(edge_canvas)
        .children(nodes)
        .mx_auto();
    let mut body = div()
        .id(SharedString::from(format!("workflow-graph-scroll-{id}")))
        .overflow_x_scroll()
        .w_full()
        .child(canvas_body);
    body.style().restrict_scroll_to_axis = Some(true);

    let controls = graph_controls(id, zoom, theme, on_zoom.clone());
    div()
        .relative()
        .w_full()
        .rounded(px(10.0))
        .bg(crate::theme::ink(0.025))
        .border_1()
        .border_color(theme.border)
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
        .child(controls)
        .child(body)
        .into_any_element()
}

fn graph_controls(id: &SharedString, zoom: f32, theme: &Theme, on_zoom: ZoomHandler) -> gpui::Div {
    let button = |name: &str, label: SharedString, factor: f32| {
        let on_zoom = on_zoom.clone();
        div()
            .id(SharedString::from(format!("workflow-graph-{name}-{id}")))
            .h(px(20.0))
            .min_w(px(20.0))
            .px(px(5.0))
            .rounded(px(5.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::ink(0.08)))
            .text_size(px(11.0))
            .child(label)
            .on_click(move |_, window, cx| on_zoom(factor, window, cx))
    };
    div()
        .h(px(28.0))
        .px(px(6.0))
        .flex()
        .items_center()
        .justify_end()
        .gap(px(3.0))
        .text_color(theme.text_muted)
        .child(button("minus", "−".into(), 1.0 / 1.15))
        .child(
            div()
                .min_w(px(36.0))
                .text_center()
                .text_size(px(10.0))
                .child(SharedString::from(format!("{:.0}%", zoom * 100.0))),
        )
        .child(button("plus", "+".into(), 1.15))
        .when(zoom != 1.0, |row| {
            row.child(button(
                "fit",
                t!("conversation_transcript.markdown.fit").into(),
                1.0 / zoom,
            ))
        })
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
                1.0,
                &theme,
                GraphInteraction {
                    identities: self.identities.clone(),
                    on_open: self.on_open.clone(),
                    on_zoom: std::rc::Rc::new(|_, _, _| {}),
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
            on_open: std::rc::Rc::new(|_, _, _| {}),
        });
        let mut visual = gpui::VisualTestContext::from_window(*window, cx);

        assert!(visual.debug_bounds("workflow-graph").is_some());
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
        let opened = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = opened.clone();
        let on_open = std::rc::Rc::new(move |ix: usize, _: &mut Window, _: &mut App| {
            sink.borrow_mut().push(ix)
        });
        let mut registry = crate::transcript::agent_avatar::AgentIdentities::new(1, None);
        let identity = registry.identity_for(Some("a"), Some("doc-a"));
        let window = cx.add_window(|_, _| GraphPreview {
            graph,
            identities: vec![identity, None],
            on_open,
        });
        let mut visual = gpui::VisualTestContext::from_window(*window, cx);

        let first = visual.debug_bounds("workflow-agent-node-0").unwrap();
        visual.simulate_click(first.center(), Default::default());
        assert_eq!(opened.borrow().as_slice(), [0]);
        let second = visual.debug_bounds("workflow-agent-node-1").unwrap();
        visual.simulate_click(second.center(), Default::default());
        assert_eq!(opened.borrow().as_slice(), [0], "unbound nodes stay inert");
    }
}
