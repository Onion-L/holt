use pi_core::agent::node::{
    Agent, AgentHarness, BashToolInput, Entry, InMemoryTelemetryContext, NodeExecutionEnv,
    SessionSearchHit, uuidv7,
};

#[test]
fn node_entry_point_reexports_the_agent_root_surface() {
    fn public_type<T>() {}

    public_type::<Agent>();
    public_type::<AgentHarness>();
    public_type::<BashToolInput>();
    public_type::<Entry>();
    public_type::<InMemoryTelemetryContext>();
    public_type::<NodeExecutionEnv>();
    public_type::<SessionSearchHit>();
    let id = uuidv7();
    assert_eq!(id.len(), 36);
}
