//! JSON persistence for spaces/chats and the stable device id. Spaces and
//! the chat registry are written atomically (tmp + rename) under the data
//! dir; per-chat transcripts are append-only JSONL (ADR-0032).

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use holt_doc::{MessagePart, SessionMessageEntry};
use holt_proto::{Chat, Space};

use crate::EngineError;

pub(crate) fn spaces_path(data_dir: &Path) -> PathBuf {
    data_dir.join("spaces.json")
}

pub(crate) fn chats_path(data_dir: &Path) -> PathBuf {
    data_dir.join("chats.json")
}

pub(crate) fn load_chats(data_dir: &Path) -> Result<Vec<Chat>, EngineError> {
    let path = chats_path(data_dir);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            EngineError::Other(format!("could not read {}: {error}", path.display()))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn persist_chats(data_dir: &Path, chats: &[Chat]) -> Result<(), EngineError> {
    let path = chats_path(data_dir);
    let temp_path = data_dir.join(format!("chats.{}.tmp", uuid::Uuid::new_v4()));
    let bytes =
        serde_json::to_vec_pretty(chats).map_err(|error| EngineError::Other(error.to_string()))?;
    std::fs::write(&temp_path, bytes)?;
    std::fs::rename(temp_path, path)?;
    Ok(())
}

pub(crate) fn load_spaces(data_dir: &Path) -> Result<Vec<Space>, EngineError> {
    let path = spaces_path(data_dir);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            EngineError::Other(format!("could not read {}: {error}", path.display()))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn persist_spaces(data_dir: &Path, spaces: &[Space]) -> Result<(), EngineError> {
    let path = spaces_path(data_dir);
    let temp_path = data_dir.join("spaces.json.tmp");
    let bytes =
        serde_json::to_vec_pretty(spaces).map_err(|error| EngineError::Other(error.to_string()))?;
    std::fs::write(&temp_path, bytes)?;
    std::fs::rename(temp_path, path)?;
    Ok(())
}

/// The id path-safety rule shared by the per-record files (transcript,
/// History, Turn change sets — chat ids and Turn message ids alike):
/// uuid-shaped ids only — anything that could escape the file's directory
/// (path separators, dots) disables the file instead of being sanitized
/// into a colliding name.
pub(crate) fn id_is_path_safe(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Per-chat transcript snapshot — the legacy whole-file format
/// (`transcripts/<chatId>.json`), written before ADR-0032. Never written
/// anymore; read only as the replay base for chats that predate the log.
pub(crate) fn transcript_path(data_dir: &Path, chat_id: &str) -> Option<PathBuf> {
    if !id_is_path_safe(chat_id) {
        return None;
    }
    Some(data_dir.join("transcripts").join(format!("{chat_id}.json")))
}

/// Per-chat transcript file (append-only JSONL, ADR-0032): a version
/// header line, then one record per line. A record is either a full
/// `SessionMessageEntry` (an entry line whose id matches an earlier line
/// replaces it, otherwise it appends) or one of the incremental settle
/// lines — an entry's tail parts, or one part backfilled in place — that
/// put a streaming run's completed units on disk without re-appending the
/// whole entry. Every completed unit lands the moment it completes; each
/// byte of part content is therefore written once.
pub(crate) fn transcript_log_path(data_dir: &Path, chat_id: &str) -> Option<PathBuf> {
    if !id_is_path_safe(chat_id) {
        return None;
    }
    Some(
        data_dir
            .join("transcripts")
            .join(format!("{chat_id}.jsonl")),
    )
}

/// The transcript log's format version, carried by the header line.
const TRANSCRIPT_VERSION: u32 = 1;

/// One JSONL line. `Entry` is the full-snapshot upsert — an entry's first
/// landing, its terminal settle, a post-run chip re-append — and the
/// replay's self-heal anchor. `Parts` appends a run entry's tail parts;
/// `Part` backfills one resolved tool call in place. The adjacent-tag shape keeps lines self-describing for the
/// tolerant reader below; later line kinds can be added without a version
/// bump, and a reader that predates a kind skips the line.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "entry", rename_all = "camelCase")]
enum TranscriptLine {
    Entry(Box<SessionMessageEntry>),
    Parts(TranscriptParts),
    Part(TranscriptPart),
}

/// The `Parts` line's body: one entry's new tail parts, starting at `from`
/// — the entry's part count when the line was written. Replay splices only
/// when `from` still matches, so a stale or out-of-order line is dropped.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscriptParts {
    id: String,
    from: usize,
    parts: Vec<MessagePart>,
}

/// The `Part` line's body: one part's current shape, addressed by its
/// index. Indices are stable because an entry's parts grow, never shrink
/// (ADR-0011).
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscriptPart {
    id: String,
    index: usize,
    part: Box<MessagePart>,
}

/// Append one entry line, creating the file (header first) when the chat
/// has no transcript log yet. Append-only by construction — same shape and
/// crash semantics as the History record (ADR-0010): a crash between
/// create and the header write leaves a zero-byte file that must not
/// collect headerless lines, and a partial trailing record is isolated
/// from every later append.
pub(crate) fn append_transcript_entry(
    data_dir: &Path,
    chat_id: &str,
    entry: &SessionMessageEntry,
) -> std::io::Result<()> {
    let Some(path) = transcript_log_path(data_dir, chat_id) else {
        return Ok(());
    };
    let mut file = open_transcript_log(&path)?;
    write_line(&mut file, &TranscriptLine::Entry(Box::new(entry.clone())))
}

/// Append one entry's new tail parts (the per-round completed-message
/// settle): the `parts` line carries `entry.parts[from..]`.
pub(crate) fn append_transcript_parts(
    data_dir: &Path,
    chat_id: &str,
    entry_id: &str,
    from: usize,
    tail: &[MessagePart],
) -> std::io::Result<()> {
    let Some(path) = transcript_log_path(data_dir, chat_id) else {
        return Ok(());
    };
    let mut file = open_transcript_log(&path)?;
    write_line(
        &mut file,
        &TranscriptLine::Parts(TranscriptParts {
            id: entry_id.to_string(),
            from,
            parts: tail.to_vec(),
        }),
    )
}

/// Append one resolved tool call's current shape as a `part` line — the
/// per-tool settle's in-place backfill.
pub(crate) fn append_transcript_part(
    data_dir: &Path,
    chat_id: &str,
    entry_id: &str,
    index: usize,
    part: &MessagePart,
) -> std::io::Result<()> {
    let Some(path) = transcript_log_path(data_dir, chat_id) else {
        return Ok(());
    };
    let mut file = open_transcript_log(&path)?;
    write_line(
        &mut file,
        &TranscriptLine::Part(TranscriptPart {
            id: entry_id.to_string(),
            index,
            part: Box::new(part.clone()),
        }),
    )
}

/// Open the log for appending, creating it header-first when the chat has
/// no transcript log yet. Same shape and crash semantics as the History
/// record (ADR-0010): a crash between create and the header write leaves a
/// zero-byte file that must not collect headerless lines, and a partial
/// trailing record is isolated from every later append.
fn open_transcript_log(path: &Path) -> std::io::Result<std::fs::File> {
    let dir = path
        .parent()
        .expect("transcript log path has a parent")
        .to_path_buf();
    std::fs::create_dir_all(&dir)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    if file.metadata()?.len() == 0 {
        let header = serde_json::json!({ "version": TRANSCRIPT_VERSION });
        writeln!(file, "{header}")?;
    } else {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            file.write_all(b"\n")?;
        }
    }
    Ok(file)
}

fn write_line<T: serde::Serialize>(file: &mut std::fs::File, value: &T) -> std::io::Result<()> {
    let line =
        serde_json::to_string(value).map_err(|error| std::io::Error::other(error.to_string()))?;
    writeln!(file, "{line}")
}

/// Replace a chat's transcript log after a Last-message edit. The rewrite is
/// atomic so a crash cannot expose a half-pruned conversation; the legacy
/// snapshot is removed only after the new log is in place.
pub(crate) fn rewrite_transcript(
    data_dir: &Path,
    chat_id: &str,
    entries: &[SessionMessageEntry],
) -> std::io::Result<()> {
    let Some(path) = transcript_log_path(data_dir, chat_id) else {
        return Ok(());
    };
    let dir = path.parent().expect("transcript log path has a parent");
    std::fs::create_dir_all(dir)?;
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        let header = serde_json::json!({ "version": TRANSCRIPT_VERSION });
        writeln!(file, "{header}")?;
        for entry in entries {
            let line = serde_json::to_string(&TranscriptLine::Entry(Box::new(entry.clone())))
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            writeln!(file, "{line}")?;
        }
        file.sync_all()?;
        std::fs::rename(&temp, &path)?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temp);
    if result.is_ok()
        && let Some(legacy) = transcript_path(data_dir, chat_id)
    {
        let _ = std::fs::remove_file(legacy);
    }
    result
}

