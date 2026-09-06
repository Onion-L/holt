//! Managed images and the served image surface: pasted screenshots saved as
//! Holt-owned local files ("Managed image" in the glossary), bounded reads for
//! the UI's previews, and cleanup that reconciles durable references before
//! reclaiming anything.
//!
//! A managed image is a *path provider*, not an upload: the bytes land under
//! `<data_dir>/images/<uuid>.<ext>` once, durably, before the referencing
//! message is ever acknowledged, and the file never moves afterwards — the
//! path inside a draft, a queued item, the Transcript, or History stays valid.
//! External files selected through the picker or drag are never candidates for
//! cleanup; only files directly inside the managed root (with the naming shape
//! this module issues) are Holt-owned.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;

pub(crate) mod codec;

pub(crate) static PROCESSING: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(2)));

/// Hard cap on any image the UI stages or previews, enforced BEFORE the bytes
/// move (metadata check) — the same budget the read tool's image path honors
/// (`tools::read_binary_file`). Generous for screenshots; past it, the honest
/// answer is an error, not an unbounded allocation.
pub const MAX_IMAGE_BYTES: u64 = 25 * 1024 * 1024;

/// The image formats this feature supports end to end (UI preview and model
/// input through the read tool). GIF and animated WebP carry their first
/// frame only; everything else (APNG, BMP, SVG, TIFF, HEIC, …) stays an
/// ordinary path reference with no promised preview or visual read.
pub const SUPPORTED_MIME_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// The managed-image root: `<data_dir>/images`.
pub struct ImageStore {
    dir: PathBuf,
    data_dir: PathBuf,
}

pub use holt_rpc::images::ManagedImage as StagedImage;

