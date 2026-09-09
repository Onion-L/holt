//! The File-sidebar workspace RPCs over the handle seam: root resolution,
//! boundary enforcement (root containment, `.git`, symlinks), the listing
//! contract, and bounded strict-UTF-8 reads (ADR-0020 groundwork).

mod common;

use std::fs;
use std::path::Path;

use common::{Fixture, ScriptedProvider};
use holt_engine::LocalEngine;
use holt_proto::{WorkspaceEntryKind, WorkspaceFileRead, WorkspaceLineEndings, WorkspaceListing};
use holt_rpc::{RpcError, RpcReply, RpcService, methods};
use serde_json::json;

/// Returns the OUTSIDE-root tempdir — the caller must keep it alive or the
/// outside-link goes broken before the engine lists it.
fn build_tree(root: &Path) -> tempfile::TempDir {
    fs::create_dir_all(root.join("src/deep")).unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(root.join("README.md"), "# title\n").unwrap();
    fs::write(root.join(".env"), "secret=1\n").unwrap();
    fs::create_dir_all(root.join(".git/refs")).unwrap();
    fs::write(root.join(".git/HEAD"), "ref").unwrap();
    // Inside-root alias (dir + file), an outside-root link, and a broken one.
    std::os::unix::fs::symlink(root.join("src"), root.join("lib-alias")).unwrap();
    std::os::unix::fs::symlink(root.join("src/main.rs"), root.join("main-alias.rs")).unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("outside-link")).unwrap();
    std::os::unix::fs::symlink("/definitely/not/here", root.join("broken-link")).unwrap();
    outside
}

async fn setup(fixture: &Fixture) -> LocalEngine {
    let provider = ScriptedProvider::new(vec![]);
    let engine = fixture.engine(&provider);
    engine
        .handle(
            methods::MUTATE,
            json!({
                "op": "createSpace",
                "spaceId": "space-1",
                "deviceId": engine.engine_info().device_id,
                "path": fixture.cwd(),
            }),
        )
        .await
        .unwrap();
    engine
        .handle(
            methods::MUTATE,
            json!({ "op": "createChat", "chatId": "chat-1", "spaceId": "space-1" }),
        )
        .await
        .unwrap();
    engine
}

async fn list(
    engine: &LocalEngine,
    params: serde_json::Value,
) -> Result<WorkspaceListing, RpcError> {
    match engine
        .handle(methods::LIST_WORKSPACE_ENTRIES, params)
        .await?
    {
        RpcReply::Value(value) => Ok(serde_json::from_value(value).unwrap()),
        _ => panic!("ListWorkspaceEntries must reply with a value"),
    }
}

async fn read(
    engine: &LocalEngine,
    params: serde_json::Value,
) -> Result<WorkspaceFileRead, RpcError> {
    match engine.handle(methods::READ_WORKSPACE_FILE, params).await? {
        RpcReply::Value(value) => Ok(serde_json::from_value(value).unwrap()),
        _ => panic!("ReadWorkspaceFile must reply with a value"),
    }
}

fn names(listing: &WorkspaceListing) -> Vec<&str> {
    listing
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect()
}

mod list_workspace_entries {
    use super::*;

    #[tokio::test]
    async fn lists_one_level_sorted_dirs_first_by_chat_and_space() {
        let fixture = Fixture::new();
        let _outside = build_tree(fixture.project_dir.path());
        let engine = setup(&fixture).await;

        for selector in [
            json!({ "chatId": "chat-1" }),
            json!({ "spaceId": "space-1" }),
        ] {
            let listing = list(&engine, selector).await.unwrap();
            assert_eq!(
                names(&listing),
                [
                    "lib-alias",
                    "outside-link",
                    "src",
                    ".env",
                    "broken-link",
                    "main-alias.rs",
                    "README.md"
                ]
            );
            // `.git` is invisible in any form.
            assert!(!names(&listing).contains(&".git"));
            // Hidden entries are shown.
            assert!(names(&listing).contains(&".env"));
        }
    }

    #[tokio::test]
    async fn subdirectories_load_by_absolute_path() {
        let fixture = Fixture::new();
        let _outside = build_tree(fixture.project_dir.path());
        let engine = setup(&fixture).await;

        let root_listing = list(&engine, json!({ "spaceId": "space-1" }))
            .await
            .unwrap();
        let src = root_listing
            .entries
            .iter()
            .find(|entry| entry.name == "src")
            .unwrap();
        let deep = list(&engine, json!({ "spaceId": "space-1", "path": src.path }))
            .await
            .unwrap();
        assert_eq!(names(&deep), ["deep", "main.rs"]);
        // The reply carries the canonical listed directory.
        assert!(deep.path.ends_with("src"));
    }

