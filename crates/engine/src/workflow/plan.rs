//! A run's declared plan (ADR-workflow-runs-declare-their-plan-up-front):
//! checked when the `Workflow` call arrives, then enforced as the script
//! starts agents, so the graph drawn before the run is the one it follows.

use std::collections::{HashMap, HashSet};

use holt_proto::{WorkflowPlan, WorkflowStep};

const MAX_STEPS: usize = 32;
const ID_CHARS: usize = 40;
const MAX_LOOP_ROUNDS: u32 = 10;

/// Check the plan's shape: unique ids, known references, no cycle through
/// `after`, and every loop pointing back upstream.
pub(crate) fn validate(plan: &WorkflowPlan) -> Result<(), String> {
    if plan.steps.is_empty() || plan.steps.len() > MAX_STEPS {
        return Err(format!("plan needs 1 to {MAX_STEPS} steps"));
    }
    let mut ids = HashSet::new();
    for step in &plan.steps {
        if step.id.trim().is_empty() || step.id.chars().count() > ID_CHARS {
            return Err(format!("plan step ids must be 1 to {ID_CHARS} characters"));
        }
        if step.title.trim().is_empty() {
            return Err(format!("plan step {:?} needs a title", step.id));
        }
        if !ids.insert(step.id.as_str()) {
            return Err(format!("plan step {:?} is declared twice", step.id));
        }
    }
    for step in &plan.steps {
        for dep in &step.after {
            if dep == &step.id || !ids.contains(dep.as_str()) {
                return Err(format!(
                    "plan step {:?} runs after unknown step {dep:?}",
                    step.id
                ));
            }
        }
    }
    let steps: HashMap<&str, &WorkflowStep> =
        plan.steps.iter().map(|s| (s.id.as_str(), s)).collect();
    // Depth-first over `after`: a step met again while still on the stack
    // closes a cycle.
    fn visit<'a>(
        id: &'a str,
        steps: &HashMap<&'a str, &'a WorkflowStep>,
        state: &mut HashMap<&'a str, bool>,
    ) -> Result<(), String> {
        match state.get(id) {
            Some(true) => return Ok(()),
            Some(false) => return Err(format!("plan steps form a cycle through {id:?}")),
            None => {}
        }
        state.insert(id, false);
        for dep in &steps[id].after {
            visit(dep, steps, state)?;
        }
        state.insert(id, true);
        Ok(())
    }
    let mut state = HashMap::new();
    for step in &plan.steps {
        visit(&step.id, &steps, &mut state)?;
    }
    for step in &plan.steps {
        let Some(back) = &step.r#loop else { continue };
        if back.max == 0 || back.max > MAX_LOOP_ROUNDS {
            return Err(format!(
                "plan step {:?} loops 1 to {MAX_LOOP_ROUNDS} times",
                step.id
            ));
        }
        if !upstream(&step.id, &steps).contains(back.to.as_str()) {
            return Err(format!(
                "plan step {:?} loops to {:?}, which is not upstream of it",
                step.id, back.to
            ));
        }
    }
    Ok(())
}

fn upstream<'a>(id: &str, steps: &HashMap<&'a str, &'a WorkflowStep>) -> HashSet<&'a str> {
    let mut seen = HashSet::new();
    let mut stack: Vec<&str> = steps[id].after.iter().map(String::as_str).collect();
    while let Some(next) = stack.pop() {
        if let Some((&key, step)) = steps.get_key_value(next)
            && seen.insert(key)
        {
            stack.extend(step.after.iter().map(String::as_str));
        }
    }
    seen
}

#[derive(Default)]
struct StepState {
    settled: usize,
    rounds: u32,
    /// The looping step's settled count when the latest round began.
    round_mark: usize,
}

/// Live enforcement: every `agent()` is admitted against the plan before it
/// starts, and reports back when it settles (success or failure alike).
pub(crate) struct PlanTracker {
    plan: WorkflowPlan,
    state: HashMap<String, StepState>,
}

impl PlanTracker {
    pub(crate) fn new(plan: WorkflowPlan) -> Self {
        let state = plan
            .steps
            .iter()
            .map(|s| (s.id.clone(), StepState::default()))
            .collect();
        Self { plan, state }
    }

