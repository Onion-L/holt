//! A planned workflow's step graph (ADR-workflow-runs-declare-their-plan-up-front):
//! every declared step is a card from the moment the run launches, its
//! agents fill in as the script starts them, and the edges are the plan's
//! own `after` links plus each loop drawn as a return arc.

use std::collections::HashMap;

use gpui::{
    App, EntityId, MouseButton, PathBuilder, SharedString, canvas, div, point, prelude::*, px,
};
use holt_doc::SubagentStatus;
use holt_proto::WorkflowPlan;

use super::{AgentNode, LinkFlow, NodeOpenHandler, StepToggleHandler, WORKFLOW_FLOW, paint_link};
use crate::i18n::t;
use crate::motion;
use crate::theme::Theme;
use crate::transcript::agent_avatar::{AgentIdentity, AvatarMood, agent_avatar};

/// Agents listed on a folded step card; the rest sit behind "N more".
pub(crate) const VISIBLE_AGENTS: usize = 5;
const CARD_WIDTH: f32 = 252.0;
const HEADER_HEIGHT: f32 = 42.0;
const AGENT_ROW: f32 = 32.0;
const CARD_PAD: f32 = 7.0;
const EMPTY_HEIGHT: f32 = 34.0;
const LAYER_GAP: f32 = 54.0;
const CARD_GAP: f32 = 16.0;
const PADDING: f32 = 28.0;
/// Room under the lowest card for loop arcs.
const LOOP_LANE: f32 = 28.0;

/// A plan with the agents its run has started so far, grouped by step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanGraph {
    plan: WorkflowPlan,
    nodes: Vec<AgentNode>,
    /// Per step (plan order): indices into `nodes`.
    members: Vec<Vec<usize>>,
    /// Per step: its layer, the longest `after` chain above it.
    layers: Vec<usize>,
}

