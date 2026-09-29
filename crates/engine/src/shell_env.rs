//! The user's login shell.
//!
//! A GUI-launched process inherits launchd's environment, not the one a login
//! shell assembles: `~/.zshenv`, `~/.zprofile`, `~/.zlogin` (and their `/etc`
//! counterparts) are read by *shells*, never by the OS. So a Dock-launched
//! Holt sees a minimal PATH — Apple's own directories only — and cannot know
//! the toolset the user sees in a terminal. Running the agent's commands
//! through the user's own login shell (see `tools::LocalExecutionEnv::exec`)
//! lets that shell assemble its environment the way the user configured it.
//! Direct spawns that cannot go through a shell — MCP stdio children
//! (ADR-0034) — ask the same shell for its PATH once ([`login_path`]) and
//! resolve bare command names against it.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    time::Duration,
};

/// The user's login shell, from the password database.
pub(crate) fn login_shell() -> String {
    resolve_login_shell(
        passwd_shell().as_deref().map(OsStr::new),
        std::env::var_os("SHELL").as_deref(),
    )
}

/// The shell recorded for this user in the password database, if any.
fn passwd_shell() -> Option<String> {
    #[cfg(unix)]
    {
        // Reentrant lookup: GUI launches need not inherit SHELL from a login shell.
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut storage = vec![0u8; 16384];
        let mut result = std::ptr::null_mut();
        unsafe {
            if libc::getpwuid_r(
                libc::getuid(),
                entry.as_mut_ptr(),
                storage.as_mut_ptr().cast(),
                storage.len(),
                &mut result,
            ) == 0
                && !result.is_null()
                && !(*result).pw_shell.is_null()
            {
                let shell = std::ffi::CStr::from_ptr((*result).pw_shell).to_string_lossy();
                if !shell.is_empty() {
                    return Some(shell.into_owned());
                }
            }
        }
    }
    None
}

