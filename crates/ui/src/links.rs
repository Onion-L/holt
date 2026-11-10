//! Chat-link protocol facade for the UI.

pub use holt_proto::{ChatLink, holt_chat_link, parse_holt_chat_link, workspace_locator};

/// Schemes a click on a content-derived target may hand to the platform's
/// URL handler. Everything else — `file:`, custom app schemes registered by
/// other software — is dropped: these targets come from model output, PTY
/// output, and repo content, and one click must not reach an arbitrary
/// registered handler (issue #32; the vendored macOS backend passed any
/// NSURL-acceptable scheme to `NSWorkspace openURL`). The file tree's
/// open-externally keeps its `file://` dispatch — its targets are the
/// user's own workspace listing, not untrusted content.
pub(crate) fn safe_external_url(target: &str) -> Option<&str> {
    let (scheme, rest) = target.split_once(':')?;
    let ok = match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" => rest.starts_with("//"),
        "mailto" => !rest.is_empty(),
        _ => false,
    };
    ok.then_some(target)
}

#[cfg(test)]
mod tests {
    use super::safe_external_url;

    #[test]
    fn allows_the_web_schemes() {
        assert_eq!(
            safe_external_url("https://github.com/Onion-L/holt"),
            Some("https://github.com/Onion-L/holt")
        );
        assert_eq!(
            safe_external_url("http://localhost:3000"),
            Some("http://localhost:3000")
        );
        assert_eq!(safe_external_url("mailto:a@b.com"), Some("mailto:a@b.com"));
        // Scheme compares case-insensitively; the original target passes through.
        assert_eq!(
            safe_external_url("HTTPS://example.com"),
            Some("HTTPS://example.com")
        );
    }

    #[test]
    fn drops_local_and_custom_schemes() {
        assert_eq!(safe_external_url("file:///Users/x/.ssh/id_ed25519"), None);
        assert_eq!(safe_external_url("x-app://payload"), None);
        assert_eq!(safe_external_url("ssh://host/repo"), None);
        assert_eq!(safe_external_url("javascript:alert(1)"), None);
        // A scheme-shaped prefix without a colon is not a URL at all.
        assert_eq!(safe_external_url("example.com"), None);
    }
}
