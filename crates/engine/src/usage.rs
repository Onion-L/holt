//! The per-chat usage ledger (usage-ledger spec, ticket 01): one append-only
//! JSONL file per chat under `usage/<chatId>.jsonl`, one record per metered
//! provider round-trip — all token fields, the attribution source (`kind`),
//! provider, model, the Turn's message id and outcome, and the upstream cost
//! stored verbatim (Holt computes no prices). The file follows the History
//! record's durability shapes: a version header line, the append atomicity
//! pattern (header on an empty file, a repaired missing final newline), and a
//! tolerant reader that skips a crash-truncated trailing line. A damaged file
//! is quarantined `.corrupt` (kept, never overwritten) and totals restart
//! from zero — bookkeeping never blocks the chat.
//!
//! Write choreography: records captured inside a Turn accumulate on the
//! chat's runtime and land as ONE batch append at settlement, after queue
//! completion; calls outside the Turn model (title task, manual Compaction)
//! append immediately at completion (ticket 02 wires those meters). Every
//! write is fire-and-forget — a failed append is logged and costs only the
//! record, never the Turn, the queue, or the terminal event. A crash before
//! settlement loses the running Turn's batch, unrepaired, by design.
//!
//! Deleting a chat archives before it deletes: the chat's whole ledger
//! segment is appended — chat-attributed — to the device-level
//! `usage/archive.jsonl` (grow-only, the future usage dashboard's feed),
//! then the per-chat file and its `.corrupt` siblings are removed. Chat-level
//! data dies with the chat; the device stream survives it. Archiving is
//! best-effort — a failure logs and the delete proceeds.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use pi_core::ai::types::{AssistantMessage, UsageCost};

use crate::agent::ChatRuntime;
use crate::store::id_is_path_safe;

/// The ledger format version carried by the header line.
const USAGE_VERSION: u32 = 1;

/// What caused a model call (the record's attribution source). A later kind
/// — the goal verifier — joins here with its own variant.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum UsageKind {
    Turn,
    Subagent,
    Compaction,
    AutoReview,
    Title,
}

/// The terminal outcome a Turn stamps onto its records; `None` on records
/// from calls outside the Turn model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TurnOutcome {
    Succeeded,
    Failed,
    Interrupted,
}

impl TurnOutcome {
    /// The ledger's stamp for a run's terminal `TurnEnd` (ADR-0019) — the
    /// failure reason stays on the terminal event; the record keeps only
    /// the outcome.
    pub(crate) fn of(end: &crate::agent::TurnEnd) -> Self {
        match end {
            crate::agent::TurnEnd::Succeeded => Self::Succeeded,
            crate::agent::TurnEnd::Failed { .. } => Self::Failed,
            crate::agent::TurnEnd::Interrupted => Self::Interrupted,
        }
    }
}

/// One metered provider round-trip. Token fields mirror the upstream
/// `Usage`; `cost` rides verbatim; `messageId`/`turnOutcome` are stamped at
/// Turn settlement; `subagentDocId` marks a subagent's record in its parent
/// chat's ledger; `chatId` is set only on archive lines, where the filename
/// no longer carries the attribution.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UsageRecord {
    pub kind: UsageKind,
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_outcome: Option<TurnOutcome>,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    pub cost: UsageCost,
    pub timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_doc_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
}

impl UsageRecord {
    /// A main-chat Turn round-trip from one completed assistant message —
    /// the raw report, read before the History repair decides what persists,
    /// so failed and aborted answers stay billed. The Turn's message id and
    /// outcome arrive at settlement.
    fn from_assistant(message: &AssistantMessage) -> Self {
        let timestamp = if message.timestamp > 0 {
            message.timestamp
        } else {
            chrono::Utc::now().timestamp_millis()
        };
        Self {
            kind: UsageKind::Turn,
            provider: message.provider.clone(),
            model: message.model.clone(),
            message_id: None,
            turn_outcome: None,
            input: message.usage.input,
            output: message.usage.output,
            cache_read: message.usage.cache_read,
            cache_write: message.usage.cache_write,
            cache_write_1h: message.usage.cache_write_1h,
            reasoning: message.usage.reasoning,
            cost: message.usage.cost,
            timestamp,
            subagent_doc_id: None,
            chat_id: None,
        }
    }
}

/// The token fields a record carries, summed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TokenSum {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cache_write_1h: Option<u64>,
    pub reasoning: Option<u64>,
}

