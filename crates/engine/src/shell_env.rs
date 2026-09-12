use std::{process::Command, sync::LazyLock};

pub(crate) fn login_shell() -> String {
    #[cfg(unix)]
    {
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
                    return shell.into_owned();
                }
            }
        }
    }
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

pub(crate) fn login_shell_path() -> &'static str {
    static PATH: LazyLock<String> = LazyLock::new(|| {
        let fallback =
            || std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin:/usr/sbin:/sbin".into());
        Command::new(login_shell())
            .args(["-lic", "printf %s \"$PATH\""])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    String::from_utf8(o.stdout).ok()
                } else {
                    None
                }
            })
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(fallback)
    });
    &PATH
}
