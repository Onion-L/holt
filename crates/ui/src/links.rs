//! Stable conversation links shared by sidebar copy actions and inbound URL routing.

use holt_proto::{AuthState, WorkspaceScope};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationDeepLink {
    pub chat_id: String,
    pub workspace: String,
}

/// Opaque locator: enough to reject links for a different local/synced
/// workspace without putting device, user, or organization ids in the URL.
pub fn workspace_locator(
    scope: Option<WorkspaceScope>,
    auth: Option<&AuthState>,
    local_device_id: Option<&str>,
) -> Option<String> {
    let scope = scope?;
    let identity = match scope {
        WorkspaceScope::Synced | WorkspaceScope::Development => {
            let Some(AuthState::SignedIn { user, org_id }) = auth else {
                return None;
            };
            format!(
                "user:{}:org:{}",
                user.id,
                org_id.as_deref().unwrap_or("personal")
            )
        }
        WorkspaceScope::Local => format!("device:{}", local_device_id?),
    };
    let mut hash = Sha256::new();
    hash.update(format!("{scope:?}\0{identity}"));
    Some(format!("{:x}", hash.finalize())[..16].to_string())
}

pub fn holt_conversation_link(chat_id: &str, workspace: &str) -> String {
    format!(
        "holt://open/chat/{}?workspace={}",
        encode_component(chat_id),
        encode_component(workspace)
    )
}

pub fn parse_holt_conversation_link(url: &str) -> Result<ConversationDeepLink, &'static str> {
    let rest = url
        .strip_prefix("holt://open/chat/")
        .ok_or("not a Holt conversation link")?;
    let (chat_id, query) = rest.split_once('?').ok_or("missing workspace locator")?;
    if chat_id.is_empty() || chat_id.contains('/') {
        return Err("invalid conversation id");
    }
    let workspace = query
        .split('&')
        .find_map(|part| part.strip_prefix("workspace="))
        .ok_or("missing workspace locator")?;
    Ok(ConversationDeepLink {
        chat_id: decode_component(chat_id)?,
        workspace: decode_component(workspace)?,
    })
}

fn encode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn decode_component(value: &str) -> Result<String, &'static str> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let encoded = bytes
                .get(index + 1..index + 3)
                .ok_or("invalid URL escape")?;
            let text = std::str::from_utf8(encoded).map_err(|_| "invalid URL escape")?;
            out.push(u8::from_str_radix(text, 16).map_err(|_| "invalid URL escape")?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "invalid UTF-8 in URL")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holt_link_round_trips_reserved_characters() {
        let link = holt_conversation_link("chat/with space", "workspace:one");
        assert_eq!(
            parse_holt_conversation_link(&link).unwrap(),
            ConversationDeepLink {
                chat_id: "chat/with space".into(),
                workspace: "workspace:one".into(),
            }
        );
    }

    #[test]
    fn malformed_or_foreign_links_are_rejected() {
        assert!(parse_holt_conversation_link("https://example.com").is_err());
        assert!(parse_holt_conversation_link("holt://open/chat/id").is_err());
        assert!(parse_holt_conversation_link("holt://open/chat/%GG?workspace=x").is_err());
    }

    #[test]
    fn local_workspace_locator_waits_for_device_identity() {
        assert_eq!(
            workspace_locator(Some(WorkspaceScope::Local), None, None),
            None
        );
        let first = workspace_locator(Some(WorkspaceScope::Local), None, Some("device-a"));
        let second = workspace_locator(Some(WorkspaceScope::Local), None, Some("device-b"));
        assert!(first.is_some());
        assert_ne!(first, second);
    }

    #[test]
    fn synced_workspace_locator_waits_for_signed_in_identity() {
        let scope = Some(WorkspaceScope::Synced);
        assert_eq!(workspace_locator(scope, None, Some("device-a")), None);
        assert_eq!(
            workspace_locator(scope, Some(&AuthState::SignedOut), Some("device-a")),
            None
        );
        assert_eq!(
            workspace_locator(
                scope,
                Some(&AuthState::NeedsOrganization {
                    user: holt_proto::UserProfile {
                        id: "user-a".into(),
                        email: "user@example.com".into(),
                        name: None,
                    },
                }),
                Some("device-a"),
            ),
            None
        );
        assert!(
            workspace_locator(
                scope,
                Some(&AuthState::SignedIn {
                    user: holt_proto::UserProfile {
                        id: "user-a".into(),
                        email: "user@example.com".into(),
                        name: None,
                    },
                    org_id: Some("org-a".into()),
                }),
                None,
            )
            .is_some()
        );
    }
}
