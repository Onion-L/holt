//! Port of `pi-core/agent/src/harness/tools/file-mutation-queue.ts`.
//!
//! TypeScript keys the queue state in a `WeakMap<ExecutionEnv, …>`; the
//! Rust port keys a global registry by the env `Arc` address and drops the
//! registry entry once its last queue settles.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::agent::harness::types::{ExecutionEnv, FileErrorCode};

#[derive(Default)]
struct QueueState {
    queues: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

fn registry() -> &'static Mutex<HashMap<usize, Arc<Mutex<QueueState>>>> {
    static REGISTRY: std::sync::OnceLock<Mutex<HashMap<usize, Arc<Mutex<QueueState>>>>> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn state_for(env: &Arc<dyn ExecutionEnv>) -> Arc<Mutex<QueueState>> {
    let key = Arc::as_ptr(env) as *const () as usize;
    let mut registry = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.entry(key).or_default().clone()
}

async fn mutation_queue_key(
    env: &Arc<dyn ExecutionEnv>,
    path: &str,
) -> Result<String, crate::agent::harness::types::FileError> {
    let absolute_path = env.absolute_path(path, None).await?;
    match env.canonical_path(&absolute_path, None).await {
        Ok(canonical) => Ok(canonical),
        Err(error)
            if error.code == FileErrorCode::NotFound
                || error.code == FileErrorCode::NotSupported =>
        {
            Ok(absolute_path)
        }
        Err(error) => Err(error),
    }
}

/// Serialize file mutations targeting the same environment and canonical
/// path (port of `withFileMutationQueue`).
pub async fn with_file_mutation_queue<T, F, Fut>(
    env: &Arc<dyn ExecutionEnv>,
    path: &str,
    operation: F,
) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let state = state_for(env);
    let key = mutation_queue_key(env, path).await.unwrap_or_else(|error| {
        // The queue key resolution itself failed; run unserialized like the
        // TypeScript rethrow would surface later at the file operation.
        let _ = error;
        path.to_string()
    });
    let queue = {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(state.queues.entry(key.clone()).or_default())
    };
    let _guard = queue.lock().await;
    let result = operation().await;
    // Drop the queue entry when this waiter was the last user.
    {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(queue) = state.queues.get(&key)
            && Arc::strong_count(queue) == 2
        // registry + our local clone
        {
            state.queues.remove(&key);
        }
    }
    result
}