/// `None` when the plan does not hold together (a run from a build whose
/// plan check differed); the row then falls back to the agent graph.
pub(crate) fn plan_graph(plan: WorkflowPlan, agents: &[AgentNode]) -> Option<PlanGraph> {
    let index: HashMap<&str, usize> = plan
        .steps
        .iter()
        .enumerate()
        .map(|(ix, step)| (step.id.as_str(), ix))
        .collect();
    if plan.steps.is_empty() || index.len() != plan.steps.len() {
        return None;
    }
    // A rejected call keeps the model's raw plan, so a loop may name no step.
    if plan
        .steps
        .iter()
        .filter_map(|step| step.r#loop.as_ref())
        .any(|back| !index.contains_key(back.to.as_str()))
    {
        return None;
    }
    let mut members = vec![Vec::new(); plan.steps.len()];
    for (ix, agent) in agents.iter().enumerate() {
        members[*index.get(agent.phase.as_deref()?)?].push(ix);
    }
    // Longest path over `after`, relaxed once per step: a cycle or an
    // unknown reference leaves it unsettled.
    let mut layers = vec![0usize; plan.steps.len()];
    for _ in 0..plan.steps.len() {
        for (ix, step) in plan.steps.iter().enumerate() {
            for dep in &step.after {
                layers[ix] = layers[ix].max(layers[*index.get(dep.as_str())?] + 1);
            }
        }
    }
    if layers.iter().any(|layer| *layer >= plan.steps.len()) {
        return None;
    }
    Some(PlanGraph {
        plan,
        nodes: agents.to_vec(),
        members,
        layers,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Card {
    x: f32,
    y: f32,
    height: f32,
}

struct Layout {
    width: f32,
    height: f32,
    cards: Vec<Card>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepState {
    Pending,
    Running,
    Done,
    Failed,
}

impl PlanGraph {
    pub(crate) fn nodes(&self) -> &[AgentNode] {
        &self.nodes
    }

    /// The agents a card lists: all when open, else the first few.
    fn visible(&self, step: usize, open: bool) -> &[usize] {
        let members = &self.members[step];
        if open {
            members
        } else {
            &members[..members.len().min(VISIBLE_AGENTS)]
        }
    }

    fn state(&self, step: usize) -> StepState {
        let statuses = self.members[step].iter().map(|ix| self.nodes[*ix].status);
        let (mut any, mut running, mut failed) = (false, false, false);
        for status in statuses {
            any = true;
            running |= matches!(status, Some(SubagentStatus::Running) | None);
            failed |= status == Some(SubagentStatus::Failed);
        }
        match (any, running, failed) {
            (false, _, _) => StepState::Pending,
            (_, true, _) => StepState::Running,
            (_, _, true) => StepState::Failed,
            _ => StepState::Done,
        }
    }

    fn layout(&self, zoom: f32, open: &dyn Fn(usize) -> bool) -> Layout {
        let layer_count = self.layers.iter().max().map_or(0, |max| max + 1);
        let card_height = |step: usize| {
            let members = &self.members[step];
            let rows =
                self.visible(step, open(step)).len() + usize::from(members.len() > VISIBLE_AGENTS);
            let body = if rows == 0 {
                EMPTY_HEIGHT
            } else {
                rows as f32 * AGENT_ROW
            };
            (HEADER_HEIGHT + body + CARD_PAD) * zoom
        };
        let mut columns: Vec<Vec<usize>> = vec![Vec::new(); layer_count];
        for (step, layer) in self.layers.iter().enumerate() {
            columns[*layer].push(step);
        }
        let column_height = |column: &[usize]| {
            column.iter().map(|step| card_height(*step)).sum::<f32>()
                + column.len().saturating_sub(1) as f32 * CARD_GAP * zoom
        };
        let tallest = columns
            .iter()
            .map(|column| column_height(column))
            .fold(0.0, f32::max);
        let has_loop = self.plan.steps.iter().any(|step| step.r#loop.is_some());
        let mut cards = vec![
            Card {
                x: 0.0,
                y: 0.0,
                height: 0.0
            };
            self.plan.steps.len()
        ];
        for (layer, column) in columns.iter().enumerate() {
            let x = (PADDING + layer as f32 * (CARD_WIDTH + LAYER_GAP)) * zoom;
            let mut y = PADDING * zoom + (tallest - column_height(column)) / 2.0;
            for step in column {
                let height = card_height(*step);
                cards[*step] = Card { x, y, height };
                y += height + CARD_GAP * zoom;
            }
        }
        Layout {
            width: (PADDING * 2.0
                + layer_count as f32 * CARD_WIDTH
                + layer_count.saturating_sub(1) as f32 * LAYER_GAP)
                * zoom,
            height: tallest + (PADDING * 2.0 + if has_loop { LOOP_LANE } else { 0.0 }) * zoom,
            cards,
        }
    }
}

pub(super) struct Presentation<'a> {
    pub theme: &'a Theme,
    pub identities: &'a [Option<AgentIdentity>],
    pub on_open: NodeOpenHandler,
    pub open_steps: &'a [SharedString],
    pub on_toggle: StepToggleHandler,
    pub view: EntityId,
}

pub(super) fn render(
    graph: &PlanGraph,
    zoom: f32,
    presentation: Presentation<'_>,
    cx: &mut App,
) -> gpui::Div {
    let Presentation {
        theme,
        identities,
        on_open,
        open_steps,
        on_toggle,
        view,
    } = presentation;
    let is_open = |step: usize| {
        open_steps
            .iter()
            .any(|id| id.as_ref() == graph.plan.steps[step].id)
    };
    let layout = graph.layout(zoom, &is_open);
    let index: HashMap<&str, usize> = graph
        .plan
        .steps
        .iter()
        .enumerate()
        .map(|(ix, step)| (step.id.as_str(), ix))
        .collect();
    let mut links = Vec::new();
    let mut loops = Vec::new();
    for (ix, step) in graph.plan.steps.iter().enumerate() {
        for dep in &step.after {
            links.push((
                layout.cards[index[dep.as_str()]],
                layout.cards[ix],
                graph.state(ix) == StepState::Running,
            ));
        }
        if let Some(back) = &step.r#loop {
            loops.push((layout.cards[ix], layout.cards[index[back.to.as_str()]]));
        }
    }
    let edge_color = theme.text_muted.opacity(0.4);
    let loop_color = theme.accent.opacity(0.45);
    let flow = links
        .iter()
        .any(|(_, _, active)| *active)
        .then(|| LinkFlow {
            phase: motion::pulse_delta(&WORKFLOW_FLOW, view, cx),
            color: theme.busy,
        });
    let floor = layout.height - LOOP_LANE * zoom * 0.5;
    let edges = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let at = |x: f32, y: f32| point(bounds.origin.x + px(x), bounds.origin.y + px(y));
            for (from, to, active) in &links {
                let anchor = HEADER_HEIGHT * zoom / 2.0;
                paint_link(
                    window,
                    at(from.x + CARD_WIDTH * zoom, from.y + anchor),
                    at(to.x, to.y + anchor),
                    zoom,
                    if *active {
                        flow.map_or(edge_color, |flow| flow.color.opacity(0.55))
                    } else {
                        edge_color
                    },
                    if *active { flow } else { None },
                );
            }
            // A loop leaves the bottom of its step, runs along the lane
            // under every card, and climbs back into the step it reopens.
            for (from, to) in &loops {
                let start = at(from.x + CARD_WIDTH * zoom / 2.0, from.y + from.height);
                let end = at(to.x + CARD_WIDTH * zoom / 2.0, to.y + to.height);
                let low = bounds.origin.y + px(floor);
                let mut builder = PathBuilder::stroke(px(1.0));
                builder.move_to(start);
                builder.cubic_bezier_to(end, point(start.x, low), point(end.x, low));
                if let Ok(path) = builder.build() {
                    window.paint_path(path, loop_color);
                }
                let mut arrow = PathBuilder::fill();
                arrow.move_to(end);
                arrow.line_to(point(end.x - px(3.0 * zoom), end.y + px(5.0 * zoom)));
                arrow.line_to(point(end.x + px(3.0 * zoom), end.y + px(5.0 * zoom)));
                arrow.close();
                if let Ok(path) = arrow.build() {
                    window.paint_path(path, loop_color);
                }
            }
        },
    )
    .absolute()
    .inset_0();
    let loop_labels = graph
        .plan
        .steps
        .iter()
        .enumerate()
        .filter_map(|(ix, step)| {
            let back = step.r#loop.as_ref()?;
            let from = layout.cards[ix];
            let to = layout.cards[index[back.to.as_str()]];
            let mid = (from.x + to.x + CARD_WIDTH * zoom) / 2.0;
            Some(
                div()
                    .absolute()
                    .left(px(mid - 20.0 * zoom))
                    .top(px(floor - 7.0 * zoom))
                    .w(px(40.0 * zoom))
                    .flex()
                    .justify_center()
                    .text_size(px(10.0 * zoom))
                    .text_color(theme.accent)
                    .bg(theme.surface)
                    .child(SharedString::from(t!(
                        "conversation_transcript.workflow.loop",
                        max = back.max
                    ))),
            )
        });
    let cards = graph
        .plan
        .steps
        .iter()
        .enumerate()
        .map(|(step, _)| {
            render_card(
                graph,
                step,
                layout.cards[step],
                zoom,
                is_open(step),
                CardPresentation {
                    theme,
                    identities,
                    on_open: on_open.clone(),
                    on_toggle: on_toggle.clone(),
                    view,
                },
                cx,
            )
        })
        .collect::<Vec<_>>();
    div()
        .w(px(layout.width))
        .h(px(layout.height))
        .child(edges)
        .children(cards)
        .children(loop_labels)
}

struct CardPresentation<'a> {
    theme: &'a Theme,
    identities: &'a [Option<AgentIdentity>],
    on_open: NodeOpenHandler,
    on_toggle: StepToggleHandler,
    view: EntityId,
}

fn render_card(
    graph: &PlanGraph,
    step: usize,
    card: Card,
    zoom: f32,
    open: bool,
    presentation: CardPresentation<'_>,
    cx: &mut App,
) -> gpui::Stateful<gpui::Div> {
    let CardPresentation {
        theme,
        identities,
        on_open,
        on_toggle,
        view,
    } = presentation;
    let declared = &graph.plan.steps[step];
    let members = &graph.members[step];
    let state = graph.state(step);
    let (dot, border) = match state {
        StepState::Pending => (theme.text_faint, theme.border_strong),
        StepState::Running => (theme.busy, theme.busy.opacity(0.55)),
        StepState::Done => (theme.success, theme.success.opacity(0.20)),
        StepState::Failed => (theme.danger, theme.danger.opacity(0.55)),
    };
    let settled = members
        .iter()
        .filter(|ix| {
            matches!(
                graph.nodes[**ix].status,
                Some(SubagentStatus::Done) | Some(SubagentStatus::Failed)
            )
        })
        .count();
    let total = members.len().max(declared.agents.unwrap_or(0) as usize);
    let count = if total == 0 {
        String::new()
    } else {
        format!("{settled}/{total}")
    };
    let header = div()
        .h(px(HEADER_HEIGHT * zoom))
        .px(px(11.0 * zoom))
        .flex()
        .items_center()
        .gap(px(7.0 * zoom))
        .child(
            div()
                .flex_none()
                .size(px(6.0 * zoom))
                .rounded_full()
                .bg(dot)
                .when(state == StepState::Running, |dot| {
                    let phase = motion::pulse_delta(&motion::HOLT_PULSE, view, cx);
                    dot.opacity(0.65 + 0.35 * motion::pulse_wave(phase))
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(12.5 * zoom))
                .text_color(if state == StepState::Pending {
                    theme.text_muted
                } else {
                    theme.text
                })
                .child(SharedString::from(declared.title.clone())),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(11.0 * zoom))
                .font_family(theme.font_mono.clone())
                .text_color(theme.text_faint)
                .child(count),
        );
    let rows = graph.visible(step, open).iter().map(|ix| {
        render_agent_row(
            *ix,
            &graph.nodes[*ix],
            zoom,
            theme,
            identities.get(*ix).cloned().flatten(),
            on_open.clone(),
            view,
            cx,
        )
    });
    let body = div()
        .px(px(5.0 * zoom))
        .flex()
        .flex_col()
        .when(members.is_empty(), |body| {
            body.h(px(EMPTY_HEIGHT * zoom))
                .px(px(10.0 * zoom))
                .text_size(px(11.0 * zoom))
                .text_color(theme.text_faint)
                .child(SharedString::from(t!(
                    "conversation_transcript.workflow.step_waiting"
                )))
        })
        .children(rows)
        .when(members.len() > VISIBLE_AGENTS, |body| {
            let id: SharedString = declared.id.clone().into();
            let label = if open {
                t!("conversation_transcript.workflow.show_fewer").into_owned()
            } else {
                t!(
                    "conversation_transcript.workflow.more",
                    count = members.len() - VISIBLE_AGENTS
                )
                .into_owned()
            };
            body.child(
                div()
                    .id(SharedString::from(format!(
                        "workflow-step-more-{}",
                        declared.id
                    )))
                    .debug_selector(|| "workflow-step-more".to_string())
                    .h(px(AGENT_ROW * zoom))
                    .px(px(6.0 * zoom))
                    .flex()
                    .items_center()
                    .rounded(px(5.0 * zoom))
                    .cursor_pointer()
                    .text_size(px(11.0 * zoom))
                    .text_color(theme.text_muted)
                    .hover(|style| style.bg(crate::theme::ink(0.06)))
                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                    })
                    .on_click(move |_, _, cx| on_toggle(id.clone(), cx))
                    .child(SharedString::from(label)),
            )
        });
    div()
        .id(SharedString::from(format!("workflow-step-{}", declared.id)))
        .debug_selector(move || format!("workflow-step-{step}"))
        .absolute()
        .left(px(card.x))
        .top(px(card.y))
        .w(px(CARD_WIDTH * zoom))
        .h(px(card.height))
        .rounded(px(10.0 * zoom))
        .bg(theme.surface_overlay)
        .border_1()
        .border_color(border)
        .when(state == StepState::Pending, |card| card.border_dashed())
        .flex()
        .flex_col()
        .child(header)
        .child(body)
}