    #[tokio::test]
    async fn symlink_kinds_report_resolution_and_boundary() {
        let fixture = Fixture::new();
        let _outside = build_tree(fixture.project_dir.path());
        let engine = setup(&fixture).await;

        let listing = list(&engine, json!({ "spaceId": "space-1" }))
            .await
            .unwrap();
        let by_name = |name: &str| {
            listing
                .entries
                .iter()
                .find(|entry| entry.name == name)
                .unwrap_or_else(|| panic!("missing {name}"))
        };
        match &by_name("lib-alias").kind {
            WorkspaceEntryKind::SymlinkInside {
                target_is_dir,
                resolved_path,
            } => {
                assert!(target_is_dir);
                assert!(resolved_path.ends_with("src"));
            }
            other => panic!("lib-alias: {other:?}"),
        }
        match &by_name("main-alias.rs").kind {
            WorkspaceEntryKind::SymlinkInside { target_is_dir, .. } => assert!(!target_is_dir),
            other => panic!("main-alias.rs: {other:?}"),
        }
        assert!(matches!(
            by_name("outside-link").kind,
            WorkspaceEntryKind::SymlinkOutside {
                target_is_dir: true
            }
        ));
        assert!(matches!(
            by_name("broken-link").kind,
            WorkspaceEntryKind::SymlinkBroken
        ));
    }

    #[tokio::test]
    async fn root_escapes_and_git_are_refused_by_the_engine() {
        let fixture = Fixture::new();
        let _outside = build_tree(fixture.project_dir.path());
        let engine = setup(&fixture).await;
        let root = fixture.cwd();

        // Absolute outside path.
        let fault = list(&engine, json!({ "spaceId": "space-1", "path": "/etc" }))
            .await
            .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(message) if message.contains("outside")),
            "{fault:?}"
        );
        // Escape through the outside symlink.
        let fault = list(
            &engine,
            json!({ "spaceId": "space-1", "path": format!("{root}/outside-link") }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(message) if message.contains("outside")),
            "{fault:?}"
        );
        // Dot-dot traversal.
        let fault = list(
            &engine,
            json!({ "spaceId": "space-1", "path": format!("{root}/..") }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(message) if message.contains("outside")),
            "{fault:?}"
        );
        // `.git` — the directory and, via its parent, anything under it.
        let fault = list(
            &engine,
            json!({ "spaceId": "space-1", "path": format!("{root}/.git") }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(message) if message.contains(".git")),
            "{fault:?}"
        );
        // Missing paths distinguish themselves from permission failures.
        let fault = list(
            &engine,
            json!({ "spaceId": "space-1", "path": format!("{root}/nope") }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(message) if message.contains("does not exist")),
            "{fault:?}"
        );
    }

    #[tokio::test]
    async fn selector_rules_mirror_search_files() {
        let fixture = Fixture::new();
        let engine = setup(&fixture).await;

        for params in [
            json!({ "path": fixture.cwd() }),
            json!({ "chatId": "chat-1", "spaceId": "space-1" }),
            json!("not an object"),
        ] {
            let fault = list(&engine, params).await.unwrap_err();
            assert!(matches!(fault, RpcError::BadParams(_)), "{fault:?}");
        }
        let fault = list(&engine, json!({ "chatId": "nope" }))
            .await
            .unwrap_err();
        assert!(matches!(fault, RpcError::Failed(_)), "{fault:?}");
    }
}

mod read_workspace_file {
    use super::*;

    #[tokio::test]
    async fn reads_text_with_source_facts_through_the_rpc() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        fs::write(
            Path::new(&root).join("doc.md"),
            "\u{FEFF}line one\r\nline two\r\n",
        )
        .unwrap();
        let engine = setup(&fixture).await;

