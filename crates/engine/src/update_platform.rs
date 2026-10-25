//! Platform arms of app self-update: what counts as a managed install on
//! this OS, which release asset it updates from, and how the verified
//! download replaces the running one. The macOS arm keeps the release
//! team's codesign gate untouched; the archive platforms (a Windows exe,
//! a Linux AppImage) stage the file beside the running one and swap it in
//! place via [`swap_with_backup`] — restarting onto it is the UI's job.
//! Layout decisions are pure functions so every platform's rule
//! unit-tests everywhere (docs/research/windows-compatibility.md §10's
//! cfg-balanced rule); only the thin `managed_install`/`install`
//! dispatch touches the OS.

use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
use std::process::Command;

/// What `apply` may replace, resolved once from the running process.
pub(crate) enum InstallTarget {
    /// `…/Holt.app` — replaced wholesale from a verified DMG.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    MacBundle(PathBuf),
    /// The running `holt.exe` in a release-managed directory.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    WindowsExe { exe: PathBuf },
    /// The running AppImage (`$APPIMAGE`, injected by its runtime).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    AppImage { file: PathBuf },
}

/// The file whose presence beside the exe marks a Windows directory the
/// updater may write into; the release zip and the setup install both
/// carry it, a `cargo run` never does.
#[cfg(target_os = "windows")]
const RELEASE_MARKER: &str = "holt-release.marker";

/// Resolve what this process would update, or `None` when it is not a
/// managed install (a dev build, a hand-copied binary, a non-AppImage
/// Linux build).
pub(crate) fn managed_install() -> Option<InstallTarget> {
    #[cfg(target_os = "macos")]
    {
        let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
        macos_bundle_layout(&exe).map(InstallTarget::MacBundle)
    }
    #[cfg(target_os = "windows")]
    {
        let exe = std::env::current_exe().ok()?;
        let marker = exe.parent()?.join(RELEASE_MARKER);
        windows_managed_layout(&exe, marker.is_file()).map(|exe| InstallTarget::WindowsExe { exe })
    }
    #[cfg(target_os = "linux")]
    {
        let appimage = PathBuf::from(std::env::var_os("APPIMAGE")?);
        if !appimage.is_file() {
            return None;
        }
        linux_appimage_path(&appimage).map(|file| InstallTarget::AppImage { file })
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        None
    }
}

// The layout helpers below (and the file swap further down) are each
// platform's rule in pure form: compiled on every host so their unit
// tests run everywhere (windows-compatibility.md §10's cfg-balanced
// rule), but production code only on their own OS — hence the per-target
// dead-code allowances.

/// `…/Holt.app` when `exe` sits at `Holt.app/Contents/MacOS/…` — the only
/// macOS layout the updater replaces.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn macos_bundle_layout(exe: &Path) -> Option<PathBuf> {
    let bundle = exe.parent()?.parent()?.parent()?;
    (bundle.extension()? == "app").then(|| bundle.to_path_buf())
}

/// Managed on Windows when the release marker sits beside the exe; the
/// marker is the whole test, so a relocated exe still updates as long as
/// its directory is release-owned.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn windows_managed_layout(exe: &Path, marker_exists: bool) -> Option<PathBuf> {
    marker_exists.then(|| exe.to_path_buf())
}

/// Updatable on Linux when the runtime-injected `$APPIMAGE` names a file
/// (rooted, with a file name — never a bare `/` or a relative fragment);
/// the caller checks existence, which is a syscall. `has_root`, not
/// `is_absolute`: on the Unix host this arm runs on they are identical,
/// and a rooted check keeps this contract observable on every host the
/// cfg-balanced tests run on (`is_absolute` rejects `/home/...` on
/// Windows, where this arm never runs but its tests do).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn linux_appimage_path(appimage: &Path) -> Option<PathBuf> {
    (appimage.has_root() && appimage.file_name().is_some()).then(|| appimage.to_path_buf())
}

/// The release asset this platform updates from — the naming the site's
/// download page already serves, reused as-is (zero site changes).
pub(crate) fn asset_suffix() -> &'static str {
    if cfg!(target_os = "macos") {
        ".dmg"
    } else if cfg!(target_os = "windows") {
        "-win.zip"
    } else {
        ".AppImage"
    }
}

/// Only macOS installs without a digest: its DMG path carries the
/// codesign requirement. The archive platforms fail closed without one.
pub(crate) fn digest_required() -> bool {
    !cfg!(target_os = "macos")
}

/// Replace the managed install from the verified download. Blocking —
/// call on `spawn_blocking`. Each arm cleans `work` up on both paths and
/// drops the file it staged beside the install when the swap fails.
pub(crate) fn install(downloaded: &Path, work: &Path, target: InstallTarget) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        match target {
            InstallTarget::MacBundle(bundle) => swap_from_dmg(downloaded, work, &bundle),
            _ => Err("self-update target is not an app bundle".into()),
        }
    }
    #[cfg(target_os = "windows")]
    {
        match target {
            InstallTarget::WindowsExe { exe } => install_windows_exe(downloaded, work, &exe),
            _ => Err("self-update target is not a managed Windows install".into()),
        }
    }
    #[cfg(target_os = "linux")]
    {
        match target {
            InstallTarget::AppImage { file } => install_appimage(downloaded, work, &file),
            _ => Err("self-update target is not an AppImage".into()),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = (downloaded, work, target);
        Err("self-update is not supported on this platform".into())
    }
}