#[allow(clippy::too_many_arguments)]
fn render_agent_row(
    ix: usize,
    node: &AgentNode,
    zoom: f32,
    theme: &Theme,
    identity: Option<AgentIdentity>,
    on_open: NodeOpenHandler,
    view: EntityId,
    cx: &mut App,
) -> gpui::Stateful<gpui::Div> {
    let mood = match node.status {
        Some(SubagentStatus::Failed) => AvatarMood::Failed,
        Some(SubagentStatus::Done) => AvatarMood::Done,
        Some(SubagentStatus::Running) | None => AvatarMood::Running,
    };
    let status_color = match node.status {
        Some(SubagentStatus::Running) => theme.busy,
        Some(SubagentStatus::Done) => theme.success,
        Some(SubagentStatus::Failed) => theme.danger,
        None => theme.text_faint,
    };
    let avatar = identity
        .as_ref()
        .map(|identity| agent_avatar(identity, mood, view, cx));
    let openable = node.subagent_ref.is_some();
    div()
        .id(SharedString::from(format!(
            "workflow-agent-{}",
            node.call_id
        )))
        .debug_selector(move || format!("workflow-agent-node-{ix}"))
        .h(px(AGENT_ROW * zoom))
        .px(px(7.0 * zoom))
        .flex()
        .items_center()
        .gap(px(7.0 * zoom))
        .rounded(px(5.0 * zoom))
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
        })
        .children(avatar)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(11.5 * zoom))
                .text_color(theme.text)
                .child(node.label.clone()),
        )
        .when_some(node.tokens.filter(|tokens| *tokens > 0), |row, tokens| {
            row.child(
                div()
                    .flex_none()
                    .text_size(px(10.0 * zoom))
                    .text_color(theme.text_faint)
                    .child(crate::token_display::compact_tokens(tokens)),
            )
        })
        .child(
            div()
                .flex_none()
                .size(px(5.0 * zoom))
                .rounded_full()
                .bg(status_color)
                .when(node.status == Some(SubagentStatus::Running), |dot| {
                    let phase = motion::pulse_delta(&motion::HOLT_PULSE, view, cx);
                    dot.opacity(0.65 + 0.35 * motion::pulse_wave(phase))
                }),
        )
        .when(openable, |row| {
            row.cursor_pointer()
                .hover(|style| style.bg(crate::theme::ink(0.06)))
                .on_click(move |_, window, cx| on_open(ix, window, cx))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn research_plan() -> WorkflowPlan {
        serde_json::from_value(serde_json::json!({"steps": [
            {"id": "research", "title": "Research", "agents": 2},
            {"id": "verify", "title": "Verify", "after": ["research"]},
            {"id": "write", "title": "Write", "after": ["verify"]},
            {"id": "review", "title": "Review", "after": ["write"]},
            {"id": "revise", "title": "Revise", "after": ["review"], "loop": {"to": "review", "max": 2}},
        ]}))
        .unwrap()
    }

    pub(crate) fn agent(label: &str, step: &str, status: Option<SubagentStatus>) -> AgentNode {
        AgentNode {
            call_id: label.into(),
            subagent_ref: None,
            label: label.into(),
            phase: Some(step.into()),
            status,
            tokens: None,
        }
    }

    #[test]
    fn every_declared_step_is_placed_before_any_agent_runs() {
        let graph = plan_graph(research_plan(), &[]).unwrap();
        assert_eq!(graph.layers, [0, 1, 2, 3, 4]);
        assert!(graph.members.iter().all(Vec::is_empty));
        assert!((0..5).all(|step| graph.state(step) == StepState::Pending));
        let layout = graph.layout(1.0, &|_| false);
        assert!(layout.cards.windows(2).all(|pair| pair[1].x > pair[0].x));
    }

    #[test]
    fn agents_join_their_step_and_set_its_state() {
        let graph = plan_graph(
            research_plan(),
            &[
                agent("a", "research", Some(SubagentStatus::Done)),
                agent("b", "research", Some(SubagentStatus::Failed)),
                agent("v", "verify", Some(SubagentStatus::Running)),
            ],
        )
        .unwrap();
        assert_eq!(graph.members[0], [0, 1]);
        assert_eq!(graph.state(0), StepState::Failed);
        assert_eq!(graph.state(1), StepState::Running);
        assert_eq!(graph.state(2), StepState::Pending);
        // An agent outside the plan means the record does not match it.
        assert!(plan_graph(research_plan(), &[agent("x", "draft", None)]).is_none());
    }

    #[test]
    fn a_loop_to_an_unknown_step_falls_back() {
        let plan = serde_json::from_value(serde_json::json!({"steps": [
            {"id": "write", "title": "Write", "loop": {"to": "draft", "max": 2}},
        ]}))
        .unwrap();
        assert!(plan_graph(plan, &[]).is_none());
    }

    #[test]
    fn a_wide_step_folds_until_opened() {
        let agents: Vec<_> = (0..9)
            .map(|ix| agent(&format!("v{ix}"), "verify", None))
            .collect();
        let graph = plan_graph(research_plan(), &agents).unwrap();
        assert_eq!(graph.visible(1, false).len(), VISIBLE_AGENTS);
        assert_eq!(graph.visible(1, true).len(), 9);
        let folded = graph.layout(1.0, &|_| false).cards[1].height;
        let open = graph.layout(1.0, &|step| step == 1).cards[1].height;
        assert_eq!(open - folded, 4.0 * AGENT_ROW);
    }
}