/// Replay one log line onto the transcript. A full entry line upserts by
/// id (a post-run settle replaces, a new id appends) and re-anchors the
/// entry wholesale. An incremental line attaches to the entry it names and
/// is dropped when it doesn't fit — a stale `from`, an out-of-range
/// index, or a missing entry — so the next full entry line self-heals
/// around whatever a crash or a downgrade skipped.
fn replay_transcript_line(transcript: &mut Vec<SessionMessageEntry>, line: &str) {
    let Ok(parsed) = serde_json::from_str::<TranscriptLine>(line) else {
        return;
    };
    match parsed {
        TranscriptLine::Entry(entry) => {
            let entry = *entry;
            match transcript
                .iter()
                .position(|existing| existing.id == entry.id)
            {
                Some(at) => transcript[at] = entry,
                None => transcript.push(entry),
            }
        }
        TranscriptLine::Parts(tail) => {
            let Some(entry) = transcript.iter_mut().find(|e| e.id == tail.id) else {
                return;
            };
            if tail.from == entry.parts.len() {
                entry.parts.extend(tail.parts);
            }
        }
        TranscriptLine::Part(backfill) => {
            let Some(entry) = transcript.iter_mut().find(|e| e.id == backfill.id) else {
                return;
            };
            if let Some(slot) = entry.parts.get_mut(backfill.index) {
                *slot = *backfill.part;
            }
        }
    }
}

