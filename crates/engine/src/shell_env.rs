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
//! (ADR-mcp-servers-join-as-engine-clients-behind-the-gate) — ask the same shell for its PATH once ([`login_path`]) and
//! resolve bare command names against it.

#[cfg(unix)]
use std::time::Duration;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

/// Spawn console children without allocating a console window of their own
/// (their stdio is piped; a GUI-subsystem parent would otherwise flash a
/// window per spawn).
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The user's login shell: the password database's record on unix, cmd.exe
/// via COMSPEC on Windows (which names the command processor by contract).
pub(crate) fn login_shell() -> String {
    #[cfg(unix)]
    {
        resolve_login_shell(
            passwd_shell().as_deref().map(OsStr::new),
            std::env::var_os("SHELL").as_deref(),
        )
    }
    // A SHELL inherited from a git-bash launch names an msys path
    // (/usr/bin/bash) win32 cannot spawn; the executability bar in
    // `resolve_login_shell` filters it, leaving COMSPEC.
    #[cfg(windows)]
    {
        resolve_login_shell(
            std::env::var_os("COMSPEC").as_deref().map(OsStr::new),
            std::env::var_os("SHELL").as_deref(),
        )
    }
}

/// The shell agent commands run under: Git Bash ahead of the login-shell
/// floor on Windows. The toolset and its prompts speak POSIX sh, and the
/// floor there is cmd.exe, which answers sh syntax with spawn errors —
/// so a stock Git-for-Windows machine (only `Git\cmd` on PATH) would run
/// every bash-tool call through cmd otherwise. WSL's
/// `System32\bash.exe` is skipped: it boots a Linux world win32 argv and
/// cwd do not translate into. Everything else matches [`login_shell`],
/// which stays the terminal's shell. The candidate order mirrors core's
/// harness discovery (`agent/harness/env/nodejs.rs`); keep the two in
/// step.
pub(crate) fn agent_shell() -> String {
    find_git_bash().unwrap_or_else(login_shell)
}

#[cfg(windows)]
fn find_git_bash() -> Option<String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    fn where_hits(program: &str) -> Vec<String> {
        Command::new("where")
            .arg(program)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    if let Some(bash) = where_hits("bash.exe")
        .into_iter()
        .find(|hit| !is_wsl_bash(hit) && Path::new(hit).is_file())
    {
        return Some(bash);
    }
    if let Some(git_exe) = where_hits("git.exe").first()
        && let Some(bash) = bash_beside_git(git_exe)
        && Path::new(&bash).is_file()
    {
        return Some(bash);
    }
    standard_git_bash_locations(
        std::env::var_os("ProgramFiles").as_deref().map(Path::new),
        std::env::var_os("ProgramFiles(x86)")
            .as_deref()
            .map(Path::new),
        std::env::var_os("LocalAppData").as_deref().map(Path::new),
    )
    .into_iter()
    .find(|bash| Path::new(bash).is_file())
}

/// Unix agent commands already run through the user's login shell.
#[cfg(not(windows))]
fn find_git_bash() -> Option<String> {
    None
}

/// `…\Git\cmd\git.exe` (or `…\Git\bin\git.exe`) → `…\Git\bin\bash.exe`,
/// derived without touching the filesystem; the caller checks existence.
/// Mirrors the core harness helper of the same name.
#[cfg(windows)]
fn bash_beside_git(git_exe: &str) -> Option<String> {
    let root = Path::new(git_exe).parent()?.parent()?;
    Some(
        root.join("bin")
            .join("bash.exe")
            .to_string_lossy()
            .into_owned(),
    )
}

/// The standard Git-for-Windows install locations of `bin\bash.exe`, most
/// canonical first. Mirrors the core harness helper of the same name.
#[cfg(windows)]
fn standard_git_bash_locations(
    program_files: Option<&Path>,
    program_files_x86: Option<&Path>,
    local_app_data: Option<&Path>,
) -> Vec<String> {
    program_files
        .into_iter()
        .chain(program_files_x86)
        .map(|root| root.join("Git").join("bin").join("bash.exe"))
        .chain(local_app_data.into_iter().map(|root| {
            root.join("Programs")
                .join("Git")
                .join("bin")
                .join("bash.exe")
        }))
        .map(|bash| bash.to_string_lossy().into_owned())
        .collect()
}

/// The WSL launcher: a bash.exe that boots Linux, not a shell win32
/// commands can run under (same spelling core's harness filters).
#[cfg(windows)]
fn is_wsl_bash(path: &str) -> bool {
    let normalized = path.replace('/', "\\").to_lowercase();
    normalized == "c:\\windows\\system32\\bash.exe"
        || normalized == "c:\\windows\\sysnative\\bash.exe"
}

/// How a shell program classifies by basename: POSIX-sh-shaped (sh, bash,
/// zsh — git-bash's bash included) or not.
#[cfg(windows)]
pub(crate) fn is_sh_like(shell: &str) -> bool {
    let name = shell
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(shell)
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    matches!(name, "sh" | "bash" | "zsh")
}

/// The argv a one-shot command string runs under: `shell -lc <cmd>` on
/// unix — a login shell assembles PATH from the user's profiles. On Windows
/// the same `-lc` holds when the resolved shell IS an sh (git-bash);
/// anything else is cmd, which takes `/d /s /c` (`/d` skips AutoRun, `/s`
/// keeps the quoting rules). PowerShell is deliberately not special-cased:
/// its sh-incompatible quoting buys nothing over cmd.
pub(crate) fn command_args(shell: &str) -> Vec<&'static str> {
    #[cfg(windows)]
    {
        if !is_sh_like(shell) {
            return vec!["/d", "/s", "/c"];
        }
    }
    #[cfg(not(windows))]
    let _ = shell;
    vec!["-lc"]
}

