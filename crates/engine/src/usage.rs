//! The per-chat usage ledger (usage-ledger spec, tickets 01–02): one
//! append-only JSONL file per chat under `usage/<chatId>.jsonl`, one record
//! per metered provider round-trip — all token fields, the attribution
//! source (`kind`: Turn work, Subagent, Compaction, Auto-review, or Title
//! task), provider, model, the Turn's message id and outcome on Turn
//! records, and the upstream cost stored verbatim (Holt computes no
//! prices). The file follows the History record's durability shapes: a
//! version header line, the append atomicity pattern (header on an empty
//! file, a repaired missing final newline), and a tolerant reader that
//! skips a crash-truncated trailing line. A damaged file is quarantined
//! `.corrupt` (kept, never overwritten) and totals restart from zero —
//! bookkeeping never blocks the chat.
//!
//! Write choreography: records captured inside a Turn — the loop's own
//! round-trips (assistant message plus its tool results' usage as one
//! record), its automatic Compactions, and its auto-review passes —
//! accumulate on the chat's runtime and land as ONE batch append at
//! settlement, after queue completion. Calls outside the Turn model — the
//! Title task and manual Compaction — append immediately at completion.
//! Subagent round-trips (their internal Compaction and auto-review calls
//! included, all one `subagent` kind) are booked from the delegation's
//! billing vector into the PARENT chat's batch, stamped with the child
//! doc id; no child ledger file ever exists. Every write is
//! fire-and-forget — a failed append is logged and costs only the record,
//! never the Turn, the queue, or the terminal event. A crash before
//! settlement loses the running Turn's batch, unrepaired, by design.
//!
//! A round-trip is booked when the provider REPORTED: an aborted or errored
//! response carries its usage and books like a clean one, so an interrupted
//! call keeps whatever arrived. A request nobody ever answered (cancelled
//! mid-flight, a transport that died silently) has no report and books
//! nothing — waiting for one is exactly what the cancellation race exists
//! to avoid.
//!
//! Deleting a chat archives before it deletes: the chat's whole ledger
//! segment is appended — chat-attributed — to the device-level
//! `usage/archive.jsonl` (grow-only, the Usage overview's feed),
//! then the per-chat file and its `.corrupt` siblings are removed. Chat-level
//! data dies with the chat; the device stream survives it. Archiving is
//! best-effort — a failure logs and the delete proceeds.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pi_core::agent::types::StreamFn;
use pi_core::ai::types::{AssistantMessage, ToolResultMessage, Usage, UsageCost};

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