pub(crate) fn load_transcript(
    data_dir: &Path,
    chat_id: &str,
) -> Result<Vec<SessionMessageEntry>, EngineError> {
    // A legacy chat's whole-file snapshot is the replay base; its log, if
    // any, applies on top. Reads are tolerant in both layers — the
    // transcript is a display record, never a correctness boundary like
    // the History.
    let mut transcript = load_legacy_snapshot(data_dir, chat_id)?;
    if let Some(path) = transcript_log_path(data_dir, chat_id)
        && let Ok(bytes) = std::fs::read(&path)
    {
        for line in String::from_utf8_lossy(&bytes).lines() {
            replay_transcript_line(&mut transcript, line);
        }
    }
    Ok(transcript)
}

/// The pre-log whole-file snapshot (`transcripts/<chatId>.json`), kept for
/// chats saved before ADR-0032. It is never written again; it stops being
/// read once the log carries the chat.
fn load_legacy_snapshot(
    data_dir: &Path,
    chat_id: &str,
) -> Result<Vec<SessionMessageEntry>, EngineError> {
    let Some(path) = transcript_path(data_dir, chat_id) else {
        return Ok(Vec::new());
    };
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            EngineError::Other(format!("could not read {}: {error}", path.display()))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// Whether the chat has any transcript record on disk — the legacy notice's
/// "a Transcript exists but no History does" check.
pub(crate) fn transcript_exists(data_dir: &Path, chat_id: &str) -> bool {
    transcript_path(data_dir, chat_id).is_some_and(|path| path.exists())
        || transcript_log_path(data_dir, chat_id).is_some_and(|path| path.exists())
}

/// Drop a chat's persisted transcript records — the log and any legacy
/// snapshot. Missing files are fine — chats that never ran have nothing on
/// disk.
pub(crate) fn delete_transcript(data_dir: &Path, chat_id: &str) {
    if let Some(path) = transcript_log_path(data_dir, chat_id) {
        let _ = std::fs::remove_file(path);
    }
    if let Some(path) = transcript_path(data_dir, chat_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Stable per-installation device id, persisted at `{data_dir}/device-id`.
pub(crate) fn load_or_create_device_id(data_dir: &Path) -> Result<String, EngineError> {
    let path = data_dir.join("device-id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim();
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::write(&path, &id)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: holt_doc::MessageRole::User,
            parts: vec![],
            created_at: 42,
            device_id: "device".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn log_path(dir: &Path, chat_id: &str) -> PathBuf {
        dir.join("transcripts").join(format!("{chat_id}.jsonl"))
    }

    fn text_part(id: &str, text: &str) -> MessagePart {
        MessagePart::Text {
            id: id.into(),
            text: text.into(),
        }
    }

    #[test]
    fn appended_entries_round_trip_and_unsafe_ids_write_nothing() {
        let dir = std::env::temp_dir().join(format!("holt-transcript-{}", uuid::Uuid::new_v4()));
        append_transcript_entry(&dir, "chat-1", &entry("m1")).expect("append");
        append_transcript_entry(&dir, "chat-1", &entry("m2")).expect("append");
        assert_eq!(
            load_transcript(&dir, "chat-1").expect("load"),
            vec![entry("m1"), entry("m2")]
        );

        // Path-hostile ids neither read nor write outside the transcripts dir.
        append_transcript_entry(&dir, "../escape", &entry("m3")).expect("append skipped");
        assert!(load_transcript(&dir, "../escape").unwrap().is_empty());
        assert!(!dir.join("transcripts").join("escape.jsonl").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_replayed_id_replaces_its_earlier_line() {
        let dir = std::env::temp_dir().join(format!("holt-transcript-{}", uuid::Uuid::new_v4()));
        append_transcript_entry(&dir, "chat-1", &entry("m1")).expect("append");
        let mut settled = entry("m1");
        settled.status = Some(holt_doc::MessageStatus::Complete);
        append_transcript_entry(&dir, "chat-1", &settled).expect("append");
        append_transcript_entry(&dir, "chat-1", &entry("m2")).expect("append");
        assert_eq!(
            load_transcript(&dir, "chat-1").expect("load"),
            vec![settled, entry("m2")]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rewrite_prunes_old_tail_and_replays_the_new_log() {
        let dir = std::env::temp_dir().join(format!("holt-transcript-{}", uuid::Uuid::new_v4()));
        append_transcript_entry(&dir, "chat-1", &entry("m1")).expect("append");
        append_transcript_entry(&dir, "chat-1", &entry("m2")).expect("append");
        rewrite_transcript(&dir, "chat-1", &[entry("m1")]).expect("rewrite");
        assert_eq!(load_transcript(&dir, "chat-1").unwrap(), vec![entry("m1")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_truncated_trailing_line_is_tolerated_and_isolated() {
        let dir = std::env::temp_dir().join(format!("holt-transcript-{}", uuid::Uuid::new_v4()));
        append_transcript_entry(&dir, "chat-1", &entry("m1")).expect("append");
        // Crash shape: a half-written line without its newline.
        let path = log_path(&dir, "chat-1");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 10);
        std::fs::write(&path, &bytes).unwrap();
        // The next append must not fuse onto the partial line.
        append_transcript_entry(&dir, "chat-1", &entry("m2")).expect("append");
        let loaded = load_transcript(&dir, "chat-1").expect("load");
        assert_eq!(loaded, vec![entry("m2")]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_legacy_snapshot_is_the_replay_base_under_the_log() {
        let dir = std::env::temp_dir().join(format!("holt-transcript-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("transcripts")).unwrap();
        let mut legacy_settled = entry("m1");
        legacy_settled.status = Some(holt_doc::MessageStatus::Complete);
        std::fs::write(
            dir.join("transcripts/chat-1.json"),
            serde_json::to_vec_pretty(&vec![entry("m1")]).unwrap(),
        )
        .unwrap();
        // The log settles the legacy entry and adds a new one.
        append_transcript_entry(&dir, "chat-1", &legacy_settled).expect("append");
        append_transcript_entry(&dir, "chat-1", &entry("m2")).expect("append");
        assert_eq!(
            load_transcript(&dir, "chat-1").expect("load"),
            vec![legacy_settled, entry("m2")]
        );
        assert!(transcript_exists(&dir, "chat-1"));
        delete_transcript(&dir, "chat-1");
        assert!(!transcript_exists(&dir, "chat-1"));
        assert!(load_transcript(&dir, "chat-1").unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn incremental_lines_replay_onto_their_entry() {
        let dir = std::env::temp_dir().join(format!("holt-transcript-{}", uuid::Uuid::new_v4()));
        let mut run = entry("m1");
        run.parts = vec![text_part("p0", "early"), text_part("p1", "round one")];
        append_transcript_entry(&dir, "chat-1", &run).expect("append");
        // A later round appends its tail, then a settle backfills p0.
        append_transcript_parts(&dir, "chat-1", "m1", 2, &[text_part("p2", "round two")])
            .expect("append parts");
        append_transcript_part(&dir, "chat-1", "m1", 0, &text_part("p0", "settled"))
            .expect("append part");
        run.parts[0] = text_part("p0", "settled");
        run.parts.push(text_part("p2", "round two"));
        assert_eq!(load_transcript(&dir, "chat-1").expect("load"), vec![run]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn misfit_incremental_lines_are_dropped() {
        let dir = std::env::temp_dir().join(format!("holt-transcript-{}", uuid::Uuid::new_v4()));
        let mut run = entry("m1");
        run.parts = vec![text_part("p0", "kept")];
        append_transcript_entry(&dir, "chat-1", &run).expect("append");
        // A stale `from` (would duplicate or gap), an out-of-range index,
        // and lines naming an entry that does not exist: all dropped.
        append_transcript_parts(&dir, "chat-1", "m1", 0, &[text_part("x", "dup")])
            .expect("append parts");
        append_transcript_parts(&dir, "chat-1", "m1", 2, &[text_part("x", "gap")])
            .expect("append parts");
        append_transcript_part(&dir, "chat-1", "m1", 5, &text_part("x", "far"))
            .expect("append part");
        append_transcript_parts(&dir, "chat-1", "ghost", 0, &[text_part("x", "lost")])
            .expect("append parts");
        append_transcript_part(&dir, "chat-1", "ghost", 0, &text_part("x", "lost"))
            .expect("append part");
        assert_eq!(load_transcript(&dir, "chat-1").expect("load"), vec![run]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_full_entry_line_re_anchors_after_incremental_lines() {
        let dir = std::env::temp_dir().join(format!("holt-transcript-{}", uuid::Uuid::new_v4()));
        let mut run = entry("m1");
        run.parts = vec![text_part("p0", "a"), text_part("p1", "b")];
        append_transcript_entry(&dir, "chat-1", &run).expect("append");
        append_transcript_parts(&dir, "chat-1", "m1", 2, &[text_part("p2", "c")])
            .expect("append parts");
        // A terminal or post-run settle re-appends the whole entry: replay
        // replaces it wholesale, however far the incremental lines got.
        run.parts.truncate(1);
        run.status = Some(holt_doc::MessageStatus::Complete);
        append_transcript_entry(&dir, "chat-1", &run).expect("append");
        assert_eq!(load_transcript(&dir, "chat-1").expect("load"), vec![run]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn incremental_appends_respect_the_path_safety_rule() {
        let dir = std::env::temp_dir().join(format!("holt-transcript-{}", uuid::Uuid::new_v4()));
        append_transcript_parts(&dir, "../escape", "m1", 0, &[text_part("x", "y")])
            .expect("append skipped");
        append_transcript_part(&dir, "../escape", "m1", 0, &text_part("x", "y"))
            .expect("append skipped");
        assert!(!dir.join("transcripts").join("escape.jsonl").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
