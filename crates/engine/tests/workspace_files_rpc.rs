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