/// Unpack the verified zip and swap the exe: stage its `holt.exe` as
/// `holt.exe.incoming` next to the running one, then
/// [`swap_with_backup`] — Windows allows renaming (not deleting) a
/// running exe, which is what makes the two-step swap work.
#[cfg(target_os = "windows")]
fn install_windows_exe(downloaded: &Path, work: &Path, exe: &Path) -> Result<(), String> {
    let unpacked = work.join("unpacked");
    let staged = exe.with_file_name("holt.exe.incoming");
    let installed = (|| {
        std::fs::create_dir(&unpacked).map_err(|e| e.to_string())?;
        let file = std::fs::File::open(downloaded).map_err(|e| e.to_string())?;
        let mut archive =
            zip::ZipArchive::new(file).map_err(|e| format!("release archive unreadable: {e}"))?;
        archive
            .extract(&unpacked)
            .map_err(|e| format!("release archive unpack failed: {e}"))?;
        let new_exe = unpacked.join("holt.exe");
        if !new_exe.is_file() {
            return Err("release archive does not contain holt.exe".into());
        }
        std::fs::copy(&new_exe, &staged)
            .map_err(|e| format!("cannot stage {}: {e}", staged.display()))?;
        swap_with_backup(exe, &staged, &exe.with_file_name("holt.exe.old"))
    })();
    // The staged file sits in the install directory, outside `work` — a
    // failure must take it back out.
    if installed.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    let _ = std::fs::remove_dir_all(work);
    installed
}

/// Stage the verified download beside the AppImage and swap it in. The
/// copy (not a rename) crosses filesystems safely — `$TMPDIR` is often
/// tmpfs — and the new file inherits the original's permissions so the
/// executable bit survives.
#[cfg(target_os = "linux")]
fn install_appimage(downloaded: &Path, work: &Path, appimage: &Path) -> Result<(), String> {
    let staged = appimage.with_file_name("holt.appimage.incoming");
    let installed = (|| {
        std::fs::copy(downloaded, &staged)
            .map_err(|e| format!("cannot stage {}: {e}", staged.display()))?;
        let permissions = std::fs::metadata(appimage)
            .map_err(|e| e.to_string())?
            .permissions();
        std::fs::set_permissions(&staged, permissions).map_err(|e| e.to_string())?;
        swap_with_backup(
            appimage,
            &staged,
            &appimage.with_file_name("holt.appimage.old"),
        )
    })();
    // The staged file sits beside the AppImage, outside `work` — a
    // failure must take it back out.
    if installed.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    let _ = std::fs::remove_dir_all(work);
    installed
}

/// Replace `target` with `staged` via `backup`: delete a stale `backup`,
/// rename the old file aside, move the staged one in. If the second
/// rename fails, [`restore`] puts the old file back. Pure `std::fs`, so
/// it builds and tests on every platform.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
pub(crate) fn swap_with_backup(target: &Path, staged: &Path, backup: &Path) -> Result<(), String> {
    // A stale backup from an earlier update would block the first rename
    // on Windows; a failure to remove it surfaces as that rename's error.
    let _ = std::fs::remove_file(backup);
    std::fs::rename(target, backup)
        .map_err(|e| format!("cannot replace {}: {e}", target.display()))?;
    if let Err(e) = std::fs::rename(staged, target) {
        // The backup is the only launchable copy left at this point: put
        // it back, and if even that fails say where it is — otherwise the
        // error names a file that no longer exists.
        return Err(match restore(backup, target) {
            Ok(()) => format!("cannot replace {}: {e}", target.display()),
            Err(failed) => format!(
                "cannot replace {}: {e}; the previous file is kept as {} ({failed})",
                target.display(),
                backup.display()
            ),
        });
    }
    // Windows cannot delete the exe that is still running — the backup
    // may survive here until the next start's `cleanup_stale`.
    let _ = std::fs::remove_file(backup);
    Ok(())
}

/// Best-effort rollback of a half-done swap: put the backup back.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
fn restore(backup: &Path, target: &Path) -> Result<(), String> {
    std::fs::rename(backup, target).map_err(|e| e.to_string())
}

/// Remove the previous update's leftovers: the old exe/AppImage that
/// could not be deleted while it was running, and the staging file a
/// crashed apply left beside the install (a failed one removes it
/// itself). Best-effort.
pub(crate) fn cleanup_stale() {
    match managed_install() {
        Some(InstallTarget::WindowsExe { exe }) => {
            let _ = std::fs::remove_file(exe.with_file_name("holt.exe.old"));
            let _ = std::fs::remove_file(exe.with_file_name("holt.exe.incoming"));
        }
        Some(InstallTarget::AppImage { file }) => {
            let _ = std::fs::remove_file(file.with_file_name("holt.appimage.old"));
            let _ = std::fs::remove_file(file.with_file_name("holt.appimage.incoming"));
        }
        _ => {}
    }
}

