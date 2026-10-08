//! Session backend conformance: runs the shared cases against the
//! in-memory and JSONL backends (the Rust counterpart of the
//! `InMemorySessionRepo conformance` describe block and the storage-level
//! JSONL conformance).

mod common;

use std::sync::Arc;

use pi_core::agent::harness::env::nodejs::{NodeExecutionEnv, NodeExecutionEnvOptions};
use pi_core::agent::harness::session::memory::InMemorySessionRepo;
use pi_core::agent::harness::session::testing::{
    SessionBackendFixture, create_session_backend_conformance, jsonl_fixture,
    run_in_memory_conformance,
};

#[tokio::test]
async fn in_memory_repo_passes_the_conformance_cases() {
    let fixture = Arc::new(SessionBackendFixture::InMemory(InMemorySessionRepo::new()));
    let cases = create_session_backend_conformance(fixture);
    // 28 of the 30 TypeScript conformance cases; the two non-JSON payload
    // rejection cases are not representable with the strongly typed Rust
    // entries and records (see the notes in testing/mod.rs).
    assert_eq!(cases.len(), 28);
    for case in &cases {
        let joined = tokio::spawn((case.run)()).await;
        assert!(
            joined.is_ok(),
            "in-memory conformance case failed: {}/{}",
            case.group,
            case.name
        );
    }
}

#[tokio::test]
async fn jsonl_repo_passes_the_conformance_cases() {
    let root = common::create_temp_dir();
    let env: Arc<NodeExecutionEnv> = Arc::new(NodeExecutionEnv::new(NodeExecutionEnvOptions {
        cwd: root.to_string(),
        ..Default::default()
    }));
    let fixture = Arc::new(jsonl_fixture(&env, &root));
    let cases = create_session_backend_conformance(fixture);
    for case in &cases {
        let joined = tokio::spawn((case.run)()).await;
        assert!(
            joined.is_ok(),
            "jsonl conformance case failed: {}/{}",
            case.group,
            case.name
        );
    }
}

#[tokio::test]
async fn conformance_runner_helper_passes() {
    run_in_memory_conformance().await;
}