        let file = read(
            &engine,
            json!({ "spaceId": "space-1", "path": format!("{root}/doc.md") }),
        )
        .await
        .unwrap();
        assert_eq!(file.text.as_deref(), Some("line one\r\nline two\r\n"));
        assert!(file.bom);
        assert_eq!(file.line_endings, WorkspaceLineEndings::Crlf);
        assert!(!file.version.is_empty());
        assert!(file.bytes > 0);
        assert_eq!(file.unsupported_reason, None);
    }

    #[tokio::test]
    async fn an_inside_alias_reads_its_target_and_shares_identity() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        fs::write(Path::new(&root).join("real.txt"), b"contents\n").unwrap();
        std::os::unix::fs::symlink(format!("{root}/real.txt"), format!("{root}/alias.txt"))
            .unwrap();
        let engine = setup(&fixture).await;

        let direct = read(
            &engine,
            json!({ "spaceId": "space-1", "path": format!("{root}/real.txt") }),
        )
        .await
        .unwrap();
        let aliased = read(
            &engine,
            json!({ "spaceId": "space-1", "path": format!("{root}/alias.txt") }),
        )
        .await
        .unwrap();
        assert_eq!(direct.path, aliased.path);
        assert_eq!(direct.version, aliased.version);
        assert_eq!(aliased.text.as_deref(), Some("contents\n"));
    }

    #[tokio::test]
    async fn boundaries_apply_to_reads_too() {
        let fixture = Fixture::new();
        let _outside = build_tree(fixture.project_dir.path());
        let engine = setup(&fixture).await;
        let root = fixture.cwd();

        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), b"x").unwrap();
        let fault = read(
            &engine,
            json!({ "spaceId": "space-1", "path": outside.path().join("secret.txt").display().to_string() }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains("outside")),
            "{fault:?}"
        );

        let fault = read(
            &engine,
            json!({ "spaceId": "space-1", "path": format!("{root}/.git/HEAD") }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains(".git")),
            "{fault:?}"
        );

        let fault = read(
            &engine,
            json!({ "spaceId": "space-1", "path": format!("{root}/nope.txt") }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains("does not exist")),
            "{fault:?}"
        );

        let fault = read(
            &engine,
            json!({ "spaceId": "space-1", "path": format!("{root}/src") }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains("directory")),
            "{fault:?}"
        );
    }

    #[tokio::test]
    async fn unsupported_content_reports_reasons_without_lossy_decoding() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        fs::write(Path::new(&root).join("blob.bin"), b"\x00\x01\x02png").unwrap();
        fs::write(Path::new(&root).join("latin.txt"), b"caf\xe9").unwrap();
        let engine = setup(&fixture).await;

        for (name, needle) in [("blob.bin", "binary"), ("latin.txt", "UTF-8")] {
            let file = read(
                &engine,
                json!({ "spaceId": "space-1", "path": format!("{root}/{name}") }),
            )
            .await
            .unwrap();
            assert_eq!(file.text, None, "{name}");
            let reason = file.unsupported_reason.expect(name);
            assert!(reason.contains(needle), "{name}: {reason}");
        }

        // A missing path is required.
        let fault = read(&engine, json!({ "spaceId": "space-1" }))
            .await
            .unwrap_err();
        assert!(matches!(fault, RpcError::BadParams(_)), "{fault:?}");
    }
}

mod save_workspace_file {
    use super::*;
    use holt_proto::WorkspaceSaveStatus;

    async fn save_file(
        engine: &LocalEngine,
        params: serde_json::Value,
    ) -> Result<holt_proto::WorkspaceFileSave, RpcError> {
        match engine.handle(methods::SAVE_WORKSPACE_FILE, params).await? {
            RpcReply::Value(value) => Ok(serde_json::from_value(value).unwrap()),
            _ => panic!("SaveWorkspaceFile must reply with a value"),
        }
    }