impl ImageStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("images"),
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// The managed root (absolute once the data dir is).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// True when `path` points at a file the store could have issued: a
    /// child of the managed root with a managed extension. Works for paths
    /// whose file is already gone (the parent is canonicalized instead), so
    /// releasing a reclaimed file stays truthful — and never true for an
    /// external picker/drag source, so cleanup can only ever consider
    /// Holt-owned files.
    pub fn is_managed(&self, path: &str) -> bool {
        let candidate = Path::new(path);
        let Ok(dir) = std::fs::canonicalize(&self.dir) else {
            return false;
        };
        let Some(parent) = candidate.parent() else {
            return false;
        };
        let Ok(canonical_parent) = std::fs::canonicalize(parent) else {
            return false;
        };
        canonical_parent == dir
            && candidate
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
            && candidate
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| matches!(ext, "png" | "jpg" | "gif" | "webp"))
    }

    /// Validate pasted bytes and persist them durably before the caller
    /// acknowledges anything that references the returned path. The format
    /// claim (from the clipboard) is verified against the actual bytes — the
    /// sniffed type wins for the file extension, because the agent's read tool
    /// and the providers judge content, not labels.
    pub async fn stage(&self, bytes: Vec<u8>) -> Result<StagedImage, String> {
        if bytes.is_empty() {
            return Err("The pasted image is empty.".into());
        }
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(format!(
                "The pasted image is too large ({} MB limit).",
                MAX_IMAGE_BYTES / (1024 * 1024)
            ));
        }
        let permit = PROCESSING.clone().acquire_owned().await.unwrap();
        let (mime, bytes) = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            codec::decode(&bytes)?;
            let mime =
                sniff_mime_type(&bytes).ok_or_else(|| "Unsupported image format.".to_string())?;
            Ok::<_, String>((mime, bytes))
        })
        .await
        .map_err(|error| format!("image staging failed: {error}"))??;
        let extension = mime.strip_prefix("image/").unwrap_or("png");
        let extension = if extension == "jpeg" {
            "jpg"
        } else {
            extension
        };
        let file_name = format!("{}.{}", uuid::Uuid::new_v4(), extension);
        let target = self.dir.join(&file_name);
        std::fs::create_dir_all(&self.dir)
            .map_err(|error| format!("could not create the image store: {error}"))?;
        std::fs::File::open(&self.data_dir)
            .and_then(|dir| dir.sync_all())
            .map_err(|error| format!("could not sync the image store: {error}"))?;
        let dir = self.dir.clone();
        let write_target = target.clone();
        tokio::task::spawn_blocking(move || write_durably(&dir, &write_target, &bytes))
            .await
            .map_err(|error| format!("image staging failed: {error}"))?
            .map_err(|error| format!("could not save the pasted image: {error}"))?;
        Ok(StagedImage {
            path: std::fs::canonicalize(target)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .into_owned(),
            mime_type: mime.to_string(),
        })
    }

    /// Read an image for UI preview: bounded (metadata-checked before the
    /// read), format-sniffed, and honest about missing/unreadable/unsupported
    /// targets. Works for any local path — managed or an external live
    /// reference; the reply carries base64 for the ndjson transport.
    pub async fn read(&self, path: &str) -> Result<(String, String), String> {
        let target = PathBuf::from(path);
        let metadata = std::fs::metadata(&target).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                format!("Image file not found: {path}")
            }
            _ => format!("Image file could not be read: {error}"),
        })?;
        if metadata.len() > MAX_IMAGE_BYTES {
            return Err(format!(
                "Image is too large to preview ({} MB limit).",
                MAX_IMAGE_BYTES / (1024 * 1024)
            ));
        }
        let permit = PROCESSING.clone().acquire_owned().await.unwrap();
        let bytes = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let bytes = codec::read_bounded(&target)?;
            codec::decode(&bytes)?;
            Ok::<_, String>(bytes)
        })
        .await
        .map_err(|error| format!("image read failed: {error}"))?
        .map_err(|error| format!("Image file could not be read: {error}"))?;
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(format!(
                "Image is too large to preview ({} MB limit).",
                MAX_IMAGE_BYTES / (1024 * 1024)
            ));
        }
        // One blocking pass: sniff the bytes and encode the reply payload.
        let (mime, data) = tokio::task::spawn_blocking(move || {
            let mime = sniff_mime_type(&bytes).ok_or_else(|| {
                "Not a supported image format (PNG, JPEG, GIF, or WebP).".to_string()
            })?;
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Ok::<(String, String), String>((mime.to_string(), data))
        })
        .await
        .map_err(|error| format!("image read failed: {error}"))??;
        Ok((mime, data))
    }

    /// Release a managed file a draft chip no longer needs. Files still
    /// referenced by durable work (any chat's queue, Transcript, or History)
    /// are kept — removing one use must not invalidate another. Returns
    /// whether nothing needs the file anymore; a non-managed path is refused
    /// so an external source can never be reclaimed through this RPC.
    pub async fn release(&self, path: &str) -> Result<bool, String> {
        if !self.is_managed(path) {
            return Ok(false);
        }
        let referenced = self.referenced_names().await;
        let Some(name) = Path::new(path).file_name().and_then(|n| n.to_str()) else {
            return Ok(false);
        };
        if referenced.contains(name) {
            return Ok(false);
        }
        let removed = tokio::task::spawn_blocking({
            let path = path.to_string();
            move || std::fs::remove_file(&path)
        })
        .await
        .map_err(|join| format!("image release failed: {join}"))?;
        match removed {
            Ok(()) => Ok(true),
            // Already gone: the goal (no file) holds.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(format!("could not remove the image: {error}")),
        }
    }

    /// Every managed file name still referenced by durable stores: queue
    /// records (pending, started, and accepted ids), Transcripts, and
    /// History. Names (not full paths) are the needle — the data dir itself
    /// can move between runs while the file names stay unique.
    async fn referenced_names(&self) -> HashSet<String> {
        let candidates = self.list_file_names();
        if candidates.is_empty() {
            return HashSet::new();
        }
        tokio::task::spawn_blocking({
            let store = self.clone_for_scan();
            move || store.scan_references(&candidates)
        })
        .await
        .unwrap_or_else(|_| self.list_file_names().into_iter().collect())
    }

    /// A plain clone for the blocking scan (the store is just two paths).
    fn clone_for_scan(&self) -> Self {
        Self {
            dir: self.dir.clone(),
            data_dir: self.data_dir.clone(),
        }
    }

    fn list_file_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return names;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_file())
                && self.is_managed(&entry.path().to_string_lossy())
            {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        names
    }

    /// Restart/crash reconciliation: reclaim managed files no durable store
    /// references. A crash between staging an image and the enqueue reply
    /// leaves it unreferenced (the draft was memory-only) — safe to reclaim.
    /// A crash AFTER the queue accepted the message keeps the file, because
    /// the durable queue record names it. Returns the reclaimed names.
    pub async fn cleanup_unreferenced(&self) -> Vec<String> {
        let referenced = self.referenced_names().await;
        let mut reclaimed = Vec::new();
        for name in self.list_file_names() {
            if referenced.contains(&name) {
                continue;
            }
            if std::fs::remove_file(self.dir.join(&name)).is_ok() {
                reclaimed.push(name);
            }
        }
        reclaimed
    }
    /// Substring scan of the durable stores for any of `candidates`.
    fn scan_references(&self, candidates: &[String]) -> HashSet<String> {
        let mut referenced = HashSet::new();
        if candidates.is_empty() {
            return referenced;
        }
        for dir in ["queues", "history", "transcripts"] {
            let entries = match std::fs::read_dir(self.data_dir.join(dir)) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return candidates.iter().cloned().collect(),
            };
            for entry in entries {
                let Ok(entry) = entry else {
                    return candidates.iter().cloned().collect();
                };
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(&path) else {
                    // Unreadable store: retain everything rather than guess —
                    // an unreadable queue is blocked, not overwritten, elsewhere.
                    referenced.extend(candidates.iter().cloned());
                    continue;
                };
                fn retain_strings(
                    value: &serde_json::Value,
                    candidates: &[String],
                    referenced: &mut HashSet<String>,
                ) {
                    match value {
                        serde_json::Value::String(text) => {
                            for name in candidates {
                                if text.contains(name) {
                                    referenced.insert(name.clone());
                                }
                            }
                        }
                        serde_json::Value::Array(values) => {
                            for value in values {
                                retain_strings(value, candidates, referenced);
                            }
                        }
                        serde_json::Value::Object(values) => {
                            for value in values.values() {
                                retain_strings(value, candidates, referenced);
                            }
                        }
                        _ => {}
                    }
                }
                let records =
                    serde_json::Deserializer::from_str(&content).into_iter::<serde_json::Value>();
                for record in records {
                    match record {
                        Ok(value) => retain_strings(&value, candidates, &mut referenced),
                        Err(_) => {
                            referenced.extend(candidates.iter().cloned());
                            break;
                        }
                    }
                }
            }
        }
        referenced
    }
}