/// The argv flags that open the shell as a login+interactive session for a
/// user-facing terminal. POSIX shells take `-l -i`; cmd.exe — the Windows
/// fallback — takes none (`/k` and friends are one-shot command spellings,
/// not session flags).
pub(crate) fn interactive_login_args(shell: &str) -> &'static [&'static str] {
    #[cfg(windows)]
    if !is_sh_like(shell) {
        return &[];
    }
    #[cfg(not(windows))]
    let _ = shell;
    &["-l", "-i"]
}

/// The shell recorded for this user in the password database, if any.
#[cfg(unix)]
fn passwd_shell() -> Option<String> {
    {
        // Reentrant lookup: GUI launches need not inherit SHELL from a login shell.
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut storage = vec![0u8; 16384];
        let mut result = std::ptr::null_mut();
        // SAFETY: `entry` and `storage` are writable buffers of the sizes the
        // man page requires; on success `result` points into them (or is
        // null on no-such-user) and `pw_shell` is checked non-null before
        // the read, all within the buffers' lifetime.
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
/// bar, else `/bin/sh` (cmd.exe on Windows — System32 is always on the spawn
/// search path) — so a broken or missing shell record degrades to the one
/// shell the platform guarantees, never to a spawn failure per command.
fn resolve_login_shell(passwd: Option<&OsStr>, env: Option<&OsStr>) -> String {
    passwd
        .into_iter()
        .chain(env)
        .find(|shell| is_executable_file(shell))
        .map(|shell| shell.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "cmd.exe".into()
            } else {
                "/bin/sh".into()
            }
        })
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

/// A bare command name resolved against a PATH value on Windows: the first
/// directory holding the name — as written when it carries an extension,
/// else through each PATHEXT extension (CreateProcess appends only `.exe`,
/// so a bare `npx` — really `npx.cmd` — resolves nowhere without this
/// walk). A name carrying a path separator — or one that resolves nowhere —
/// is `None`, meaning "spawn it as written".
#[cfg(windows)]
pub(crate) fn resolve_command(name: &str, path: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains(['\\', '/']) {
        return None;
    }
    let pathtext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let candidates: Vec<String> = pathtext
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| format!("{name}{extension}"))
        .collect();
    for dir in path.split(';').filter(|dir| !dir.is_empty()) {
        for candidate in std::iter::once(name).chain(candidates.iter().map(String::as_str)) {
            // Absolute candidates only: resolution must never depend on the
            // directory Holt happens to run from (relative PATH entries are
            // skipped, as on unix).
            let candidate = Path::new(dir).join(candidate);
            if candidate.is_absolute() && candidate.is_file() {
                return Some(candidate);
            }
        }
    }
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
        // The one shell each platform guarantees (see resolve_login_shell).
        let fallback = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };
        assert_eq!(resolve_login_shell(None, None), fallback);
        assert_eq!(resolve_login_shell(Some(OsStr::new("")), None), fallback);
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
        // getpwuid_r (COMSPEC on Windows) is the source; the env fallback
        // only fires without one. Absolute in the platform's own grammar.
        assert!(std::path::Path::new(&login_shell()).is_absolute());
    }

    #[test]
    fn agent_shell_is_an_absolute_path() {
        // Git Bash when an install exists, the login shell otherwise —
        // either way a spawnable absolute path.
        assert!(std::path::Path::new(&agent_shell()).is_absolute());
    }

    #[cfg(windows)]
    #[test]
    fn bash_beside_git_targets_the_install_roots_bin() {
        // `cmd\git.exe` and `bin\git.exe` both sit two levels under the
        // install root; a bare name has no root to derive from. Compared
        // as `Path`s so the assertion holds under either separator.
        assert_eq!(
            bash_beside_git("C:/Program Files/Git/cmd/git.exe")
                .as_deref()
                .map(Path::new),
            Some(Path::new("C:/Program Files/Git/bin/bash.exe"))
        );
        assert_eq!(
            bash_beside_git("D:/tools/git/bin/git.exe")
                .as_deref()
                .map(Path::new),
            Some(Path::new("D:/tools/git/bin/bash.exe"))
        );
        assert_eq!(bash_beside_git("git.exe").as_deref().map(Path::new), None);
    }

    #[cfg(windows)]
    #[test]
    fn standard_locations_cover_both_program_files_and_per_user_installs() {
        let locations = standard_git_bash_locations(
            Some(Path::new("C:/Program Files")),
            Some(Path::new("C:/Program Files (x86)")),
            Some(Path::new("C:/Users/u/AppData/Local")),
        );
        let as_paths: Vec<&Path> = locations.iter().map(|p| Path::new(p.as_str())).collect();
        assert_eq!(
            as_paths,
            vec![
                Path::new("C:/Program Files/Git/bin/bash.exe"),
                Path::new("C:/Program Files (x86)/Git/bin/bash.exe"),
                Path::new("C:/Users/u/AppData/Local/Programs/Git/bin/bash.exe"),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn wsl_bash_spellings_are_recognized_across_case_and_separators() {
        assert!(is_wsl_bash("C:\\Windows\\System32\\BASH.EXE"));
        assert!(is_wsl_bash("C:/windows/sysnative/bash.exe"));
        assert!(!is_wsl_bash("C:\\Program Files\\Git\\bin\\bash.exe"));
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