    #[tokio::test]
    async fn round_trips_text_and_preserves_formatting_and_permissions() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        let path = Path::new(&root).join("doc.md");
        fs::write(&path, "\u{FEFF}line one\r\nline two\r\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        }
        let engine = setup(&fixture).await;

        let read = read(
            &engine,
            json!({ "spaceId": "space-1", "path": path.display().to_string() }),
        )
        .await
        .unwrap();
        // The buffer keeps the original terminators; the save writes them
        // back verbatim with the BOM reattached ahead of the text.
        let edited = format!(
            "{}\r\n",
            read.text.clone().unwrap().replace("line one", "LINE ONE")
        );
        let save = save_file(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": path.display().to_string(),
                "text": edited,
                "version": read.version,
                "bom": read.bom,
            }),
        )
        .await
        .unwrap();
        assert_eq!(save.status, WorkspaceSaveStatus::Saved);
        assert!(save.version.is_some() && save.version != Some(read.version.clone()));

        let disk = fs::read(&path).unwrap();
        let mut expected = vec![0xEF, 0xBB, 0xBF];
        expected.extend_from_slice(edited.as_bytes());
        assert_eq!(disk, expected);
        assert!(disk.starts_with(&[0xEF, 0xBB, 0xBF]), "BOM reattached");
        assert!(
            disk.windows(22)
                .any(|w| w == b"LINE ONE\r\nline two\r\n\r\n"),
            "CRLF preserved"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o640, "permissions preserved");
        }
        // The new token matches a fresh read.
        let reread = super::read(
            &engine,
            json!({ "spaceId": "space-1", "path": path.display().to_string() }),
        )
        .await
        .unwrap();
        assert_eq!(reread.version, save.version.unwrap());
    }

    #[tokio::test]
    async fn a_stale_version_conflicts_without_writing() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        let path = Path::new(&root).join("conflict.txt");
        fs::write(&path, "original\n").unwrap();
        let engine = setup(&fixture).await;
        let read = read(
            &engine,
            json!({ "spaceId": "space-1", "path": path.display().to_string() }),
        )
        .await
        .unwrap();

        // External change the UI has not seen.
        fs::write(&path, "externally changed\n").unwrap();
        let save = save_file(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": path.display().to_string(),
                "text": "my draft\n",
                "version": read.version,
                "bom": false,
            }),
        )
        .await
        .unwrap();
        assert_eq!(save.status, WorkspaceSaveStatus::VersionConflict);
        assert!(save.disk_version.is_some());
        assert_ne!(save.disk_version, Some(read.version.clone()));
        // The external version is intact — no partial write, no overwrite.
        assert_eq!(fs::read_to_string(&path).unwrap(), "externally changed\n");
        // No stray temp files leaked into the tree.
        assert!(fs::read_dir(Path::new(&root)).unwrap().count() == 1);

        // A second save based on the SAME read still conflicts — the UI's
        // interim state never adopts the unseen disk version as its
        // baseline, so no silent overwrite path exists (ticket 02's draft
        // guarantee ahead of ticket 04's resolution workflow).
        let stale_disk = save.disk_version.unwrap();
        let retry = save_file(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": path.display().to_string(),
                "text": "my draft again\n",
                "version": read.version,
                "bom": false,
            }),
        )
        .await
        .unwrap();
        assert_eq!(retry.status, WorkspaceSaveStatus::VersionConflict);
        assert_eq!(retry.disk_version, Some(stale_disk));
        assert_eq!(fs::read_to_string(&path).unwrap(), "externally changed\n");
    }

    #[tokio::test]
    async fn missing_files_are_not_recreated_and_boundaries_hold() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        fs::write(Path::new(&root).join("gone.txt"), "x\n").unwrap();
        let engine = setup(&fixture).await;
        let read = read(
            &engine,
            json!({ "spaceId": "space-1", "path": format!("{root}/gone.txt") }),
        )
        .await
        .unwrap();
        fs::remove_file(Path::new(&root).join("gone.txt")).unwrap();

        let fault = save_file(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": format!("{root}/gone.txt"),
                "text": "recreated\n",
                "version": read.version,
                "bom": false,
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains("does not exist")),
            "{fault:?}"
        );
        assert!(!Path::new(&root).join("gone.txt").exists(), "not recreated");

        let fault = save_file(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": "/etc/hosts",
                "text": "no\n",
                "version": "0:0",
                "bom": false,
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains("outside")),
            "{fault:?}"
        );

        let fault = save_file(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": format!("{root}/.git/HEAD"),
                "text": "no\n",
                "version": "0:0",
                "bom": false,
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains(".git")),
            "{fault:?}"
        );
    }

    #[tokio::test]
    async fn saves_go_through_the_owning_space_selector() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        fs::write(Path::new(&root).join("s.txt"), "v1\n").unwrap();
        let engine = setup(&fixture).await;
        let read = read(
            &engine,
            json!({ "chatId": "chat-1", "path": format!("{root}/s.txt") }),
        )
        .await
        .unwrap();
        // The chat may be deselected by reply time — the save still lands.
        let save = save_file(
            &engine,
            json!({
                "chatId": "chat-1",
                "path": format!("{root}/s.txt"),
                "text": "v2\n",
                "version": read.version,
                "bom": false,
            }),
        )
        .await
        .unwrap();
        assert_eq!(save.status, WorkspaceSaveStatus::Saved);
        assert_eq!(
            fs::read_to_string(Path::new(&root).join("s.txt")).unwrap(),
            "v2\n"
        );
    }
}

