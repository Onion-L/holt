//! Engine-owned goal-verifier settings (ADR-0044 follow-up): the optional
//! model the goal loop's verification pass rides. `None` — the default —
//! keeps the chat's own model: goal mode stays zero-config until the user
//! opts into a separate judge.

use std::path::Path;

use holt_proto::GoalSettings;

use crate::EngineError;
use crate::settings_record::SettingsRecord;

const FILE_NAME: &str = "goal-settings.json";

pub(crate) type GoalSettingsStore = SettingsRecord<GoalSettings>;

pub(crate) fn load(data_dir: &Path) -> Result<GoalSettingsStore, EngineError> {
    SettingsRecord::load(data_dir, FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path()).unwrap();
        assert_eq!(store.get(), GoalSettings::default());

        store
            .save(GoalSettings {
                model_id: Some("openai/gpt-5.4-mini".into()),
            })
            .unwrap();
        let restored = load(dir.path()).unwrap();
        assert_eq!(
            restored.get().model_id.as_deref(),
            Some("openai/gpt-5.4-mini")
        );
    }

    #[test]
    fn corrupt_settings_load_as_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), "{broken").unwrap();
        let store = load(dir.path()).unwrap();
        assert_eq!(store.get(), GoalSettings::default());
    }
}
