//! Shared helpers for harness integration tests (Rust counterpart of
//! `pi-core/agent/test/harness/session-test-utils.ts`; the `afterEach`
//! cleanup hook becomes RAII). Not a test target; unused helpers stay
//! available for the test binaries that need them.
#![allow(dead_code)]

/// Shared helpers for the credential-gated live AI suites (`tests/ai_live_*`).
pub mod live;

use std::ops::Deref;

/// A temp directory removed when the guard drops (per-test, replacing the
/// TypeScript suite's `afterEach` registry).
pub struct TempDirGuard {
    path: String,
}

impl Deref for TempDirGuard {
    type Target = str;
    fn deref(&self) -> &str {
        &self.path
    }
}

impl AsRef<str> for TempDirGuard {
    fn as_ref(&self) -> &str {
        &self.path
    }
}

impl std::fmt::Display for TempDirGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.path)
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Creates a unique temp directory removed when the returned guard drops.
pub fn create_temp_dir() -> TempDirGuard {
    let dir = std::env::temp_dir().join(format!(
        "pi-agent-session-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default(),
        hex_suffix()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    TempDirGuard {
        path: dir.to_string_lossy().into_owned(),
    }
}

fn hex_suffix() -> String {
    let mut bytes = [0u8; 6];
    getrandom::fill(&mut bytes).expect("system RNG is always available");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