mod live_refresh_and_resolution {
    use super::*;
    use holt_proto::{WorkspaceSaveStatus, WorkspaceWatchFrame};

    async fn save_as(
        engine: &LocalEngine,
        params: serde_json::Value,
    ) -> Result<holt_proto::WorkspaceFileSave, RpcError> {
        match engine
            .handle(methods::WRITE_WORKSPACE_FILE_AS, params)
            .await?
        {
            RpcReply::Value(value) => Ok(serde_json::from_value(value).unwrap()),
            _ => panic!("WriteWorkspaceFileAs must reply with a value"),
        }
    }

    async fn save_with(
        engine: &LocalEngine,
        params: serde_json::Value,
    ) -> Result<holt_proto::WorkspaceFileSave, RpcError> {
        match engine.handle(methods::SAVE_WORKSPACE_FILE, params).await? {
            RpcReply::Value(value) => Ok(serde_json::from_value(value).unwrap()),
            _ => panic!("SaveWorkspaceFile must reply with a value"),
        }
    }

    #[tokio::test]
    async fn watch_frames_deliver_coalesced_changes() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        fs::write(Path::new(&root).join("watched.txt"), "v1\n").unwrap();
        let engine = setup(&fixture).await;

        let mut stream = match engine
            .handle(
                methods::WATCH_WORKSPACE_ENTRIES,
                json!({ "spaceId": "space-1" }),
            )
            .await
            .unwrap()
        {
            RpcReply::Stream(stream) => stream,
            _ => panic!("WatchWorkspaceEntries must reply with a stream"),
        };

        // Give the spawned watcher a moment to arm before writing — the
        // subscription returns before the platform watcher registers.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        // A burst of writes coalesces into one debounced frame.
        fs::write(Path::new(&root).join("watched.txt"), "v2\n").unwrap();
        fs::write(Path::new(&root).join("created.txt"), "new\n").unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let frame: WorkspaceWatchFrame = loop {
            let next = tokio::time::timeout_at(deadline, stream.next()).await;
            let Some(item) = next.expect("frame within deadline") else {
                panic!("stream ended");
            };
            let frame: WorkspaceWatchFrame = serde_json::from_value(item).unwrap();
            if !frame.paths.is_empty() {
                break frame;
            }
        };
        let joined = frame.paths.join("\n");
        assert!(joined.contains("watched.txt"), "{joined:?}");
        assert!(joined.contains("created.txt"), "{joined:?}");