/// The executable agent commands run under: the password database's shell for
/// this user when it names an executable file, else `$SHELL` under the same
/// bar, else `/bin/sh` — so a broken or missing shell record degrades to the
/// one shell every Unix guarantees, never to a spawn failure per command.
fn resolve_login_shell(passwd: Option<&OsStr>, env: Option<&OsStr>) -> String {
    passwd
        .into_iter()
        .chain(env)
        .find(|shell| is_executable_file(shell))
        .map(|shell| shell.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/bin/sh".into())
}

/// An absolute path to an executable regular file. Metadata only: a candidate
/// shell is never spawned to prove itself.
#[cfg(unix)]
fn is_executable_file(path: &OsStr) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let path = Path::new(path);
    path.is_absolute()
        && std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &OsStr) -> bool {
    let path = Path::new(path);
    path.is_absolute() && std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// How long a login shell gets to assemble its PATH before the probe gives
/// up and Holt falls back to its inherited environment.
#[cfg(unix)]
const PATH_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The PATH the user's login shell assembles, probed at most once per
/// process. `None` means the probe failed and the caller keeps its own
/// inherited environment — a degraded PATH, never a broken spawn.
#[cfg(unix)]
pub(crate) async fn login_path() -> Option<String> {
    static CACHED: tokio::sync::OnceCell<Option<String>> = tokio::sync::OnceCell::const_new();
    CACHED
        .get_or_init(|| async {
            let shell = login_shell();
            probe_path(&shell, PATH_PROBE_TIMEOUT).await
        })
        .await
        .clone()
}

/// One shell's assembled PATH: run it as a login shell over `/usr/bin/env`
/// and take the last `PATH=` line. The shell builds its environment before
/// running the command, so its answer lands after any profile chatter that
/// reaches stdout; `/usr/bin/env` prints the same across POSIX and fish.
#[cfg(unix)]
async fn probe_path(shell: &str, timeout: Duration) -> Option<String> {
    use std::process::Stdio;

    let output = tokio::process::Command::new(shell)
        .arg("-l")
        .arg("-c")
        .arg("/usr/bin/env")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(timeout, output).await {
        Ok(Ok(output)) if output.status.success() => output,
        Ok(Err(error)) => {
            tracing::warn!(target: "holt::shell_env", shell = %shell, %error, "login-shell PATH probe failed to run");
            return None;
        }
        _ => {
            tracing::warn!(target: "holt::shell_env", shell = %shell, "login-shell PATH probe timed out or failed; keeping the inherited environment");
            return None;
        }
    };
    let path = String::from_utf8_lossy(&output.stdout)
        .lines()
        .rfind(|line| line.starts_with("PATH="))
        .map(|line| line["PATH=".len()..].to_owned())
        .filter(|path| !path.is_empty());
    if path.is_none() {
        tracing::warn!(target: "holt::shell_env", shell = %shell, "login shell reported no PATH; keeping the inherited environment");
    }
    path
}

/// A bare command name resolved against a PATH value: the first directory
/// holding an executable regular file. A name carrying a path separator —
/// or one that resolves nowhere — is `None`, meaning "spawn it as written".
#[cfg(unix)]
pub(crate) fn resolve_command(name: &str, path: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    for dir in path.split(':').filter(|dir| !dir.is_empty()) {
        // `is_executable_file` demands an absolute candidate, which also
        // rules out cwd-relative PATH entries: resolution must never depend
        // on the directory Holt happens to run from.
        let candidate = Path::new(dir).join(name);
        if is_executable_file(candidate.as_os_str()) {
            return Some(candidate);
        }
    }
    None
}

/// GUI processes get the user PATH from the registry on Windows; no probe.
#[cfg(not(unix))]
pub(crate) async fn login_path() -> Option<String> {
    None
}

#[cfg(not(unix))]
pub(crate) fn resolve_command(_name: &str, _path: &str) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in passwd/`$SHELL` entry: a file on disk with the wanted mode,
    /// named for the shells users actually configure, so resolver tests never
    /// touch the machine's real shell setup.
    #[cfg(unix)]
    fn fake_shell(dir: &Path, name: &str, mode: u32) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn resolver_takes_a_valid_passwd_shell() {
        let dir = tempfile::tempdir().unwrap();
        let zsh = fake_shell(dir.path(), "zsh", 0o755);
        assert_eq!(
            resolve_login_shell(Some(OsStr::new(&zsh)), None),
            zsh,
            "a valid passwd shell must be used as-is"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolver_falls_back_to_the_shell_env() {
        let dir = tempfile::tempdir().unwrap();
        let bash = fake_shell(dir.path(), "bash", 0o755);
        let env = OsStr::new(&bash);
        assert_eq!(resolve_login_shell(None, Some(env)), bash);
        // An empty passwd record is no record at all.
        assert_eq!(resolve_login_shell(Some(OsStr::new("")), Some(env)), bash);
    }

    #[cfg(unix)]
    #[test]
    fn resolver_takes_passwd_over_the_env() {
        let dir = tempfile::tempdir().unwrap();
        let zsh = fake_shell(dir.path(), "zsh", 0o755);
        let bash = fake_shell(dir.path(), "bash", 0o755);
        assert_eq!(
            resolve_login_shell(Some(OsStr::new(&zsh)), Some(OsStr::new(&bash))),
            zsh,
            "the password database outranks $SHELL"
        );
    }

    #[test]
    fn resolver_falls_back_without_any_value() {
        assert_eq!(resolve_login_shell(None, None), "/bin/sh");
        assert_eq!(resolve_login_shell(Some(OsStr::new("")), None), "/bin/sh");
    }

    #[cfg(unix)]
    #[test]
    fn resolver_falls_back_when_the_path_is_invalid() {
        // A shell record pointing nowhere — a removed homebrew install, say —
        // must not break command spawning; it falls through deterministically.
        assert_eq!(
            resolve_login_shell(
                Some(OsStr::new("/nonexistent/holt-zsh")),
                Some(OsStr::new("/nonexistent/holt-bash")),
            ),
            "/bin/sh"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolver_falls_back_when_the_file_is_not_executable() {
        let dir = tempfile::tempdir().unwrap();
        let plain = fake_shell(dir.path(), "zsh", 0o644);
        let bash = fake_shell(dir.path(), "bash", 0o755);
        assert_eq!(
            resolve_login_shell(Some(OsStr::new(&plain)), Some(OsStr::new(&bash))),
            bash,
            "a file without execute permission is not a shell"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolver_rejects_a_relative_path() {
        // Exists on disk, but only relative to the temp dir — a relative
        // $SHELL would resolve against whatever cwd we happen to spawn from.
        let dir = tempfile::tempdir().unwrap();
        fake_shell(dir.path(), "zsh", 0o755);
        assert_eq!(
            resolve_login_shell(None, Some(OsStr::new("zsh"))),
            "/bin/sh",
            "a relative $SHELL is not a usable login shell"
        );
    }

    #[test]
    fn login_shell_is_an_absolute_path() {
        // getpwuid_r is the source; the env fallback only fires without one.
        assert!(login_shell().starts_with('/'));
    }

    /// A stand-in shell whose whole behavior is a canned script body, so
    /// probe tests never touch the machine's real login shell.
    #[cfg(unix)]
    fn fake_shell_with(dir: &Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn resolution_takes_the_first_executable_match() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let tool = fake_shell(first.path(), "tool", 0o755);
        fake_shell(second.path(), "tool", 0o755);
        let path = format!(
            "/nonexistent:{}:{}",
            first.path().display(),
            second.path().display()
        );
        assert_eq!(
            resolve_command("tool", &path),
            Some(PathBuf::from(&tool)),
            "the first directory holding the command wins"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolution_skips_non_executable_and_empty_entries() {
        let plain = tempfile::tempdir().unwrap();
        let real = tempfile::tempdir().unwrap();
        fake_shell(plain.path(), "tool", 0o644);
        let tool = fake_shell(real.path(), "tool", 0o755);
        // Empty entries are cwd-relative lookups and must not resolve.
        let path = format!("::{}:{}", plain.path().display(), real.path().display());
        assert_eq!(resolve_command("tool", &path), Some(PathBuf::from(&tool)));
    }

    #[cfg(unix)]
    #[test]
    fn resolution_leaves_pathed_and_unknown_names_alone() {
        let dir = tempfile::tempdir().unwrap();
        fake_shell(dir.path(), "tool", 0o755);
        let path = dir.path().display().to_string();
        assert_eq!(resolve_command("", &path), None);
        assert_eq!(resolve_command("dir/tool", &path), None);
        assert_eq!(resolve_command("nowhere", &path), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_takes_the_last_path_line_past_profile_chatter() {
        let dir = tempfile::tempdir().unwrap();
        let shell = fake_shell_with(
            dir.path(),
            "sh",
            "printf 'chatter\\nPATH=/stale\\nPATH=/fresh/bin\\n'",
        );
        assert_eq!(
            probe_path(&shell, Duration::from_secs(5)).await.as_deref(),
            Some("/fresh/bin"),
            "the shell's own answer, printed last, wins over profile output"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_falls_through_when_the_shell_exits_nonzero() {
        let dir = tempfile::tempdir().unwrap();
        let shell = fake_shell_with(dir.path(), "sh", "echo PATH=/ignored; exit 3");
        assert_eq!(probe_path(&shell, Duration::from_secs(5)).await, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_falls_through_when_the_shell_never_answers() {
        let dir = tempfile::tempdir().unwrap();
        let shell = fake_shell_with(dir.path(), "sh", "sleep 30");
        assert_eq!(
            probe_path(&shell, Duration::from_millis(50)).await,
            None,
            "the timeout bounds the probe and kills the shell"
        );
    }
}
