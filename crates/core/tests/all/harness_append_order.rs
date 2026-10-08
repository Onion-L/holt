//! Regression: `NodeExecutionEnv::append_file` must resolve only after the
//! write(2) reached the OS. `tokio::fs::File::write_all`
//! returns as soon as the bytes are copied into tokio's internal buffer —
//! the blocking write runs afterwards — so on a fresh handle the future
//! could resolve while the bytes were still queued. That let two sequential
//! appends land out of order in the file (observed on CI as a session JSONL
//! with lines swapped relative to their seq order) and let a read straight
//! after an append miss it. The env flushes the handle before resolving; this
//! test asserts the visible consequence: bytes appended by a resolved call
//! are immediately readable. `write_file` (one-shot `tokio::fs::write`,
//! which awaits the blocking write) is checked alongside as a guard.

use std::sync::Arc;

use pi_core::agent::harness::env::nodejs::{NodeExecutionEnv, NodeExecutionEnvOptions};
use pi_core::agent::harness::types::{FileSystem, WriteContent};

use crate::common;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn appends_and_writes_are_readable_as_soon_as_they_resolve() {
    let root = common::create_temp_dir();
    let env: Arc<NodeExecutionEnv> = Arc::new(NodeExecutionEnv::new(NodeExecutionEnvOptions {
        cwd: root.to_string(),
        ..Default::default()
    }));

    // Rolling files keep each read small while still exercising thousands
    // of append-then-read round trips (the reorder window is tiny and
    // scheduling-dependent; volume is what makes it observable).
    const ROUNDS: usize = 60;
    const LINES_PER_FILE: usize = 400;

    for round in 0..ROUNDS {
        let append_path = format!("append-{round}.txt");
        let mut expected = String::new();
        for line in 0..LINES_PER_FILE {
            let text = format!("l{line}\n");
            env.append_file(&append_path, &WriteContent::Text(text.clone()), None)
                .await
                .unwrap();
            expected.push_str(&text);
            let read = env.read_text_file(&append_path, None).await.unwrap();
            assert_eq!(read, expected, "append resolved before its bytes landed");
        }

        let write_path = format!("write-{round}.txt");
        let text = format!("round {round}\n");
        env.write_file(&write_path, &WriteContent::Text(text.clone()), None)
            .await
            .unwrap();
        let read = env.read_text_file(&write_path, None).await.unwrap();
        assert_eq!(read, text, "write resolved before its bytes landed");
    }
}
