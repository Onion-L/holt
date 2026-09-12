//! The user's login shell.
//!
//! A GUI-launched process inherits launchd's environment, not the one a login
//! shell assembles: `~/.zshenv`, `~/.zprofile`, `~/.zlogin` (and their `/etc`
//! counterparts) are read by *shells*, never by the OS. So a Dock-launched
//! Holt sees a minimal PATH — Apple's own directories only — and cannot know
//! the toolset the user sees in a terminal. Running the agent's commands
//! through the user's own login shell (see `tools::LocalExecutionEnv::exec`)
//! lets that shell assemble its environment the way the user configured it.

use std::{ffi::OsStr, path::Path};

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
}