impl UsageKind {
    /// The wire key this kind rides in the frame's `byKind` breakdown.
    /// Pinned by test against the serde form a record's `kind` writes, so
    /// the ledger line and the frame never disagree about a source's
    /// spelling.
    pub(crate) fn kind_key(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Subagent => "subagent",
            Self::Compaction => "compaction",
            Self::AutoReview => "auto-review",
            Self::Title => "title",
        }
    }
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
    /// One round-trip from a completed assistant message — the raw report,
    /// read before any caller decides what persists, so failed and aborted
    /// answers stay billed. Turn records get their message id and outcome
    /// at settlement; subagent records carry the child doc id from their
    /// caller.
    pub(crate) fn from_message(kind: UsageKind, message: &AssistantMessage) -> Self {
        let timestamp = if message.timestamp > 0 {
            message.timestamp
        } else {
            chrono::Utc::now().timestamp_millis()
        };
        Self {
            kind,
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

    /// The record's gross token count — the four headline fields, the number
    /// a chat's total and a spawn chip's summary both show.
    pub(crate) fn gross(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    /// Fold a second usage report into this record — the round-trip's tool
    /// results, when a provider bills them separately from the assistant
    /// message.
    fn add_usage(&mut self, usage: &Usage) {
        self.input += usage.input;
        self.output += usage.output;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
        if let Some(n) = usage.cache_write_1h {
            *self.cache_write_1h.get_or_insert(0) += n;
        }
        if let Some(n) = usage.reasoning {
            *self.reasoning.get_or_insert(0) += n;
        }
        self.cost.input.0 += usage.cost.input.0;
        self.cost.output.0 += usage.cost.output.0;
        self.cost.cache_read.0 += usage.cost.cache_read.0;
        self.cost.cache_write.0 += usage.cost.cache_write.0;
        self.cost.total.0 += usage.cost.total.0;
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
    /// How many records these totals were summed from — the frame's record
    /// count, so building a frame never has to re-read the ledger file.
    pub records: u64,
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
        self.records += 1;
        self.gross += record.gross();
    }
}

/// The gross token count of an upstream report — input, output, and both
/// cache fields. Holt's one definition of "total tokens": the ledger, the
/// usage frame, and a spawn chip's summary all read it.
pub(crate) fn gross_tokens(usage: &Usage) -> u64 {
    usage.input + usage.output + usage.cache_read + usage.cache_write
}

/// The latest main-run provider report: the number the occupancy numerator
/// divides. Its request input plus both cache fields — what the provider had
/// to read and write to answer — and nothing else: a subagent's round-trips
/// never move it, because a child does not fill its parent's History.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LastReport {
    input: u64,
    cache_read: u64,
    cache_write: u64,
}

impl LastReport {
    fn of(usage: &Usage) -> Self {
        Self {
            input: usage.input,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
        }
    }

    fn tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

/// The occupancy denominator's inputs. `by_model` is the engine's catalog —
/// every model it can run, keyed by wire id — where a bare custom id maps
/// to `None` by contract: its window is a cloned template's guess, and an
/// unknown window must read as unknown rather than as a number (a live
/// model record carries its own window). `selected`
/// is the chat's own selection, which answers while its queue holds nothing
/// next.
#[derive(Clone, Debug, Default)]
pub(crate) struct OccupancyWindows {
    by_model: BTreeMap<String, Option<u64>>,
    selected: Option<String>,
}

impl OccupancyWindows {
    /// The window of the model the chat runs NEXT: the queue's item when it
    /// has one, else the chat's selection. `None` is "unknown window" — a
    /// bare custom id, or a model no catalog row covers — and the UI shows
    /// absolute tokens instead of a percentage.
    fn window_for(&self, queue_model: Option<&str>) -> Option<u64> {
        let wire = queue_model.or(self.selected.as_deref())?;
        self.by_model.get(wire).copied().flatten()
    }
}

/// The wire id the occupancy windows are keyed by. Both a provider-qualified
/// `provider/model` (what the composer sends) and a bare `model` (what a
/// stored chat config may hold) arrive here, so the provider half is
/// normalized in.
pub(crate) fn wire_model_id(provider: &str, model: &str) -> String {
    format!("{provider}/{}", model.rsplit('/').next().unwrap_or(model))
}

/// Seed the chat's occupancy tables: the engine's whole catalog plus the
/// chat's current selection. Called where a usage watch opens — the one
/// moment the engine holds both the catalog and the chat.
pub(crate) fn seed_occupancy(
    chat: &ChatRuntime,
    windows: BTreeMap<String, Option<u64>>,
    selected: Option<String>,
) {
    *chat.usage_windows.lock().unwrap_or_else(|e| e.into_inner()) = OccupancyWindows {
        by_model: windows,
        selected,
    };
}

/// Move the chat's selection — the denominator's answer while the queue
/// holds nothing next. The catalog half refreshes when a usage watch
/// (re)opens: a builtin window never moves within a process, a bare custom
/// id is unknown whether or not its settings entry is still there, and a
/// live record's window is whatever the seed captured.
pub(crate) fn set_selected_model(chat: &ChatRuntime, selected: Option<String>) {
    let changed = {
        let mut windows = chat.usage_windows.lock().unwrap_or_else(|e| e.into_inner());
        let changed = windows.selected != selected;
        windows.selected = selected;
        changed
    };
    if changed {
        // The denominator moved, so the frame moves with it: with nothing in
        // the queue next, the selection's window IS the occupancy divisor.
        publish(chat);
    }
}

/// The chat's usage frame: the ledger's running totals over settled batches
/// AND the running Turn's buffered records (so the number is live before it
/// settles), plus the occupancy of the request the chat would run next —
/// derived for display only, never persisted.
///
/// The occupancy numerator is the latest main-run report
/// ([`LastReport`]); until this process has seen one the frame falls back to
/// the History-based estimate and says so (`estimated`), which is also what
/// a restart resumes from. The denominator is the window of the model the
/// chat's queue runs next, the chat's own selection while the queue holds
/// nothing, and absent when that window is unknown (a custom model) — the
/// UI then shows absolute tokens instead of a percentage.
pub(crate) fn watch_snapshot(chat: &ChatRuntime) -> holt_proto::ChatUsage {
    let mut totals = chat
        .usage_totals
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    {
        let pending = chat.usage_pending.lock().unwrap_or_else(|e| e.into_inner());
        for record in pending.iter() {
            totals.add_record(record);
        }
    }
    let context_window = {
        // The queue's own answer, so the denominator follows the item that
        // will actually run — the executing one first, then the head.
        let next_model = {
            let queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue
                .next_request()
                .map(|request| wire_model_id(&request.provider.0, &request.model))
        };
        chat.usage_windows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .window_for(next_model.as_deref())
    };
    let report = *chat
        .usage_last_report
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let estimated = report.is_none();
    let tokens = report.map_or_else(
        || {
            pi_core::agent::harness::compaction::compaction::estimate_context_tokens(
                &chat.history.read().unwrap_or_else(|e| e.into_inner()),
            )
            .tokens as u64
        },
        |report| report.tokens(),
    );
    let by_kind = totals
        .by_kind
        .iter()
        .map(|(kind, sum)| {
            (
                kind.kind_key().to_string(),
                holt_proto::ChatUsageTokens {
                    input: sum.input,
                    output: sum.output,
                    cache_read: sum.cache_read,
                    cache_write: sum.cache_write,
                },
            )
        })
        .collect();
    holt_proto::ChatUsage {
        gross: totals.gross,
        by_kind,
        record_count: totals.records,
        occupancy: holt_proto::ChatOccupancy {
            tokens,
            context_window,
            estimated,
        },
    }
}

/// Publish the chat's frame. Every booking publishes once, and the watch
/// keeps only the latest value for a lagging subscriber, so publishing from
/// several places is cheap and lossy by design.
pub(crate) fn publish(chat: &ChatRuntime) {
    let frame = serde_json::to_value(watch_snapshot(chat)).expect("usage frame serializes");
    let _ = chat.usage_tx.send(frame);
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

/// The chat ids with a live ledger file on disk — the archive's reserved
/// name and every quarantine sibling excluded. Attribution is the file
/// name: a live chat's records carry no chat id of their own.
pub(crate) fn ledger_chat_ids(data_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(data_dir.join("usage")) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".jsonl") && id_is_path_safe(name.trim_end_matches(".jsonl")))
        .map(|name| name.trim_end_matches(".jsonl").to_string())
        // The reserved name never resolves through `usage_path`, so a
        // stray file so named is not a chat's ledger either.
        .filter(|id| id != "archive")
        .collect();
    ids.sort();
    ids
}

/// The device-level archive's readable records, best-effort: the version
/// header and any undecodable line are skipped, never fatal — the
/// aggregate counts what survives. A damaged archive is never quarantined
/// from here: stats are strictly read-only.
pub(crate) fn load_archive_records(data_dir: &Path) -> Vec<UsageRecord> {
    let bytes = match std::fs::read(archive_path(data_dir)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(target: "holt::usage", %error, "could not read the usage archive");
            return Vec::new();
        }
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<UsageRecord>(line).ok())
        .collect()
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

/// Buffer one round-trip of the running main-chat Turn, attributed to
/// `kind`, and publish the frame it moved. A child run's round-trips are not
/// booked here: the delegation's billing vector owns them
/// ([`capture_subagent_round_trip`]), so the child's buffer never becomes a
/// phantom child ledger.
fn book(chat: &ChatRuntime, kind: UsageKind, message: &AssistantMessage) {
    let record = UsageRecord::from_message(kind, message);
    chat.usage_pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(record);
    publish(chat);
}

/// Book one main-chat round-trip — the child-run guard the callers share.
fn capture(chat: &ChatRuntime, kind: UsageKind, message: &AssistantMessage) {
    if chat.child.is_some() {
        return;
    }
    book(chat, kind, message);
}

/// Buffer the Turn's own round-trip (the assistant response; a separately
/// billed tool result folds in via [`merge_tool_result`]) and take its
/// report as the occupancy numerator. The numerator is set before the frame
/// goes out, so the chat's first report flips the status line from the
/// History estimate to the measured number in the same frame.
pub(crate) fn capture_round_trip(chat: &ChatRuntime, message: &AssistantMessage) {
    if chat.child.is_some() {
        return;
    }
    *chat
        .usage_last_report
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(LastReport::of(&message.usage));
    book(chat, UsageKind::Turn, message);
}

/// Fold a tool result's usage into the round-trip it belongs to — the MOST
/// RECENT buffered Turn record, not simply the last one: an auto-review pass
/// of the same round buffers its own record between the assistant message
/// and the tool result. The `Agent` delegation result is skipped: it carries
/// the child's TOTAL, already booked per-round-trip as `subagent` records.
pub(crate) fn merge_tool_result(chat: &ChatRuntime, result: &ToolResultMessage) {
    if chat.child.is_some() || result.tool_name == "Agent" {
        return;
    }
    let Some(usage) = &result.usage else {
        return;
    };
    {
        let mut pending = chat.usage_pending.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(turn) = pending
            .iter_mut()
            .rev()
            .find(|record| record.kind == UsageKind::Turn)
        {
            turn.add_usage(usage);
        }
    }
    publish(chat);
}

/// Buffer one auto-review pass of the running Turn. A child run's reviews
/// ride its delegation's billing vector instead (all `subagent` kind).
pub(crate) fn capture_review(chat: &ChatRuntime, response: &AssistantMessage) {
    capture(chat, UsageKind::AutoReview, response);
}

/// Buffer one automatic (in-Turn) Compaction summary response — it settles
/// with the Turn's batch. Child runs compact on the delegation's metered
/// transport and never book here.
pub(crate) fn capture_compaction(chat: &ChatRuntime, response: &AssistantMessage) {
    capture(chat, UsageKind::Compaction, response);
}

/// Buffer one subagent round-trip into the PARENT chat's batch — the
/// delegation's billing vector sees every request the child caused (its own
/// Compaction and auto-review calls included, one `subagent` kind), and the
/// child doc id is the only sub-task attribution.
pub(crate) fn capture_subagent_round_trip(
    parent: &ChatRuntime,
    child_doc_id: &str,
    message: &AssistantMessage,
) {
    let mut record = UsageRecord::from_message(UsageKind::Subagent, message);
    record.subagent_doc_id = Some(child_doc_id.to_string());
    parent
        .usage_pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(record);
    // The parent's frame moves with its ledger: a child's round-trip is the
    // parent chat's spend even while the child owns the run.
    publish(parent);
}

/// Book a manual Compaction's summary response immediately — the queued
/// `/compact` runs outside the Turn model, so it never waits for a batch.
pub(crate) fn record_compaction(chat: &ChatRuntime, response: &AssistantMessage) {
    record_immediate(
        chat,
        UsageRecord::from_message(UsageKind::Compaction, response),
    );
}

/// Book the Title task's response immediately at completion — the task is
/// outside the Turn lifecycle (ADR-0012), and its chat attribution stands
/// even when the reply normalizes to no title at all.
pub(crate) fn record_title(chat: &ChatRuntime, response: &AssistantMessage) {
    record_immediate(chat, UsageRecord::from_message(UsageKind::Title, response));
}

/// Settle the finished Turn's usage: stamp the Turn's own records with its
/// message id and outcome (the Compaction and auto-review records in the
/// same batch keep their kind-only attribution), warm the totals, and land
/// the whole batch as ONE append — after queue completion,
/// fire-and-forget. A chat deleted mid-run drops its batch instead of
/// resurrecting a file (the removed check sits inside the persistence
/// lock, so a settle that waited out a concurrent delete still writes
/// nothing); a crash before this point loses it unrepaired.
pub(crate) fn settle_turn(chat: &ChatRuntime, message_id: &str, outcome: TurnOutcome) {
    let mut records =
        std::mem::take(&mut *chat.usage_pending.lock().unwrap_or_else(|e| e.into_inner()));
    if records.is_empty() {
        return;
    }
    for record in &mut records {
        if record.kind == UsageKind::Turn {
            record.message_id = Some(message_id.to_string());
            record.turn_outcome = Some(outcome);
        }
    }
    {
        // The removed check and the append stay inside the persistence lock,
        // so a settle that waited out a concurrent delete still writes
        // nothing.
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
    // The frame is built outside the lock: it reads the ledger totals, the
    // queue, and (before the first report) the History, and no other
    // persistence user should wait behind that.
    publish(chat);
}

/// Book one record from a call outside the Turn model (the Title task and
/// manual Compaction paths above): appended immediately at completion,
/// never batched, still fire-and-forget. The removed check sits inside the
/// persistence lock for the same delete-race reason as [`settle_turn`].
fn record_immediate(chat: &ChatRuntime, record: UsageRecord) {
    {
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
    publish(chat);
}

/// The completed round-trips observed through a metered transport — one
/// event stream per request, its result read (without consuming) once the
/// round-trip finished. Streams that never finished (a cancelled or hung
/// request the provider never reported on) stay pending and book nothing.
pub(crate) type Billing =
    Arc<Mutex<Vec<pi_core::ai::utils::event_stream::AssistantMessageEventStream>>>;

/// Wrap a transport so every request it serves is observed: the stream is
/// cloned into the billing vector before it flows back, results included —
/// the metering bypass subagent delegations (and, through them, the
/// children's own Compaction and auto-review calls) ride.
pub(crate) fn metered_stream(source: StreamFn) -> (StreamFn, Billing) {
    let billing = Billing::default();
    let tasks = billing.clone();
    let stream: StreamFn = Arc::new(move |model, context, options| {
        let stream = source(model, context, options)?;
        tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(stream.clone());
        Ok(stream)
    });
    (stream, billing)
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
    use pi_core::ai::types::{JsF64, ToolResultMessage, Usage};

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

    /// One kind's replayed sums — what a consumer (the frame) filters the
    /// totals by.
    fn sum(totals: &UsageTotals, kind: UsageKind) -> TokenSum {
        totals.by_kind.get(&kind).copied().unwrap_or_default()
    }

    #[test]
    fn the_frames_kind_keys_match_the_serialized_record_kinds() {
        // The frame's `byKind` map is keyed by hand while a record's `kind`
        // rides serde: the two must never drift apart.
        for kind in [
            UsageKind::Turn,
            UsageKind::Subagent,
            UsageKind::Compaction,
            UsageKind::AutoReview,
            UsageKind::Title,
        ] {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::json!(kind.kind_key()),
                "a frame key must match the record's own kind spelling"
            );
        }
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
        let turns = sum(&totals, UsageKind::Turn);
        assert_eq!((turns.input, turns.output), (101, 11));
        assert_eq!(turns.cache_read, 7);
        assert_eq!(turns.cache_write, 5);
        assert_eq!(turns.cache_write_1h, Some(3));
        assert_eq!(turns.reasoning, Some(2));
        assert_eq!(sum(&totals, UsageKind::Compaction).input, 50);
        assert_eq!(sum(&totals, UsageKind::Title).input, 20);
        assert_eq!(sum(&totals, UsageKind::Subagent), TokenSum::default());
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
        assert_eq!(sum(&warm_totals(&dir, "chat-1"), UsageKind::Turn).input, 1);

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

    /// A retired kind's ledger lines (the removed `jev-review`, ADR-0027)
    /// skip with a warning — they never block the ledger's other records
    /// or a startup.
    #[test]
    fn a_retired_kinds_lines_skip_without_blocking_the_ledger() {
        let dir = temp_dir();
        append_records(&dir, "chat-1", &[record(UsageKind::Turn, 5, 1)]).unwrap();
        let path = usage_path(&dir, "chat-1").unwrap();
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(
            b"{\"kind\":\"jev-review\",\"provider\":\"typesafe\",\"model\":\"jev-latest\",\"input\":330,\"output\":34,\"cacheRead\":0,\"cacheWrite\":0,\"cost\":{\"input\":0.0,\"output\":0.0,\"cacheRead\":0.0,\"cacheWrite\":0.0},\"timestamp\":1}\n",
        )
        .unwrap();
        drop(file);
        append_records(&dir, "chat-1", &[record(UsageKind::Turn, 7, 2)]).unwrap();
        let loaded = load_records(&dir, "chat-1").unwrap();
        assert_eq!(
            loaded.iter().map(|record| record.kind).collect::<Vec<_>>(),
            vec![UsageKind::Turn, UsageKind::Turn],
            "the retired line drops, its neighbors replay"
        );
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
        let chat = ChatRuntime::load(&dir, "chat-1", "device");
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
        let chat = ChatRuntime::load(&dir, "chat-1", "device");
        record_immediate(&chat, record(UsageKind::Title, 30, 3));
        let booked = load_records(&dir, "chat-1").unwrap();
        assert_eq!(booked.len(), 1);
        assert_eq!(booked[0].kind, UsageKind::Title);
        assert!(booked[0].turn_outcome.is_none());
        assert_eq!(
            sum(&chat.usage_totals.lock().unwrap(), UsageKind::Title).input,
            30
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn settling_stamps_only_the_turns_own_records() {
        let dir = temp_dir();
        let chat = ChatRuntime::load(&dir, "chat-1", "device");
        let mut compaction = record(UsageKind::Compaction, 40, 4);
        compaction.message_id = None;
        compaction.turn_outcome = None;
        chat.usage_pending
            .lock()
            .unwrap()
            .extend([record(UsageKind::Turn, 10, 1), compaction]);

        settle_turn(&chat, "m-1", TurnOutcome::Interrupted);

        let settled = load_records(&dir, "chat-1").unwrap();
        assert_eq!(settled.len(), 2);
        assert_eq!(
            (settled[0].message_id.as_deref(), settled[0].turn_outcome),
            (Some("m-1"), Some(TurnOutcome::Interrupted)),
            "the Turn's own record carries the stamp"
        );
        assert_eq!(
            (settled[1].message_id.as_deref(), settled[1].turn_outcome),
            (None, None),
            "a batched Compaction record keeps its kind-only attribution"
        );
        // Totals span both kinds.
        assert_eq!(chat.usage_totals.lock().unwrap().gross, 55);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_tool_results_usage_merges_into_its_round_trip() {
        let dir = temp_dir();
        let chat = ChatRuntime::load(&dir, "chat-1", "device");
        chat.usage_pending
            .lock()
            .unwrap()
            .push(record(UsageKind::Turn, 10, 1));
        let billed = ToolResultMessage {
            tool_call_id: "c-1".into(),
            tool_name: "bash".into(),
            usage: Some(Usage {
                input: 5,
                output: 2,
                cache_read: 1,
                ..Default::default()
            }),
            ..Default::default()
        };
        merge_tool_result(&chat, &billed);
        let merged = chat.usage_pending.lock().unwrap();
        assert_eq!(merged.len(), 1, "same round-trip, one record");
        assert_eq!(
            (merged[0].input, merged[0].output, merged[0].cache_read),
            (15, 3, 1)
        );

        // The delegation result carries the child's total — booked per
        // round-trip as subagent records, never merged into the parent's.
        let spawn = ToolResultMessage {
            tool_name: "Agent".into(),
            usage: Some(Usage {
                input: 100,
                ..Default::default()
            }),
            ..Default::default()
        };
        merge_tool_result(&chat, &spawn);
        assert_eq!(merged[0].input, 15);
    }

    /// An auto-review pass of the same round buffers its record between the
    /// assistant message and the tool result. The merge must reach past it
    /// to the round's Turn record — merging into the last record would land
    /// a tool result's usage on the reviewer's.
    #[test]
    fn a_tool_result_merges_past_an_interleaved_review_record() {
        let dir = temp_dir();
        let chat = ChatRuntime::load(&dir, "chat-1", "device");
        chat.usage_pending.lock().unwrap().extend([
            record(UsageKind::Turn, 10, 1),
            record(UsageKind::AutoReview, 90, 9),
        ]);
        let billed = ToolResultMessage {
            tool_call_id: "c-1".into(),
            tool_name: "bash".into(),
            usage: Some(Usage {
                input: 5,
                ..Default::default()
            }),
            ..Default::default()
        };

        merge_tool_result(&chat, &billed);

        let pending = chat.usage_pending.lock().unwrap();
        assert_eq!(pending.len(), 2);
        let by_kind = |kind| pending.iter().find(|record| record.kind == kind).unwrap();
        assert_eq!(by_kind(UsageKind::Turn).input, 15, "the round-trip took it");
        assert_eq!(
            by_kind(UsageKind::AutoReview).input,
            90,
            "the reviewer's record is untouched"
        );
        drop(pending);
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
        let built = UsageRecord::from_message(UsageKind::Turn, &message);
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