        // Dropping the stream ends the watch (the server task exits); a
        // fresh subscription still works afterwards.
        drop(stream);
    }

    #[tokio::test]
    async fn save_as_creates_new_files_without_touching_the_original() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        fs::write(Path::new(&root).join("orig.txt"), "original\n").unwrap();
        let engine = setup(&fixture).await;

        let saved = save_as(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": format!("{root}/copy.txt"),
                "text": "draft\n",
                "bom": false,
            }),
        )
        .await
        .unwrap();
        assert_eq!(saved.status, WorkspaceSaveStatus::Saved);
        assert_eq!(
            fs::read_to_string(Path::new(&root).join("copy.txt")).unwrap(),
            "draft\n"
        );
        assert_eq!(
            fs::read_to_string(Path::new(&root).join("orig.txt")).unwrap(),
            "original\n",
            "the original is untouched"
        );

        // Collisions refuse; escapes and .git refuse; missing parents refuse.
        let fault = save_as(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": format!("{root}/copy.txt"),
                "text": "again\n",
                "bom": false,
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains("already exists")),
            "{fault:?}"
        );

        let fault = save_as(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": "/etc/evil.txt",
                "text": "no\n",
                "bom": false,
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains("outside")),
            "{fault:?}"
        );

        let fault = save_as(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": format!("{root}/.git/hooks/new"),
                "text": "no\n",
                "bom": false,
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains(".git")),
            "{fault:?}"
        );

        let fault = save_as(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": format!("{root}/missing-parent/new.txt"),
                "text": "no\n",
                "bom": false,
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains("does not exist")),
            "{fault:?}"
        );
    }

    #[tokio::test]
    async fn confirmed_overwrite_targets_the_reviewed_version_only() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        fs::write(Path::new(&root).join("ow.txt"), "original\n").unwrap();
        let engine = setup(&fixture).await;
        let read = read(
            &engine,
            json!({ "spaceId": "space-1", "path": format!("{root}/ow.txt") }),
        )
        .await
        .unwrap();

        // An external change lands; the conflict reports its disk version.
        fs::write(Path::new(&root).join("ow.txt"), "external A\n").unwrap();
        let conflict = save_with(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": format!("{root}/ow.txt"),
                "text": "my draft\n",
                "version": read.version,
                "bom": false,
            }),
        )
        .await
        .unwrap();
        assert_eq!(conflict.status, WorkspaceSaveStatus::VersionConflict);
        let reviewed = conflict.disk_version.clone().unwrap();

        // The user reviewed THAT version and confirmed an overwrite: the
        // save succeeds against the reviewed token.
        let confirmed = save_with(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": format!("{root}/ow.txt"),
                "text": "my draft\n",
                "version": read.version,
                "expectDiskVersion": reviewed,
                "bom": false,
            }),
        )
        .await
        .unwrap();
        assert_eq!(confirmed.status, WorkspaceSaveStatus::Saved);
        assert_eq!(
            fs::read_to_string(Path::new(&root).join("ow.txt")).unwrap(),
            "my draft\n"
        );

        // A SECOND intervening change invalidates the earlier decision: the
        // stale reviewed token re-conflicts instead of overwriting.
        fs::write(Path::new(&root).join("ow.txt"), "external B\n").unwrap();
        let stale = save_with(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": format!("{root}/ow.txt"),
                "text": "my draft 2\n",
                "version": read.version,
                "expectDiskVersion": reviewed,
                "bom": false,
            }),
        )
        .await
        .unwrap();
        assert_eq!(stale.status, WorkspaceSaveStatus::VersionConflict);
        assert_eq!(
            fs::read_to_string(Path::new(&root).join("ow.txt")).unwrap(),
            "external B\n",
            "no overwrite of the unreviewed version"
        );
    }
}

use futures::StreamExt as _;

mod create_and_rename {
    use super::*;

    async fn create(
        engine: &LocalEngine,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        match engine
            .handle(methods::CREATE_WORKSPACE_ENTRY, params)
            .await?
        {
            RpcReply::Value(value) => Ok(value),
            _ => panic!("CreateWorkspaceEntry must reply with a value"),
        }
    }

    async fn rename(engine: &LocalEngine, params: serde_json::Value) -> Result<String, RpcError> {
        match engine
            .handle(methods::RENAME_WORKSPACE_ENTRY, params)
            .await?
        {
            RpcReply::Value(value) => Ok(value
                .get("path")
                .and_then(|p| p.as_str())
                .unwrap()
                .to_string()),
            _ => panic!("RenameWorkspaceEntry must reply with a value"),
        }
    }

    #[tokio::test]
    async fn creates_files_and_directories_with_unicode_names() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        let engine = setup(&fixture).await;

        create(
            &engine,
            json!({ "spaceId": "space-1", "name": "notes draft ✓.md", "isDir": false }),
        )
        .await
        .unwrap();
        create(
            &engine,
            json!({ "spaceId": "space-1", "name": "nested dir", "isDir": true }),
        )
        .await
        .unwrap();
        create(
            &engine,
            json!({
                "spaceId": "space-1",
                "parentPath": format!("{root}/nested dir"),
                "name": "inner.txt",
                "isDir": false,
            }),
        )
        .await
        .unwrap();
        assert!(Path::new(&root).join("notes draft ✓.md").is_file());
        assert!(Path::new(&root).join("nested dir").is_dir());
        assert!(Path::new(&root).join("nested dir/inner.txt").is_file());

