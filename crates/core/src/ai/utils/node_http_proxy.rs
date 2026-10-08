//! Port of `pi-core/ai/src/utils/node-http-proxy.ts`: environment-driven
//! HTTP proxy resolution for provider requests.
//!
//! The TypeScript version produces the proxy URL that a Node HTTP agent
//! consumes; the Rust port returns the same resolved URL (a `URL` object
//! whose serialization normalizes an empty path to a trailing slash) so the
//! transport layer can construct an equivalent client. `NO_PROXY`/wildcard
//! matching is ported exactly.

use crate::ai::types::ProviderEnv;
use crate::ai::utils::provider_env::get_provider_env_value;

const DEFAULT_PROXY_PORTS: &[(&str, u16)] = &[
    ("ftp", 21),
    ("gopher", 70),
    ("http", 80),
    ("https", 443),
    ("ws", 80),
    ("wss", 443),
];

fn default_proxy_port(protocol: &str) -> u16 {
    DEFAULT_PROXY_PORTS
        .iter()
        .find(|(name, _)| *name == protocol)
        .map_or(0, |(_, port)| *port)
}

fn get_proxy_env(key: &str, env: Option<&ProviderEnv>) -> String {
    let lowercase_key = key.to_lowercase();
    let uppercase_key = key.to_uppercase();
    let from_env = |name: &str| {
        env.and_then(|env| env.get(name))
            .map(|value| (!value.is_empty()).then(|| value.clone()))
            .unwrap_or(None)
    };
    from_env(&lowercase_key)
        .or_else(|| from_env(&uppercase_key))
        .or_else(|| get_provider_env_value(&lowercase_key, None))
        .or_else(|| get_provider_env_value(&uppercase_key, None))
        .unwrap_or_default()
}

fn should_proxy_hostname(hostname: &str, port: u16, env: Option<&ProviderEnv>) -> bool {
    let no_proxy = get_proxy_env("no_proxy", env).to_lowercase();
    if no_proxy.is_empty() {
        return true;
    }
    if no_proxy == "*" {
        return false;
    }

    no_proxy
        .split(|ch: char| ch == ',' || ch.is_whitespace())
        .all(|proxy| {
            if proxy.is_empty() {
                return true;
            }
            let (proxy_hostname, proxy_port) = match proxy.rsplit_once(':') {
                Some((host, port_text)) => match port_text.parse::<u16>() {
                    Ok(parsed) => (host, parsed),
                    Err(_) => (proxy, 0),
                },
                None => (proxy, 0),
            };
            if proxy_port != 0 && proxy_port != port {
                return true;
            }

            let starts_with_wildcard =
                proxy_hostname.starts_with('*') || proxy_hostname.starts_with('.');
            if !starts_with_wildcard {
                return hostname != proxy_hostname;
            }

            let suffix = proxy_hostname.trim_start_matches('*');
            !hostname.ends_with(suffix)
        })
}

fn get_proxy_for_url(target_url: &str, env: Option<&ProviderEnv>) -> Result<String, String> {
    let parsed = match url::Url::parse(target_url) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(String::new()),
    };
    if parsed.host_str().is_none() {
        return Ok(String::new());
    }

    let protocol = parsed.scheme().to_string();
    let hostname = parsed.host_str().unwrap_or_default().to_string();
    let port = parsed
        .port()
        .unwrap_or_else(|| default_proxy_port(&protocol));
    if !should_proxy_hostname(&hostname, port, env) {
        return Ok(String::new());
    }

    let mut proxy = get_proxy_env(&format!("{protocol}_proxy"), env);
    if proxy.is_empty() {
        proxy = get_proxy_env("all_proxy", env);
    }
    if !proxy.is_empty() && !proxy.contains("://") {
        proxy = format!("{protocol}://{proxy}");
    }
    Ok(proxy)
}

/// Port of `UNSUPPORTED_PROXY_PROTOCOL_MESSAGE`.
pub const UNSUPPORTED_PROXY_PROTOCOL_MESSAGE: &str = "Unsupported proxy protocol. SOCKS and PAC proxy URLs are not supported; use an HTTP or HTTPS proxy URL.";