/// A chat's replayed running totals: per-kind sums (consumers filter by
/// source themselves) plus the headline gross token count — input, output,
/// and both cache fields, the number the status line will show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct UsageTotals {
    pub by_kind: BTreeMap<UsageKind, TokenSum>,
    pub gross: u64,
}

impl UsageTotals {
    pub(crate) fn add_record(&mut self, record: &UsageRecord) {
        let sum = self.by_kind.entry(record.kind).or_default();
        sum.input += record.input;
        sum.output += record.output;
        sum.cache_read += record.cache_read;
        sum.cache_write += record.cache_write;
        if let Some(n) = record.cache_write_1h {
            *sum.cache_write_1h.get_or_insert(0) += n;
        }
        if let Some(n) = record.reasoning {
            *sum.reasoning.get_or_insert(0) += n;
        }
        self.gross += record.input + record.output + record.cache_read + record.cache_write;
    }

    /// The per-kind view consumers filter by (the WatchChatUsage surface,
    /// ticket 04); the module's own tests ride it until then.
    #[allow(dead_code)]
    pub(crate) fn sum_for(&self, kind: UsageKind) -> TokenSum {
        self.by_kind.get(&kind).copied().unwrap_or_default()
    }
}

/// Per-chat ledger file, guarded by the shared id path-safety rule. The id
/// `archive` is reserved for the device-wide stream — a chat so named keeps
/// no per-chat file rather than writing the archive as its own ledger.
pub(crate) fn usage_path(data_dir: &Path, chat_id: &str) -> Option<PathBuf> {
    if !id_is_path_safe(chat_id) || chat_id == "archive" {
        return None;
    }
    Some(data_dir.join("usage").join(format!("{chat_id}.jsonl")))
}

/// The device-level, grow-only stream deleted chats archive into.
fn archive_path(data_dir: &Path) -> PathBuf {
    data_dir.join("usage").join("archive.jsonl")
}

/// Append one batch of records — the whole settlement append — creating the
/// file (header first) when the chat has no ledger yet. One open per batch:
/// a Turn's records land as one contiguous segment.
pub(crate) fn append_records(
    data_dir: &Path,
    chat_id: &str,
    records: &[UsageRecord],
) -> std::io::Result<()> {
    let Some(path) = usage_path(data_dir, chat_id) else {
        return Ok(());
    };
    append_lines(&path, records)
}

/// The History record's append atomicity pattern, applied to any ledger
/// file: header on an empty file (never on a mere create, so a crash between
/// the two leaves no headerless file), a repaired missing final newline, and
/// one line per record.
fn append_lines(path: &Path, records: &[UsageRecord]) -> std::io::Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let dir = path
        .parent()
        .expect("ledger path has a parent")
        .to_path_buf();
    std::fs::create_dir_all(&dir)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    if file.metadata()?.len() == 0 {
        let header = serde_json::json!({ "version": USAGE_VERSION });
        writeln!(file, "{header}")?;
    } else {
        // A crash may leave a partial record or only omit its newline. Keep
        // those bytes, but isolate them from every later append.
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            file.write_all(b"\n")?;
        }
    }
    for record in records {
        let line = serde_json::to_string(record)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        writeln!(file, "{line}")?;
    }
    Ok(())
}

/// Replay a chat's ledger. A missing or empty file is no ledger; an
/// unreadable header or an unknown version is an error the caller decides
/// how to surface (the runtime quarantines); a truncated or undecodable
/// trailing line — the crash-mid-append shape — is treated as absent.
pub(crate) fn load_records(data_dir: &Path, chat_id: &str) -> Result<Vec<UsageRecord>, String> {
    let Some(path) = usage_path(data_dir, chat_id) else {
        return Ok(Vec::new());
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| format!("{}: usage ledger has no header line", path.display()))?;
    let version = serde_json::from_str::<serde_json::Value>(header)
        .ok()
        .and_then(|value| value.get("version").and_then(|v| v.as_u64()))
        .ok_or_else(|| format!("{}: unreadable usage ledger header", path.display()))?;
    if version != USAGE_VERSION as u64 {
        return Err(format!(
            "{}: unknown usage ledger format version {version} (supported: {USAGE_VERSION})",
            path.display()
        ));
    }
    let mut records = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<UsageRecord>(line) {
            Ok(record) => records.push(record),
            Err(error) => {
                tracing::warn!(target: "holt::usage", %error, "skipping undecodable usage ledger line")
            }
        }
    }
    Ok(records)
}