/// Durable write: create-new temp, fsync, rename, fsync the directory — the
/// credentials-store pattern. A crash leaves either the old directory or the
/// complete new file, never a truncated image under its final name.
fn write_durably(dir: &Path, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, target)?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Magic-byte sniffing over pi-core's detector (the same judgment the read
/// tool applies), narrowed to the formats this feature supports — BMP is
/// detected upstream but carries no preview/read promise here.
pub(crate) fn sniff_mime_type(bytes: &[u8]) -> Option<&'static str> {
    let mime = pi_core::agent::harness::tools::image::detect_supported_image_mime_type(bytes)?;
    SUPPORTED_MIME_TYPES
        .into_iter()
        .find(|supported| *supported == mime)
}

/// Shared engine assembly: the store plus the boot-time reconciliation task.
pub fn assemble(data_dir: &Path) -> Arc<ImageStore> {
    let store = Arc::new(ImageStore::new(data_dir));
    // Assembly precedes serving RPCs: no newly staged file can race cleanup.
    let candidates = store.list_file_names();
    let referenced = store.scan_references(&candidates);
    for name in candidates {
        if !referenced.contains(&name) {
            let _ = std::fs::remove_file(store.dir.join(name));
        }
    }
    store
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes() -> Vec<u8> {
        let mut output = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(2, 1, image::Rgba([255, 0, 0, 255]))
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    fn jpeg_bytes() -> Vec<u8> {
        let mut output = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(2, 1, image::Rgb([255, 0, 0]))
            .write_to(&mut output, image::ImageFormat::Jpeg)
            .unwrap();
        output.into_inner()
    }

    fn store(dir: &Path) -> ImageStore {
        std::fs::create_dir_all(dir.join("images")).unwrap();
        ImageStore::new(dir)
    }

    #[tokio::test]
    async fn stage_verifies_content_and_persists_durably() {
        let dir = tempfile::tempdir().unwrap();
        let images = store(dir.path());
        let staged = images.stage(png_bytes()).await.expect("png stages");
        assert!(staged.path.ends_with(".png"));
        assert_eq!(staged.mime_type, "image/png");
        assert!(Path::new(&staged.path).exists());
        assert!(images.is_managed(&staged.path));
        // A text file wearing a .png claim must not stage.
        let text = b"definitely not an image".to_vec();
        assert!(images.stage(text).await.is_err());
        // Empty content is rejected before any file exists.
        assert!(images.stage(Vec::new()).await.is_err());
        let files: Vec<_> = std::fs::read_dir(dir.path().join("images"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 1, "failed stages leave no files: {files:?}");
    }

    #[tokio::test]
    async fn read_reports_missing_unsupported_and_supported_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let images = store(dir.path());
        let missing = images.read("/nonexistent/never.png").await;
        assert!(missing.is_err());
        assert!(
            missing.unwrap_err().contains("not found"),
            "missing is distinct"
        );
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, b"plain text").unwrap();
        let unsupported = images.read(text.to_str().unwrap()).await.unwrap_err();
        assert!(unsupported.contains("Unsupported image"), "{unsupported}");
        let staged = images.stage(jpeg_bytes()).await.unwrap();
        let (mime, data) = images.read(&staged.path).await.unwrap();
        assert_eq!(mime, "image/jpeg");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap();
        assert_eq!(decoded, jpeg_bytes());
    }

    #[tokio::test]
    async fn release_refuses_external_paths_and_referenced_managed_files() {
        let dir = tempfile::tempdir().unwrap();
        let images = store(dir.path());
        let external = dir.path().join("source.png");
        std::fs::write(&external, png_bytes()).unwrap();
        // An external source file is never a cleanup candidate.
        assert_eq!(images.release(external.to_str().unwrap()).await, Ok(false));
        assert!(external.exists());

        let staged = images.stage(png_bytes()).await.unwrap();
        // Referenced by a durable queue record: release keeps it.
        std::fs::create_dir_all(dir.path().join("queues")).unwrap();
        std::fs::write(
            dir.path().join("queues").join("chat.json"),
            format!("{{\"prompt\":\"see {}\"}}", staged.path),
        )
        .unwrap();
        assert_eq!(images.release(&staged.path).await, Ok(false));
        assert!(Path::new(&staged.path).exists());
        // Once the record goes, release reclaims.
        std::fs::remove_file(dir.path().join("queues").join("chat.json")).unwrap();
        assert_eq!(images.release(&staged.path).await, Ok(true));
        assert!(!Path::new(&staged.path).exists());
        // Releasing an already-absent managed path is still success.
        assert_eq!(images.release(&staged.path).await, Ok(true));
    }

    #[tokio::test]
    async fn cleanup_reconciles_queues_history_and_transcripts() {
        let dir = tempfile::tempdir().unwrap();
        let images = store(dir.path());
        for sub in ["queues", "history", "transcripts"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        let queued = images.stage(png_bytes()).await.unwrap();
        let sent = images.stage(png_bytes()).await.unwrap();
        let unsent = images.stage(png_bytes()).await.unwrap();
        std::fs::write(
            dir.path().join("queues").join("chat-a.json"),
            format!("\"{}\"", queued.path),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("history").join("chat-a.jsonl"),
            format!("{{\"text\":\"{}\"}}\n", sent.path),
        )
        .unwrap();
        // A transcript may store an escaped JSON path; the file NAME needle
        // still matches.
        std::fs::write(
            dir.path().join("transcripts").join("chat-a.json"),
            format!("{{\"text\":\"look at {} please\"}}", sent.path),
        )
        .unwrap();
        let reclaimed = images.cleanup_unreferenced().await;
        assert_eq!(
            reclaimed,
            vec![
                Path::new(&unsent.path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            ]
        );
        assert!(
            Path::new(&queued.path).exists(),
            "queued item keeps its image"
        );
        assert!(
            Path::new(&sent.path).exists(),
            "sent image stays with its chat"
        );
        assert!(!Path::new(&unsent.path).exists());
    }

    #[tokio::test]
    async fn cleanup_keeps_everything_when_a_store_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let images = store(dir.path());
        std::fs::create_dir_all(dir.path().join("queues")).unwrap();
        let staged = images.stage(png_bytes()).await.unwrap();
        std::fs::write(dir.path().join("queues").join("chat.json"), b"\xff\xfe\xfa").unwrap();
        let reclaimed = images.cleanup_unreferenced().await;
        assert!(reclaimed.is_empty(), "no reclaim on an unreadable store");
        assert!(Path::new(&staged.path).exists());
    }

    #[tokio::test]
    async fn oversized_stage_is_rejected_before_any_file_lands() {
        let dir = tempfile::tempdir().unwrap();
        let images = store(dir.path());
        let big = vec![0u8; (MAX_IMAGE_BYTES + 1) as usize];
        let err = images.stage(big).await.unwrap_err();
        assert!(err.contains("too large"), "{err}");
        assert!(
            std::fs::read_dir(dir.path().join("images"))
                .unwrap()
                .flatten()
                .next()
                .is_none()
        );
    }
}
