//! Port of `pi-core/ai/src/session-resources.ts` and
//! `pi-core/ai/src/utils/pi-user-agent.ts`.

use std::sync::{Arc, Mutex, OnceLock};

/// Port of `SessionResourceCleanup`.
pub type SessionResourceCleanup = Arc<dyn Fn(Option<&str>) + Send + Sync>;

fn cleanups() -> &'static Mutex<Vec<SessionResourceCleanup>> {
    static CLEANUPS: OnceLock<Mutex<Vec<SessionResourceCleanup>>> = OnceLock::new();
    CLEANUPS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Port of `registerSessionResourceCleanup`; the returned closure unregisters.
pub fn register_session_resource_cleanup(cleanup: SessionResourceCleanup) -> impl Fn() + Send {
    let list = cleanups();
    list.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(Arc::clone(&cleanup));
    move || {
        let list = cleanups();
        let mut guard = list
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.retain(|registered| !Arc::ptr_eq(registered, &cleanup));
    }
}

/// Port of `cleanupSessionResources`: runs every registered cleanup,
/// collecting panics as an aggregate failure. The TypeScript version joins
/// thrown values into an `AggregateError`; Rust reports the count and the
/// first panic message in the error string.
pub fn cleanup_session_resources(session_id: Option<&str>) -> Result<(), String> {
    let list = cleanups();
    let guard = list
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut errors: Vec<String> = Vec::new();
    for cleanup in guard.iter() {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cleanup(session_id)));
        if let Err(panic) = result {
            let message = panic
                .downcast_ref::<&str>()
                .map(|message| message.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "cleanup panicked".to_string());
            errors.push(message);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Failed to cleanup session resources: {} error(s): {}",
            errors.len(),
            errors.join("; ")
        ))
    }
}

/// Port of `getPiUserAgent` (utils/pi-user-agent.ts). The TypeScript version
/// reads `node:os` platform/release/arch; Rust reads the compile-time target
/// triple plus `std::env::consts`. Deliberate deviation from upstream: the
/// product prefix is `pi-core-rs` instead of `pi` so providers can tell the
/// two clients apart.
pub fn get_pi_user_agent() -> String {
    format!(
        "pi-core-rs ({} {}; {})",
        std::env::consts::OS,
        os_release(),
        std::env::consts::ARCH
    )
}

fn os_release() -> String {
    #[cfg(target_os = "macos")]
    if let Ok(release) = std::process::Command::new("sw_vers")
        .args(["-productVersion"])
        .output()
    {
        let text = String::from_utf8_lossy(&release.stdout).trim().to_string();
        if !text.is_empty() {
            return text;
        }
    }
    #[cfg(target_os = "linux")]
    if let Ok(contents) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        let text = contents.trim().to_string();
        if !text.is_empty() {
            return text;
        }
    }
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The cleanup registry is process-global; serialize the tests that touch
    // it.
    static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn session_resources_register_run_and_unregister() {
        let _registry_guard = REGISTRY_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_for_cleanup = Arc::clone(&calls);
        let unregister = register_session_resource_cleanup(Arc::new(move |session_id| {
            calls_for_cleanup
                .lock()
                .unwrap()
                .push(session_id.unwrap_or("none").to_string());
        }));

        cleanup_session_resources(Some("session-1")).expect("cleanup succeeds");
        unregister();
        cleanup_session_resources(Some("session-2")).expect("cleanup succeeds");

        assert_eq!(*calls.lock().unwrap(), vec!["session-1".to_string()]);
    }

    #[test]
    fn session_resource_panics_are_collected() {
        let _registry_guard = REGISTRY_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let unregister = register_session_resource_cleanup(Arc::new(|_| {
            panic!("boom");
        }));
        let error = cleanup_session_resources(None).expect_err("panicking cleanup fails");
        assert!(error.contains("Failed to cleanup session resources"));
        assert!(error.contains("boom"));
        unregister();
        cleanup_session_resources(None).expect("clean after unregister");
    }

    #[test]
    fn pi_user_agent_has_expected_shape() {
        let agent = get_pi_user_agent();
        assert!(
            agent.starts_with("pi-core-rs ("),
            "unexpected agent: {agent}"
        );
        assert!(agent.ends_with(')'));
    }
}