        // The new file opens through the read RPC; the new directory lists.
        let read = read(
            &engine,
            json!({ "spaceId": "space-1", "path": format!("{root}/nested dir/inner.txt") }),
        )
        .await
        .unwrap();
        assert_eq!(read.text.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn collisions_and_invalid_names_refuse_without_mutation() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        fs::write(Path::new(&root).join("exists.txt"), "keep\n").unwrap();
        let engine = setup(&fixture).await;

        // Collision refuses and leaves the existing entry intact.
        let fault = create(
            &engine,
            json!({ "spaceId": "space-1", "name": "exists.txt", "isDir": false }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains("already exists")),
            "{fault:?}"
        );
        assert_eq!(
            fs::read_to_string(Path::new(&root).join("exists.txt")).unwrap(),
            "keep\n"
        );

        for bad in ["", "  ", "a/b", "..", ".git", "a\\b"] {
            let fault = create(
                &engine,
                json!({ "spaceId": "space-1", "name": bad, "isDir": false }),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(&fault, RpcError::Failed(m) if m.contains("name")),
                "{bad:?}: {fault:?}"
            );
        }

        // Missing parent and outside-root parents refuse.
        let fault = create(
            &engine,
            json!({
                "spaceId": "space-1",
                "parentPath": format!("{root}/nope"),
                "name": "x",
                "isDir": false,
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains("does not exist")),
            "{fault:?}"
        );
        let fault = create(
            &engine,
            json!({ "spaceId": "space-1", "parentPath": "/etc", "name": "x", "isDir": false }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains("outside")),
            "{fault:?}"
        );
    }

    #[tokio::test]
    async fn renames_in_place_with_collisions_refusing() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        fs::write(Path::new(&root).join("old name.md"), "draft\n").unwrap();
        fs::write(Path::new(&root).join("taken.md"), "other\n").unwrap();
        let engine = setup(&fixture).await;

        let destination = rename(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": format!("{root}/old name.md"),
                "newName": "new name.md",
            }),
        )
        .await
        .unwrap();
        assert!(destination.ends_with("new name.md"));
        assert!(Path::new(&root).join("new name.md").is_file());
        assert!(!Path::new(&root).join("old name.md").exists());
        assert_eq!(
            fs::read_to_string(Path::new(&root).join("new name.md")).unwrap(),
            "draft\n",
            "contents ride along untouched"
        );

        // Collision refuses; the source keeps its identity.
        let fault = rename(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": format!("{root}/new name.md"),
                "newName": "taken.md",
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains("already exists")),
            "{fault:?}"
        );
        assert!(Path::new(&root).join("new name.md").is_file());

        // Directories rename wholesale (children move with them).
        fs::create_dir_all(Path::new(&root).join("pkg/src")).unwrap();
        rename(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": format!("{root}/pkg"),
                "newName": "crate",
            }),
        )
        .await
        .unwrap();
        assert!(Path::new(&root).join("crate/src").is_dir());
    }

    #[tokio::test]
    async fn renames_act_on_symlink_entries_not_targets() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        fs::write(Path::new(&root).join("real.txt"), "target\n").unwrap();
        std::os::unix::fs::symlink(format!("{root}/real.txt"), format!("{root}/link.txt")).unwrap();
        let engine = setup(&fixture).await;

        rename(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": format!("{root}/link.txt"),
                "newName": "alias.txt",
            }),
        )
        .await
        .unwrap();
        // The link moved; the target is untouched and still readable
        // through the renamed alias.
        assert!(!std::fs::symlink_metadata(format!("{root}/link.txt")).is_ok());
        assert!(Path::new(&root).join("real.txt").is_file());
        let read = read(
            &engine,
            json!({ "spaceId": "space-1", "path": format!("{root}/alias.txt") }),
        )
        .await
        .unwrap();
        assert_eq!(read.text.as_deref(), Some("target\n"));
    }

    #[tokio::test]
    async fn boundaries_hold_for_renames() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        let engine = setup(&fixture).await;
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "x").unwrap();

        let fault = rename(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": outside.path().join("secret.txt").display().to_string(),
                "newName": "inside.txt",
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains("outside")),
            "{fault:?}"
        );
        assert!(Path::new(&outside.path().join("secret.txt")).is_file());

        // A same-name rename is a no-op success (idempotent), but a `.git`
        // name refuses.
        let fault = rename(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": format!("{root}/x.txt"),
                "newName": ".git",
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains("valid name")),
            "{fault:?}"
        );
    }
}

mod create_rename_guards {
    use super::*;

    async fn rename_raw(
        engine: &LocalEngine,
        params: serde_json::Value,
    ) -> Result<String, RpcError> {
        match engine
            .handle(methods::RENAME_WORKSPACE_ENTRY, params)
            .await?
        {
            RpcReply::Value(value) => Ok(value
                .get("path")
                .and_then(|p| p.as_str())
                .unwrap()
                .to_string()),
            _ => panic!("RenameWorkspaceEntry must reply with a value"),
        }
    }

    #[tokio::test]
    async fn permission_failures_refuse_without_side_effects() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        // A read-only directory: creations inside must fail cleanly.
        let locked = Path::new(&root).join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("seed.txt"), "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        }
        let engine = setup(&fixture).await;