/// Port of `resolveHttpProxyUrlForTarget`: resolves the proxy URL for a
/// target from the scoped/process environment, or `None` when no proxy
/// applies. The returned string is the URL serialization (empty paths
/// normalize to a trailing slash, matching `URL.toString()`). Errors carry
/// the unsupported-protocol message for SOCKS/PAC.
pub fn resolve_http_proxy_url_for_target(
    target_url: &str,
    env: Option<&ProviderEnv>,
) -> Result<Option<String>, String> {
    let proxy = get_proxy_for_url(target_url, env)?;
    if proxy.is_empty() {
        return Ok(None);
    }

    let proxy_url =
        url::Url::parse(&proxy).map_err(|error| format!("Invalid proxy URL {proxy:?}: {error}"))?;
    if proxy_url.scheme() != "http" && proxy_url.scheme() != "https" {
        return Err(format!(
            "{UNSUPPORTED_PROXY_PROTOCOL_MESSAGE} Got {}:",
            proxy_url.scheme()
        ));
    }

    Ok(Some(proxy_url.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::types::ProviderEnv;

    fn scoped_env(pairs: &[(&str, &str)]) -> ProviderEnv {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    /// Port of the "node HTTP proxy resolution" describe block (the process
    /// env cases read through `get_provider_env_value`, so tests pass scoped
    /// envs for determinism).
    #[test]
    fn respects_no_proxy_exclusions() {
        let env = scoped_env(&[
            ("HTTPS_PROXY", "http://proxy.example:8080"),
            ("NO_PROXY", "bedrock-runtime.us-east-1.amazonaws.com"),
        ]);
        let resolved = resolve_http_proxy_url_for_target(
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            Some(&env),
        )
        .unwrap();
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolves_http_and_https_proxy_urls() {
        let env = scoped_env(&[("HTTPS_PROXY", "http://proxy.example:8080")]);
        let resolved = resolve_http_proxy_url_for_target(
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            Some(&env),
        )
        .unwrap();
        assert_eq!(resolved.as_deref(), Some("http://proxy.example:8080/"));
    }

    #[test]
    fn prefers_scoped_proxy_before_process_env() {
        let env = scoped_env(&[("HTTPS_PROXY", "http://scoped-proxy.example:8080")]);
        let resolved = resolve_http_proxy_url_for_target(
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            Some(&env),
        )
        .unwrap();
        assert_eq!(
            resolved.as_deref(),
            Some("http://scoped-proxy.example:8080/")
        );
    }

    #[test]
    fn rejects_socks_and_pac_proxy_urls_explicitly() {
        let env = scoped_env(&[("HTTPS_PROXY", "socks5://proxy.example:1080")]);
        let error = resolve_http_proxy_url_for_target(
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            Some(&env),
        )
        .unwrap_err();
        assert!(
            error.starts_with(UNSUPPORTED_PROXY_PROTOCOL_MESSAGE),
            "{error}"
        );
    }

    #[test]
    fn wildcard_no_proxy_suffix_matching() {
        let env = scoped_env(&[
            ("HTTPS_PROXY", "http://proxy.example:8080"),
            ("NO_PROXY", "*.amazonaws.com"),
        ]);
        let resolved = resolve_http_proxy_url_for_target(
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            Some(&env),
        )
        .unwrap();
        assert_eq!(resolved, None, "wildcard suffix should bypass the proxy");
        let resolved =
            resolve_http_proxy_url_for_target("https://example.com", Some(&env)).unwrap();
        assert_eq!(resolved.as_deref(), Some("http://proxy.example:8080/"));
    }

    #[test]
    fn all_proxy_falls_back_and_defaults_scheme() {
        let env = scoped_env(&[("ALL_PROXY", "proxy.example:3128")]);
        let resolved = resolve_http_proxy_url_for_target("http://example.com", Some(&env)).unwrap();
        assert_eq!(resolved.as_deref(), Some("http://proxy.example:3128/"));
    }
}