/// Warm the chat's in-memory totals from its ledger. A damaged file is set
/// aside — kept, never overwritten — and the totals continue from zero; the
/// chat works as though it had never been billed.
pub(crate) fn warm_totals(data_dir: &Path, chat_id: &str) -> UsageTotals {
    let mut totals = UsageTotals::default();
    match load_records(data_dir, chat_id) {
        Ok(records) => {
            for record in &records {
                totals.add_record(record);
            }
        }
        Err(reason) => {
            tracing::warn!(target: "holt::usage", %reason, "setting a damaged usage ledger aside");
            quarantine(data_dir, chat_id);
        }
    }
    totals
}

/// Rename a damaged ledger aside with a timestamped `.corrupt` suffix —
/// never over an earlier quarantine, so nothing is silently thrown away.
pub(crate) fn quarantine(data_dir: &Path, chat_id: &str) {
    let Some(path) = usage_path(data_dir, chat_id) else {
        return;
    };
    let stamp = chrono::Utc::now().timestamp_millis();
    let mut aside = path.with_extension(format!("jsonl.{stamp}.corrupt"));
    let mut bump = 1;
    while aside.exists() {
        aside = path.with_extension(format!("jsonl.{stamp}-{bump}.corrupt"));
        bump += 1;
    }
    if let Err(error) = std::fs::rename(&path, &aside)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(target: "holt::usage", %error, "could not set the damaged usage ledger aside");
    }
}

/// Buffer one completed round-trip of the running main-chat Turn. Subagent
/// round-trips are not booked here: the delegation's billing vector owns
/// them (ticket 02 writes them to the parent ledger), so the child's buffer
/// never becomes a phantom child ledger.
pub(crate) fn capture_round_trip(chat: &ChatRuntime, message: &AssistantMessage) {
    if chat.child.is_some() {
        return;
    }
    chat.usage_pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(UsageRecord::from_assistant(message));
}

/// Settle the finished Turn's usage: stamp the batch with the Turn's message
/// id and outcome, warm the totals, and land the whole batch as ONE append —
/// after queue completion, fire-and-forget. A chat deleted mid-run drops its
/// batch instead of resurrecting a file (the removed check sits inside the
/// persistence lock, so a settle that waited out a concurrent delete still
/// writes nothing); a crash before this point loses it unrepaired.
pub(crate) fn settle_turn(chat: &ChatRuntime, message_id: &str, outcome: TurnOutcome) {
    let mut records =
        std::mem::take(&mut *chat.usage_pending.lock().unwrap_or_else(|e| e.into_inner()));
    if records.is_empty() {
        return;
    }
    for record in &mut records {
        record.message_id = Some(message_id.to_string());
        record.turn_outcome = Some(outcome);
    }
    let _persistence = chat.persistence.lock().unwrap_or_else(|e| e.into_inner());
    if chat.chat_id.is_empty() || chat.is_removed() {
        return;
    }
    let mut totals = chat.usage_totals.lock().unwrap_or_else(|e| e.into_inner());
    for record in &records {
        totals.add_record(record);
    }
    drop(totals);
    if let Err(error) = append_records(&chat.data_dir, &chat.chat_id, &records) {
        tracing::warn!(target: "holt::usage", %error, "usage ledger append failed");
    }
}

/// Book one record from a call outside the Turn model (title task, manual
/// Compaction — ticket 02 wires those meters): appended immediately at
/// completion, never batched, still fire-and-forget. The removed check sits
/// inside the persistence lock for the same delete-race reason as
/// [`settle_turn`].
#[allow(dead_code)] // the meters arrive with ticket 02
pub(crate) fn record_immediate(chat: &ChatRuntime, record: UsageRecord) {
    let _persistence = chat.persistence.lock().unwrap_or_else(|e| e.into_inner());
    if chat.chat_id.is_empty() || chat.is_removed() {
        return;
    }
    chat.usage_totals
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .add_record(&record);
    if let Err(error) = append_records(&chat.data_dir, &chat.chat_id, &[record]) {
        tracing::warn!(target: "holt::usage", %error, "usage ledger append failed");
    }
}