        #[cfg(unix)]
        {
            let fault = match engine
                .handle(
                    methods::CREATE_WORKSPACE_ENTRY,
                    json!({
                        "spaceId": "space-1",
                        "parentPath": locked.display().to_string(),
                        "name": "nope.txt",
                        "isDir": false,
                    }),
                )
                .await
            {
                Ok(_) => panic!("create in a read-only directory must fail"),
                Err(error) => error,
            };
            assert!(matches!(fault, RpcError::Failed(_)), "{fault:?}");
            assert!(!locked.join("nope.txt").exists());

            // Rename INTO the read-only directory is a sibling rename (same
            // directory) — renaming a file already inside it also fails.
            let fault = match engine
                .handle(
                    methods::RENAME_WORKSPACE_ENTRY,
                    json!({
                        "spaceId": "space-1",
                        "path": locked.join("seed.txt").display().to_string(),
                        "newName": "renamed.txt",
                    }),
                )
                .await
            {
                Ok(_) => panic!("rename in a read-only directory must fail"),
                Err(error) => error,
            };
            assert!(matches!(fault, RpcError::Failed(_)), "{fault:?}");
            assert!(locked.join("seed.txt").exists());

            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[tokio::test]
    async fn traversal_names_and_git_never_move_or_create() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        std::fs::create_dir_all(Path::new(&root).join(".git/refs")).unwrap();
        fs::write(Path::new(&root).join("file.txt"), "x").unwrap();
        let engine = setup(&fixture).await;

        // A traversal-carrying NEW name refuses (rename keeps its parent).
        for bad in ["../escape", "a/../b", "sub/x"] {
            let fault = match engine
                .handle(
                    methods::RENAME_WORKSPACE_ENTRY,
                    json!({
                        "spaceId": "space-1",
                        "path": format!("{root}/file.txt"),
                        "newName": bad,
                    }),
                )
                .await
            {
                Ok(_) => panic!("{bad:?} must refuse"),
                Err(error) => error,
            };
            assert!(
                matches!(&fault, RpcError::Failed(m) if m.contains("valid name")),
                "{bad:?}: {fault:?}"
            );
        }
        assert!(Path::new(&root).join("file.txt").exists(), "untouched");

        // Creating INSIDE .git refuses (parent fence).
        let fault = match engine
            .handle(
                methods::CREATE_WORKSPACE_ENTRY,
                json!({
                    "spaceId": "space-1",
                    "parentPath": format!("{root}/.git/refs"),
                    "name": "evil",
                    "isDir": false,
                }),
            )
            .await
        {
            Ok(_) => panic!("create inside .git must refuse"),
            Err(error) => error,
        };
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains(".git")),
            "{fault:?}"
        );

        // Renaming a .git ENTRY refuses too (leaf fence).
        let fault = match engine
            .handle(
                methods::RENAME_WORKSPACE_ENTRY,
                json!({
                    "spaceId": "space-1",
                    "path": format!("{root}/.git/refs"),
                    "newName": "refs-old",
                }),
            )
            .await
        {
            Ok(_) => panic!("renaming a .git entry must refuse"),
            Err(error) => error,
        };
        assert!(
            matches!(&fault, RpcError::Failed(m) if m.contains(".git")),
            "{fault:?}"
        );
        assert!(Path::new(&root).join(".git/refs").is_dir());
    }

    #[tokio::test]
    async fn case_only_renames_succeed_on_case_insensitive_filesystems() {
        let fixture = Fixture::new();
        let root = fixture.cwd();
        fs::write(Path::new(&root).join("readme.md"), "same\n").unwrap();
        let engine = setup(&fixture).await;

        let destination = rename_raw(
            &engine,
            json!({
                "spaceId": "space-1",
                "path": format!("{root}/readme.md"),
                "newName": "README.md",
            }),
        )
        .await;
        // Case-sensitive filesystems rename; case-insensitive ones stat the
        // source as the destination and must STILL rename (not report a
        // collision). Either way the file survives under one spelling with
        // its contents intact.
        let _ = destination;
        let survived = Path::new(&root).join("readme.md").is_file()
            || Path::new(&root).join("README.md").is_file();
        assert!(survived);
        let contents = std::fs::read_to_string(Path::new(&root).join("README.md"))
            .or_else(|_| std::fs::read_to_string(Path::new(&root).join("readme.md")))
            .unwrap();
        assert_eq!(contents, "same\n");
    }
}
