//! Engine-owned title-task settings (ADR-0012): one JSON record in the data
//! dir, separate from UI settings, mutated only through the typed RPC
//! surface. An empty model id means automatic titles are disabled.

use std::path::Path;

use holt_proto::TitleSettings;

use crate::EngineError;
use crate::settings_record::SettingsRecord;

const FILE_NAME: &str = "title-settings.json";

/// Upper bound for a saved instruction — generous enough for tone/language
/// customization, small enough to catch a pasted document.
pub(crate) const MAX_TITLE_INSTRUCTION_CHARS: usize = 2000;

pub(crate) type TitleSettingsStore = SettingsRecord<TitleSettings>;

pub(crate) fn load(data_dir: &Path) -> Result<TitleSettingsStore, EngineError> {
    SettingsRecord::load(data_dir, FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(model_id: Option<&str>, instruction: &str) -> TitleSettings {
        TitleSettings {
            model_id: model_id.map(str::to_string),
            instruction: instruction.to_string(),
        }
    }

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = load(dir.path()).unwrap();
        assert_eq!(store.get(), TitleSettings::default());

        store
            .save(settings(Some("openai/gpt-5.4"), "name it"))
            .unwrap();
        let restored = load(dir.path()).unwrap();
        assert_eq!(restored.get(), settings(Some("openai/gpt-5.4"), "name it"));
    }

    #[test]
    fn corrupt_settings_load_as_defaults_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "{broken").unwrap();
        let store = load(dir.path()).unwrap();
        assert_eq!(store.get(), TitleSettings::default());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");

        // The store keeps working: a later save repairs the file.
        store.save(settings(None, "name it")).unwrap();
        let restored = load(dir.path()).unwrap();
        assert_eq!(restored.get(), settings(None, "name it"));
    }

    #[test]
    fn empty_settings_file_loads_as_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), b"").unwrap();
        let store = load(dir.path()).unwrap();
        assert_eq!(store.get(), TitleSettings::default());
    }

    #[test]
    fn a_record_missing_new_fields_fills_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            br#"{"modelId":"openai/gpt-5.4"}"#,
        )
        .unwrap();
        let store = load(dir.path()).unwrap();
        assert_eq!(
            store.get(),
            settings(
                Some("openai/gpt-5.4"),
                holt_proto::DEFAULT_TITLE_INSTRUCTION
            )
        );
    }
}