/// Delete-time choreography: archive first, then remove. The chat's whole
/// ledger segment — every readable record, restamped with its chat id —
/// appends to the device-level archive (only ever growing), and then the
/// per-chat file and its `.corrupt` siblings go. Archiving is best-effort:
/// a failure logs and the delete proceeds — bookkeeping never blocks it.
pub(crate) fn archive_and_delete(data_dir: &Path, chat_id: &str) {
    let Some(path) = usage_path(data_dir, chat_id) else {
        return;
    };
    match load_records(data_dir, chat_id) {
        Ok(records) if !records.is_empty() => {
            let mut stamped = records;
            for record in &mut stamped {
                record.chat_id = Some(chat_id.to_string());
            }
            if let Err(error) = append_lines(&archive_path(data_dir), &stamped) {
                tracing::warn!(target: "holt::usage", %error, chat_id, "usage archive append failed");
            }
        }
        Ok(_) => {}
        Err(reason) => {
            tracing::warn!(target: "holt::usage", %reason, chat_id, "damaged usage ledger was not archived")
        }
    }
    let _ = std::fs::remove_file(&path);
    let prefix = format!("{chat_id}.jsonl");
    if let Some(dir) = path.parent()
        && let Ok(entries) = dir.read_dir()
    {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with(&prefix)
                && name.to_string_lossy().ends_with(".corrupt")
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::ai::types::{JsF64, Usage};

    fn record(kind: UsageKind, input: u64, output: u64) -> UsageRecord {
        UsageRecord {
            kind,
            provider: "openai".into(),
            model: "openai/gpt-5.4".into(),
            message_id: Some("m-1".into()),
            turn_outcome: (kind == UsageKind::Turn).then_some(TurnOutcome::Succeeded),
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: None,
            cost: UsageCost {
                input: JsF64(0.001),
                output: JsF64(0.002),
                ..Default::default()
            },
            timestamp: 1_700_000_000_000,
            subagent_doc_id: None,
            chat_id: None,
        }
    }

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("holt-usage-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn records_serialize_camel_case_with_kebab_kinds() {
        let turn = serde_json::to_value(record(UsageKind::Turn, 1, 2)).unwrap();
        assert_eq!(turn["kind"], "turn");
        assert_eq!(turn["turnOutcome"], "succeeded");
        assert_eq!(turn["messageId"], "m-1");
        assert_eq!(turn["cacheRead"], 0);
        assert!(turn.get("cacheWrite1h").is_none());
        assert!(turn.get("reasoning").is_none());
        assert!(turn.get("subagentDocId").is_none());
        assert!(turn.get("chatId").is_none());
        assert_eq!(turn["cost"]["input"], 0.001);

        // Non-Turn records carry no outcome; subagent records carry the
        // child doc id.
        let subagent = UsageRecord {
            message_id: None,
            turn_outcome: None,
            subagent_doc_id: Some("chat-1--sub--abc".into()),
            ..record(UsageKind::Subagent, 3, 4)
        };
        let line = serde_json::to_value(&subagent).unwrap();
        assert_eq!(line["kind"], "subagent");
        assert!(line.get("turnOutcome").is_none());
        assert_eq!(line["subagentDocId"], "chat-1--sub--abc");
        // The JSON shape round-trips losslessly.
        let round_tripped =
            serde_json::to_value(serde_json::from_value::<UsageRecord>(line.clone()).unwrap())
                .unwrap();
        assert_eq!(line, round_tripped);

        let review = serde_json::to_value(record(UsageKind::AutoReview, 5, 6)).unwrap();
        assert_eq!(review["kind"], "auto-review");
        assert!(review.get("turnOutcome").is_none());
    }

    #[test]
    fn replay_restores_per_kind_sums_and_the_gross_total() {
        let dir = temp_dir();
        append_records(
            &dir,
            "chat-1",
            &[
                record(UsageKind::Turn, 100, 10),
                UsageRecord {
                    cache_read: 7,
                    cache_write: 5,
                    cache_write_1h: Some(3),
                    reasoning: Some(2),
                    ..record(UsageKind::Turn, 1, 1)
                },
                record(UsageKind::Compaction, 50, 5),
                record(UsageKind::Title, 20, 2),
            ],
        )
        .unwrap();
        let totals = warm_totals(&dir, "chat-1");
        let turns = totals.sum_for(UsageKind::Turn);
        assert_eq!((turns.input, turns.output), (101, 11));
        assert_eq!(turns.cache_read, 7);
        assert_eq!(turns.cache_write, 5);
        assert_eq!(turns.cache_write_1h, Some(3));
        assert_eq!(turns.reasoning, Some(2));
        assert_eq!(totals.sum_for(UsageKind::Compaction).input, 50);
        assert_eq!(totals.sum_for(UsageKind::Title).input, 20);
        assert_eq!(totals.sum_for(UsageKind::Subagent), TokenSum::default());
        // Gross: every field of every kind's four headline tokens.
        assert_eq!(totals.gross, 101 + 11 + 7 + 5 + 50 + 5 + 20 + 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_truncated_tail_is_tolerated_and_a_damaged_header_is_quarantined() {
        let dir = temp_dir();
        append_records(&dir, "chat-1", &[record(UsageKind::Turn, 1, 1)]).unwrap();
        let path = usage_path(&dir, "chat-1").unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{\"kind\":\"tur");
        std::fs::write(&path, &bytes).unwrap();
        // The crash-mid-append shape: the complete record replays, the
        // partial line is treated as absent, and nothing is quarantined.
        assert_eq!(load_records(&dir, "chat-1").unwrap().len(), 1);
        assert_eq!(
            warm_totals(&dir, "chat-1").sum_for(UsageKind::Turn).input,
            1
        );

        // A garbage header is the damaged shape: the file is set aside with
        // a timestamped, never-overwritten name, and the totals restart
        // from zero.
        std::fs::write(&path, "garbage, not a header\n").unwrap();
        assert_eq!(warm_totals(&dir, "chat-1"), UsageTotals::default());
        let aside: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("chat-1.jsonl") && name.ends_with(".corrupt"))
            .collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert!(!path.exists(), "the damaged file was renamed, not copied");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_version_is_quarantined_too() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("usage")).unwrap();
        std::fs::write(
            dir.join("usage/chat-1.jsonl"),
            "{\"version\":99}\n{\"kind\":\"turn\"}\n",
        )
        .unwrap();
        assert!(load_records(&dir, "chat-1").is_err());
        assert_eq!(warm_totals(&dir, "chat-1"), UsageTotals::default());
        assert!(!dir.join("usage/chat-1.jsonl").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn appending_after_a_missing_final_newline_repairs_the_boundary() {
        let dir = temp_dir();
        append_records(&dir, "chat-1", &[record(UsageKind::Turn, 1, 1)]).unwrap();
        let path = usage_path(&dir, "chat-1").unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.pop(), Some(b'\n'));
        std::fs::write(&path, &bytes).unwrap();

        append_records(&dir, "chat-1", &[record(UsageKind::Turn, 2, 2)]).unwrap();
        let loaded = load_records(&dir, "chat-1").unwrap();
        assert_eq!(loaded.len(), 2);
        // The earlier bytes are kept verbatim ahead of the repair newline.
        assert!(std::fs::read(&path).unwrap().starts_with(&bytes));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unsafe_and_reserved_ids_neither_read_nor_write() {
        let dir = temp_dir();
        append_records(&dir, "../escape", &[record(UsageKind::Turn, 1, 1)]).unwrap();
        assert!(load_records(&dir, "../escape").unwrap().is_empty());
        // `archive` is the device stream's own name — a chat so named keeps
        // no per-chat ledger rather than writing the archive as its own.
        append_records(&dir, "archive", &[record(UsageKind::Turn, 1, 1)]).unwrap();
        assert!(load_records(&dir, "archive").unwrap().is_empty());
        assert!(!dir.join("usage/archive.jsonl").exists());
        match dir.join("usage").read_dir() {
            Ok(mut entries) => assert!(entries.next().is_none()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("unreadable usage dir: {error}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The archive's raw lines — the reserved `archive` id never resolves
    /// through `usage_path`, so the device stream is read straight off disk.
    fn read_archive(dir: &Path) -> Vec<UsageRecord> {
        let text = std::fs::read_to_string(dir.join("usage/archive.jsonl")).unwrap();
        text.lines()
            .skip(1) // the version header
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn deleting_a_chat_archives_its_whole_segment_then_removes_the_files() {
        let dir = temp_dir();
        let original = record(UsageKind::Turn, 100, 10);
        append_records(&dir, "chat-1", std::slice::from_ref(&original)).unwrap();
        append_records(&dir, "chat-2", &[record(UsageKind::Title, 5, 1)]).unwrap();
        // A quarantined sibling of the same chat dies with the chat.
        std::fs::write(dir.join("usage/chat-1.jsonl.123.corrupt"), "junk").unwrap();

        archive_and_delete(&dir, "chat-1");

        // The device stream carries the chat-attributed segment, losslessly.
        let archived = read_archive(&dir);
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].chat_id.as_deref(), Some("chat-1"));
        assert_eq!(archived[0].kind, UsageKind::Turn);
        assert_eq!(archived[0].input, 100);
        assert_eq!(archived[0].output, 10);
        assert_eq!(archived[0].timestamp, original.timestamp);
        assert_eq!(archived[0].cost.input, original.cost.input);

        // The chat's files are gone; other chats keep theirs.
        assert!(!dir.join("usage/chat-1.jsonl").exists());
        assert!(!dir.join("usage/chat-1.jsonl.123.corrupt").exists());
        assert!(dir.join("usage/chat-2.jsonl").exists());

        // Archiving again (a second segment for another chat) appends — the
        // stream only ever grows.
        archive_and_delete(&dir, "chat-2");
        assert_eq!(read_archive(&dir).len(), 2);
        assert!(!dir.join("usage/chat-2.jsonl").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn settling_a_turn_drains_the_buffer_into_one_stamped_batch() {
        let dir = temp_dir();
        let chat = ChatRuntime::load(
            &dir,
            "chat-1",
            "device",
            std::sync::Arc::new(std::sync::Mutex::new(())),
        );
        chat.usage_pending
            .lock()
            .unwrap()
            .push(record(UsageKind::Turn, 10, 1));
        chat.usage_pending
            .lock()
            .unwrap()
            .push(record(UsageKind::Turn, 20, 2));

        settle_turn(&chat, "m-1", TurnOutcome::Failed);

        let settled = load_records(&dir, "chat-1").unwrap();
        assert_eq!(settled.len(), 2);
        assert!(settled.iter().all(|record| {
            record.message_id.as_deref() == Some("m-1")
                && record.turn_outcome == Some(TurnOutcome::Failed)
        }));
        // The buffer is spent: a later settle writes nothing new.
        assert!(chat.usage_pending.lock().unwrap().is_empty());
        settle_turn(&chat, "m-2", TurnOutcome::Succeeded);
        assert_eq!(load_records(&dir, "chat-1").unwrap().len(), 2);
        // Totals warmed from the settled batch, replay agrees.
        assert_eq!(chat.usage_totals.lock().unwrap().gross, 33);
        assert_eq!(warm_totals(&dir, "chat-1").gross, 33);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_record_outside_the_turn_model_appends_immediately() {
        let dir = temp_dir();
        let chat = ChatRuntime::load(
            &dir,
            "chat-1",
            "device",
            std::sync::Arc::new(std::sync::Mutex::new(())),
        );
        record_immediate(&chat, record(UsageKind::Title, 30, 3));
        let booked = load_records(&dir, "chat-1").unwrap();
        assert_eq!(booked.len(), 1);
        assert_eq!(booked[0].kind, UsageKind::Title);
        assert!(booked[0].turn_outcome.is_none());
        assert_eq!(
            chat.usage_totals
                .lock()
                .unwrap()
                .sum_for(UsageKind::Title)
                .input,
            30
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn capture_builds_a_turn_record_from_the_assistant_report() {
        let message = AssistantMessage {
            provider: "openai".into(),
            model: "openai/gpt-5.4".into(),
            usage: Usage {
                input: 100,
                output: 10,
                cache_read: 7,
                cache_write: 5,
                cache_write_1h: Some(3),
                reasoning: Some(2),
                cost: UsageCost {
                    input: JsF64(0.25),
                    ..Default::default()
                },
                ..Default::default()
            },
            timestamp: 1_700_000_000_123,
            ..Default::default()
        };
        let built = UsageRecord::from_assistant(&message);
        assert_eq!(built.kind, UsageKind::Turn);
        assert_eq!(built.provider, "openai");
        assert_eq!(built.model, "openai/gpt-5.4");
        assert_eq!(built.input, 100);
        assert_eq!(built.cache_read, 7);
        assert_eq!(built.cache_write_1h, Some(3));
        assert_eq!(built.reasoning, Some(2));
        assert_eq!(built.cost.input, JsF64(0.25));
        assert_eq!(built.timestamp, 1_700_000_000_123);
        assert!(built.message_id.is_none());
        assert!(built.turn_outcome.is_none());
    }
}
