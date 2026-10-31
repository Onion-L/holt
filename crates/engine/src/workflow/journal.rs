//! A run's agent cache: every child that finished cleanly, keyed by what
//! it was asked. A relaunched run (`resume`) answers a matching `agent()`
//! from here instead of running the child again.
//!
//! The key is content-addressed — role, schema, and prompt, plus which
//! occurrence of that exact request this is — never the call's start
//! order: a `pipeline()` starts later stages in completion order, which a
//! replay does not reproduce. A step that depends on an earlier result
//! embeds it in its prompt, so a changed upstream result changes the key and
//! the step reruns on its own. Failures are never recorded: they rerun.

use std::{
    collections::HashMap,
    io::Write as _,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub(crate) const FILE: &str = "journal.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Record {
    pub(crate) key: String,
    /// The child doc the result came from — a replayed chip links to it.
    pub(crate) subagent: String,
    pub(crate) result: serde_json::Value,
    #[serde(default)]
    pub(crate) tokens: u64,
}

pub(crate) struct Journal {
    path: PathBuf,
    records: HashMap<String, Record>,
    occurrences: HashMap<String, usize>,
}

impl Journal {
    /// Open the run directory's journal, seeded from `resume_from`'s when
    /// relaunching: the seed is copied in first, so a run resumed twice
    /// still carries every earlier result.
    pub(crate) fn open(run_dir: &Path, resume_from: Option<&Path>) -> std::io::Result<Self> {
        std::fs::create_dir_all(run_dir)?;
        let path = run_dir.join(FILE);
        if let Some(seed) = resume_from.map(|dir| dir.join(FILE))
            && seed.exists()
            && seed != path
        {
            std::fs::copy(&seed, &path)?;
        }
        let mut records = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            // A torn last line (crash mid-append) is skipped, not fatal.
            for record in text
                .lines()
                .filter_map(|line| serde_json::from_str::<Record>(line).ok())
            {
                records.insert(record.key.clone(), record);
            }
        }
        Ok(Self {
            path,
            records,
            occurrences: HashMap::new(),
        })
    }

    /// The cache key for the next request with these inputs.
    pub(crate) fn key(
        &mut self,
        role: &str,
        schema: Option<&serde_json::Value>,
        prompt: &str,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(
            serde_json::to_string(&(role, schema, prompt)).expect("plain JSON values serialize"),
        );
        let base: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let occurrence = self.occurrences.entry(base.clone()).or_default();
        let key = format!("{base}#{occurrence}");
        *occurrence += 1;
        key
    }

    pub(crate) fn cached(&self, key: &str) -> Option<&Record> {
        self.records.get(key)
    }

    pub(crate) fn record(&mut self, record: Record) -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let mut line = serde_json::to_string(&record).map_err(std::io::Error::other)?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        self.records.insert(record.key.clone(), record);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(journal: &mut Journal, key: &str, result: &str) {
        journal
            .record(Record {
                key: key.into(),
                subagent: format!("doc-{result}"),
                result: serde_json::json!(result),
                tokens: 10,
            })
            .unwrap();
    }

    #[test]
    fn repeated_requests_take_successive_occurrences() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path(), None).unwrap();
        let first = journal.key("explorer", None, "same");
        let second = journal.key("explorer", None, "same");
        assert_ne!(first, second);
        assert!(first.ends_with("#0") && second.ends_with("#1"));
        // Every input is part of the identity.
        let schema = serde_json::json!({"type": "object"});
        assert_ne!(
            journal.key("worker", None, "other").split('#').next(),
            journal.key("explorer", None, "other").split('#').next()
        );
        assert_ne!(
            journal
                .key("explorer", Some(&schema), "x")
                .split('#')
                .next(),
            journal.key("explorer", None, "x").split('#').next()
        );
    }

    #[test]
    fn a_resumed_run_hits_recorded_results_and_misses_changed_prompts() {
        let first_dir = tempfile::tempdir().unwrap();
        let mut first = Journal::open(first_dir.path(), None).unwrap();
        let done = first.key("explorer", None, "audit a");
        record(&mut first, &done, "a");
        let _failed = first.key("explorer", None, "audit b");

        let second_dir = tempfile::tempdir().unwrap();
        let mut second = Journal::open(second_dir.path(), Some(first_dir.path())).unwrap();
        let again = second.key("explorer", None, "audit a");
        assert_eq!(second.cached(&again).unwrap().result, "a");
        let retry = second.key("explorer", None, "audit b");
        assert!(second.cached(&retry).is_none(), "failures are never cached");
        let changed = second.key("explorer", None, "audit a, differently");
        assert!(second.cached(&changed).is_none());
        record(&mut second, &retry, "b");

        // A second resume still sees both generations.
        let third_dir = tempfile::tempdir().unwrap();
        let mut third = Journal::open(third_dir.path(), Some(second_dir.path())).unwrap();
        let a = third.key("explorer", None, "audit a");
        let b = third.key("explorer", None, "audit b");
        assert!(third.cached(&a).is_some() && third.cached(&b).is_some());
    }

    #[test]
    fn a_torn_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path(), None).unwrap();
        let key = journal.key("explorer", None, "x");
        record(&mut journal, &key, "x");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join(FILE))
            .unwrap();
        file.write_all(b"{\"key\": \"torn").unwrap();
        let mut reopened = Journal::open(dir.path(), None).unwrap();
        let key = reopened.key("explorer", None, "x");
        assert!(reopened.cached(&key).is_some());
    }
}