    pub(crate) fn admit(&mut self, id: Option<&str>) -> Result<(), String> {
        let Some(id) = id else {
            return Err("agent() needs a step from the plan: agent(prompt, { step })".into());
        };
        let Some(step) = self.plan.steps.iter().find(|s| s.id == id) else {
            let known: Vec<_> = self.plan.steps.iter().map(|s| s.id.as_str()).collect();
            return Err(format!(
                "agent() step {id:?} is not in the plan (steps: {})",
                known.join(", ")
            ));
        };
        if let Some(dep) = step.after.iter().find(|dep| self.state[*dep].settled == 0) {
            return Err(format!(
                "agent() step {id:?} started before any {dep:?} agent finished; the plan runs it after {dep:?}"
            ));
        }
        // A start after the looping step settled more agents than at the
        // last round's start opens a new round.
        if let Some(looper) = self
            .plan
            .steps
            .iter()
            .find(|s| s.r#loop.as_ref().is_some_and(|l| l.to == id))
        {
            let max = looper.r#loop.as_ref().map_or(0, |l| l.max);
            let looped = self.state[&looper.id].settled;
            let state = self.state.get_mut(id).expect("declared step");
            if looped > state.round_mark {
                if state.rounds >= max {
                    return Err(format!(
                        "agent() step {id:?} ran past its loop: {:?} sends work back at most {max} times",
                        looper.id
                    ));
                }
                state.rounds += 1;
                state.round_mark = looped;
            }
        }
        Ok(())
    }

    pub(crate) fn settle(&mut self, id: &str) {
        if let Some(state) = self.state.get_mut(id) {
            state.settled += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(json: serde_json::Value) -> WorkflowPlan {
        serde_json::from_value(json).unwrap()
    }

    fn review_loop() -> WorkflowPlan {
        plan(serde_json::json!({"steps": [
            {"id": "write", "title": "Write"},
            {"id": "review", "title": "Review", "after": ["write"]},
            {"id": "revise", "title": "Revise", "after": ["review"], "loop": {"to": "review", "max": 1}},
        ]}))
    }

    #[test]
    fn validation_rejects_malformed_plans() {
        assert!(validate(&review_loop()).is_ok());
        let cases = [
            (serde_json::json!({"steps": []}), "1 to 32 steps"),
            (
                serde_json::json!({"steps": [{"id": "a", "title": "A"}, {"id": "a", "title": "B"}]}),
                "declared twice",
            ),
            (
                serde_json::json!({"steps": [{"id": "a", "title": "A", "after": ["b"]}]}),
                "unknown step",
            ),
            (
                serde_json::json!({"steps": [
                    {"id": "a", "title": "A", "after": ["b"]},
                    {"id": "b", "title": "B", "after": ["a"]},
                ]}),
                "cycle",
            ),
            (
                serde_json::json!({"steps": [
                    {"id": "a", "title": "A"},
                    {"id": "b", "title": "B", "loop": {"to": "a", "max": 1}},
                ]}),
                "not upstream",
            ),
            (
                serde_json::json!({"steps": [{"id": "a", "title": " "}]}),
                "needs a title",
            ),
        ];
        for (json, expected) in cases {
            let error = validate(&plan(json.clone())).unwrap_err();
            assert!(error.contains(expected), "{json}: {error}");
        }
        assert!(
            serde_json::from_value::<WorkflowPlan>(
                serde_json::json!({"steps": [{"id": "a", "title": "A", "phase": "x"}]})
            )
            .is_err(),
            "unknown step fields are refused"
        );
    }

    #[test]
    fn steps_wait_for_a_settled_upstream_agent() {
        let mut tracker = PlanTracker::new(review_loop());
        assert!(tracker.admit(None).unwrap_err().contains("needs a step"));
        assert!(
            tracker
                .admit(Some("draft"))
                .unwrap_err()
                .contains("not in the plan")
        );
        assert!(
            tracker
                .admit(Some("review"))
                .unwrap_err()
                .contains("before any \"write\"")
        );
        tracker.admit(Some("write")).unwrap();
        tracker.settle("write");
        tracker.admit(Some("review")).unwrap();
        tracker.admit(Some("review")).unwrap();
    }

    #[test]
    fn a_loop_admits_its_declared_rounds_only() {
        let mut tracker = PlanTracker::new(review_loop());
        tracker.admit(Some("write")).unwrap();
        tracker.settle("write");
        // Round 0: two reviewers, then a revision.
        for _ in 0..2 {
            tracker.admit(Some("review")).unwrap();
            tracker.settle("review");
        }
        tracker.admit(Some("revise")).unwrap();
        tracker.settle("revise");
        // The loop's one round: both reviewers belong to it.
        tracker.admit(Some("review")).unwrap();
        tracker.admit(Some("review")).unwrap();
        tracker.settle("review");
        tracker.admit(Some("revise")).unwrap();
        tracker.settle("revise");
        assert!(
            tracker
                .admit(Some("review"))
                .unwrap_err()
                .contains("past its loop")
        );
    }
}
