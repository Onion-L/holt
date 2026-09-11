//! Shared typed seam for workspace registry adapters.
//!
//! `WorkspaceDoc` (Loro) and `RegistryDoc` (HLC/overlay) deliberately keep
//! different persistence mechanisms. This interface names the domain surface
//! they must agree on; contract tests exercise both adapters through it.

use chrono::{DateTime, Utc};

use holt_proto::{Chat, Device, Session, Space};

use crate::{DeletedSpace, DocError, WorkspaceState};

/// Typed workspace operations shared by registry adapters.
pub trait WorkspaceRegistry {
    fn read_all(&self) -> Result<WorkspaceState, DocError>;
    fn upsert_device(&mut self, device: &Device) -> Result<(), DocError>;
    fn upsert_space(&mut self, space: &Space) -> Result<(), DocError>;
    fn upsert_chat(&mut self, chat: &Chat) -> Result<(), DocError>;
    fn upsert_session(&mut self, session: &Session) -> Result<(), DocError>;
    fn delete_space(&mut self, space_id: &str) -> Result<DeletedSpace, DocError>;
    fn set_chat_archived(&mut self, chat_id: &str, archived: bool) -> Result<bool, DocError>;
    fn set_chat_seen(&mut self, chat_id: &str, at: DateTime<Utc>) -> Result<bool, DocError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn exercise<R: WorkspaceRegistry>(mut registry: R) {
        let now = Utc.with_ymd_and_hms(2026, 8, 31, 0, 0, 0).unwrap();
        let device = Device {
            id: "dev-a".into(),
            name: "Laptop".into(),
            platform: "macos".into(),
            last_seen_at: None,
            created_at: Some(now),
            version: None,
        };
        let space = Space {
            id: "space-a".into(),
            device_id: device.id.clone(),
            path: "/tmp/project".into(),
            name: None,
            git_detected: true,
            git_checked_at: Some(now),
            checkout_id: None,
            created_at: now,
        };
        let chat = Chat {
            id: "chat-a".into(),
            device_id: device.id.clone(),
            title: Some("Work".into()),
            title_source: holt_proto::TitleSource::UserManual,
            title_task_started: false,
            archived: false,
            cwd: Some(space.path.clone()),
            branch: None,
            checkout_id: None,
            source_context: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: now,
            space_id: Some(space.id.clone()),
            last_seen_at: None,
            room_gen: None,
            compact_before_next_turn: false,
            plan_mode: None,
            approved_plan_path: None,
        };
        let session = Session {
            chat_id: chat.id.clone(),
            device_id: device.id.clone(),
            status: holt_proto::SessionStatus::Idle,
            started_at: None,
            updated_at: now,
        };
        registry.upsert_device(&device).unwrap();
        registry.upsert_space(&space).unwrap();
        registry.upsert_chat(&chat).unwrap();
        registry.upsert_session(&session).unwrap();
        assert_eq!(registry.read_all().unwrap().chats.len(), 1);
        assert!(registry.set_chat_archived(&chat.id, true).unwrap());
        assert!(registry.set_chat_seen(&chat.id, now).unwrap());
        let deleted = registry.delete_space(&space.id).unwrap();
        assert!(deleted.existed);
        assert_eq!(deleted.chat_ids, vec![chat.id]);
        assert!(registry.read_all().unwrap().chats.is_empty());
    }

    #[test]
    fn both_adapters_obey_the_same_workspace_contract() {
        exercise(crate::WorkspaceDoc::new());
        exercise(crate::RegistryDoc::new("dev-a"));
    }
}