// --- macOS: the DMG swap chain, unchanged since it shipped — the
// codesign requirement is the release trust chain and must not be
// weakened. ---

/// Designated requirement every release bundle must satisfy (the
/// Developer ID team `scripts/build-dmg.sh` signs with).
#[cfg(target_os = "macos")]
const CODE_REQUIREMENT: &str =
    "anchor apple generic and certificate leaf[subject.OU] = \"AQMMZQ3MDX\"";

#[cfg(target_os = "macos")]
fn swap_from_dmg(dmg: &Path, work: &Path, bundle: &Path) -> Result<(), String> {
    let mount = work.join("mnt");
    run(Command::new("hdiutil")
        .args([
            "attach",
            "-nobrowse",
            "-readonly",
            "-noautoopen",
            "-mountpoint",
        ])
        .arg(&mount)
        .arg(dmg))?;
    let staged = bundle.with_file_name(".Holt.app.update");
    let copied = (|| {
        let source = mount.join("Holt.app");
        run(Command::new("codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(format!("-R={CODE_REQUIREMENT}"))
            .arg(&source))?;
        let _ = std::fs::remove_dir_all(&staged);
        run(Command::new("ditto").arg(&source).arg(&staged))
    })();
    let _ = run(Command::new("hdiutil")
        .args(["detach", "-force"])
        .arg(&mount));
    let _ = std::fs::remove_dir_all(work);
    copied?;

    let old = bundle.with_file_name(".Holt.app.old");
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(bundle, &old)
        .map_err(|e| format!("cannot replace {}: {e}", bundle.display()))?;
    if let Err(e) = std::fs::rename(&staged, bundle) {
        let _ = std::fs::rename(&old, bundle);
        return Err(format!("cannot replace {}: {e}", bundle.display()));
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

#[cfg(target_os = "macos")]
fn run(command: &mut Command) -> Result<(), String> {
    let output = command.output().map_err(|e| e.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let program = command.get_program().to_string_lossy().into_owned();
    Err(format!(
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_layout_accepts_only_app_bundles() {
        let bundle = macos_bundle_layout(Path::new("/Applications/Holt.app/Contents/MacOS/holt"));
        assert_eq!(bundle.as_deref(), Some(Path::new("/Applications/Holt.app")));
        // A bare binary and a non-.app package are not managed.
        assert_eq!(macos_bundle_layout(Path::new("/opt/holt/bin/holt")), None);
        assert_eq!(
            macos_bundle_layout(Path::new("/Applications/Holt.dmg/Contents/MacOS/holt")),
            None
        );
        // Too shallow to be Contents/MacOS/….
        assert_eq!(macos_bundle_layout(Path::new("/Holt.app/holt")), None);
    }

    #[test]
    fn windows_layout_requires_the_release_marker() {
        let exe = Path::new(r"C:\Program Files\Holt\holt.exe");
        assert_eq!(windows_managed_layout(exe, true).as_deref(), Some(exe));
        assert_eq!(windows_managed_layout(exe, false), None);
    }

    #[test]
    fn linux_layout_requires_an_absolute_file_path() {
        let appimage = Path::new("/home/u/Applications/Holt-0.7.0-x64.AppImage");
        assert_eq!(linux_appimage_path(appimage).as_deref(), Some(appimage));
        // Relative fragment, bare root, empty value: none name a file.
        assert_eq!(linux_appimage_path(Path::new("Holt.AppImage")), None);
        assert_eq!(linux_appimage_path(Path::new("/")), None);
        assert_eq!(linux_appimage_path(Path::new("")), None);
    }

    #[test]
    fn swap_replaces_the_target_and_drops_the_backup() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("holt.bin");
        let staged = dir.path().join("holt.bin.incoming");
        let backup = dir.path().join("holt.bin.old");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&staged, b"new").unwrap();
        swap_with_backup(&target, &staged, &backup).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!staged.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn swap_replaces_a_stale_backup() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("holt.bin");
        let staged = dir.path().join("holt.bin.incoming");
        let backup = dir.path().join("holt.bin.old");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&staged, b"new").unwrap();
        std::fs::write(&backup, b"ancient").unwrap();
        swap_with_backup(&target, &staged, &backup).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!backup.exists());
    }

    /// Rollback: when the staged file cannot be moved onto the target,
    /// the original comes back. The Windows-specific semantics — renaming
    /// a *running* exe, deleting it failing until the next start — cannot
    /// be reproduced by a unit test and are left to real-machine
    /// verification.
    #[test]
    fn a_failed_swap_restores_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("holt.bin");
        let staged = dir.path().join("missing.incoming");
        let backup = dir.path().join("holt.bin.old");
        std::fs::write(&target, b"old").unwrap();
        let error = swap_with_backup(&target, &staged, &backup).unwrap_err();
        assert!(error.contains("cannot replace"), "{error}");
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(!backup.exists());
    }
}
