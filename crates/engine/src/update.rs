//! App self-update behind `UpdateStatus` / `ApplyUpdate`: polls the latest
//! GitHub release, and on apply downloads this arch's DMG, verifies the
//! bundle's signature against the release team, and swaps it in place of
//! the running `.app`. The download streams progress on the status and can
//! be cancelled until the install starts; restarting onto the new bundle is
//! the UI's job. A binary not running from an `.app` bundle (`cargo run`)
//! never checks — debug builds can fake a release with `HOLT_FAKE_UPDATE`.

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use futures::StreamExt;
use holt_proto::{UpdatePhase, UpdateStatus};
use serde::Deserialize;
use tokio::{io::AsyncWriteExt, sync::watch};
use tokio_util::sync::CancellationToken;

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/Onion-L/holt/releases/latest";
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Minimum gap between download-progress frames.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const CANCELLED: &str = "update cancelled";
/// Designated requirement every release bundle must satisfy (the Developer
/// ID team `scripts/build-dmg.sh` signs with).
const CODE_REQUIREMENT: &str =
    "anchor apple generic and certificate leaf[subject.OU] = \"AQMMZQ3MDX\"";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const ARCH: &str = if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "x64"
};

pub(crate) struct Updater {
    tx: watch::Sender<serde_json::Value>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    status: UpdateStatus,
    /// Download URL of `status.available`'s DMG.
    asset_url: Option<String>,
    checking: bool,
    /// Aborts the running download; `None` outside `Downloading`.
    cancel: Option<CancellationToken>,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

impl Updater {
    pub(crate) fn new() -> Arc<Self> {
        let status = UpdateStatus {
            current_version: CURRENT_VERSION.to_string(),
            ..Default::default()
        };
        let (tx, _) = watch::channel(serde_json::to_value(&status).unwrap_or_default());
        Arc::new(Self {
            tx,
            inner: Mutex::new(Inner {
                status,
                ..Default::default()
            }),
        })
    }

    /// Subscribe to status frames; the first subscriber starts the check
    /// loop (it needs the runtime, which `assemble` does not have).
    pub(crate) fn subscribe(self: &Arc<Self>) -> watch::Receiver<serde_json::Value> {
        let start = {
            let mut inner = self.lock();
            !std::mem::replace(&mut inner.checking, true)
        };
        if start && fake_update() {
            {
                let mut inner = self.lock();
                inner.status.available = Some("99.0.0".into());
                inner.asset_url = Some(String::new());
            }
            self.publish();
        } else if start && bundle_path().is_some() {
            tokio::spawn(check_loop(Arc::downgrade(self)));
        }
        self.tx.subscribe()
    }

    /// Download and install the available release. Returns once the new
    /// bundle is in place (`Ready`), or `Ok` on a cancel.
    pub(crate) async fn apply(&self) -> Result<(), String> {
        let bundle = if fake_update() {
            None
        } else {
            Some(bundle_path().ok_or("Holt is not running from an app bundle")?)
        };
        let token = CancellationToken::new();
        let (version, url) = {
            let mut inner = self.lock();
            if inner.status.phase != UpdatePhase::Idle {
                return Err("an update is already in progress".into());
            }
            let (Some(version), Some(url)) =
                (inner.status.available.clone(), inner.asset_url.clone())
            else {
                return Err("no update available".into());
            };
            inner.status.phase = UpdatePhase::Downloading;
            inner.status.downloaded = 0;
            inner.status.total = None;
            inner.status.error = None;
            inner.cancel = Some(token.clone());
            (version, url)
        };
        self.publish();
        let result = match bundle {
            Some(bundle) => self.install(&version, &url, bundle, &token).await,
            None => self.fake_install(&token).await,
        };
        let cancelled = token.is_cancelled() && result.is_err();
        {
            let mut inner = self.lock();
            inner.cancel = None;
            match &result {
                Ok(()) => inner.status.phase = UpdatePhase::Ready,
                Err(error) => {
                    inner.status.phase = UpdatePhase::Idle;
                    inner.status.downloaded = 0;
                    inner.status.total = None;
                    inner.status.error = (!cancelled).then(|| error.clone());
                }
            }
        }
        self.publish();
        if cancelled { Ok(()) } else { result }
    }

    /// Abort a download in flight; a no-op once the install has started.
    pub(crate) fn cancel(&self) {
        if let Some(token) = &self.lock().cancel {
            token.cancel();
        }
    }

    async fn install(
        &self,
        version: &str,
        url: &str,
        bundle: PathBuf,
        token: &CancellationToken,
    ) -> Result<(), String> {
        let work = std::env::temp_dir().join(format!("holt-update-{version}"));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        let dmg = work.join("Holt.dmg");
        let downloaded = async {
            self.download(url, &dmg, token).await?;
            self.begin_install(token)
        }
        .await;
        if let Err(error) = downloaded {
            let _ = std::fs::remove_dir_all(&work);
            return Err(error);
        }
        tokio::task::spawn_blocking(move || swap_from_dmg(&dmg, &work, &bundle))
            .await
            .map_err(|e| e.to_string())?
    }

    async fn download(
        &self,
        url: &str,
        dmg: &Path,
        token: &CancellationToken,
    ) -> Result<(), String> {
        let failed = |e: reqwest::Error| format!("download failed: {e}");
        let response = client()?
            .get(url)
            .timeout(Duration::from_secs(15 * 60))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(failed)?;
        let total = response.content_length();
        self.set_progress(0, total);
        let mut file = tokio::fs::File::create(dmg)
            .await
            .map_err(|e| e.to_string())?;
        let mut stream = response.bytes_stream();
        let mut downloaded = 0;
        let mut published = Instant::now();
        loop {
            let chunk = tokio::select! {
                _ = token.cancelled() => return Err(CANCELLED.into()),
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = chunk else {
                break;
            };
            let chunk = chunk.map_err(failed)?;
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            downloaded += chunk.len() as u64;
            if published.elapsed() >= PROGRESS_INTERVAL {
                published = Instant::now();
                self.set_progress(downloaded, total);
            }
        }
        file.flush().await.map_err(|e| e.to_string())?;
        self.set_progress(downloaded, total);
        Ok(())
    }

    /// `HOLT_FAKE_UPDATE`'s stand-in for [`Self::install`]: ticks a 48 MB
    /// download over ~6 s, then "installs" without touching the bundle.
    async fn fake_install(&self, token: &CancellationToken) -> Result<(), String> {
        const TOTAL: u64 = 48 << 20;
        let mut downloaded = 0;
        while downloaded < TOTAL {
            tokio::select! {
                _ = token.cancelled() => return Err(CANCELLED.into()),
                _ = tokio::time::sleep(PROGRESS_INTERVAL) => {}
            }
            downloaded = (downloaded + TOTAL / 60).min(TOTAL);
            self.set_progress(downloaded, Some(TOTAL));
        }
        self.begin_install(token)?;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        Ok(())
    }

    fn set_progress(&self, downloaded: u64, total: Option<u64>) {
        {
            let mut inner = self.lock();
            inner.status.downloaded = downloaded;
            inner.status.total = total;
        }
        self.publish();
    }

    /// Leave `Downloading` for `Installing`; past this point a cancel can
    /// no longer land (the bundle swap must not stop halfway).
    fn begin_install(&self, token: &CancellationToken) -> Result<(), String> {
        {
            let mut inner = self.lock();
            inner.cancel = None;
            if token.is_cancelled() {
                return Err(CANCELLED.into());
            }
            inner.status.phase = UpdatePhase::Installing;
        }
        self.publish();
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self) {
        let value = serde_json::to_value(&self.lock().status).unwrap_or_default();
        self.tx.send_replace(value);
    }
}

async fn check_loop(updater: Weak<Updater>) {
    loop {
        match latest_release().await {
            Ok(release) => {
                let Some(updater) = updater.upgrade() else {
                    return;
                };
                let version = release.tag_name.trim_start_matches('v').to_string();
                let asset_name = format!("Holt-{version}-{ARCH}.dmg");
                let asset = release.assets.into_iter().find(|a| a.name == asset_name);
                let newer = is_newer(&version, CURRENT_VERSION);
                let changed = {
                    let mut inner = updater.lock();
                    let available = (newer && asset.is_some()).then_some(version);
                    let changed = inner.status.phase == UpdatePhase::Idle
                        && inner.status.available != available;
                    if changed {
                        inner.status.available = available;
                        inner.asset_url = asset.map(|a| a.browser_download_url);
                    }
                    changed
                };
                if changed {
                    updater.publish();
                }
            }
            Err(error) => tracing::debug!(%error, "update check failed"),
        }
        tokio::time::sleep(CHECK_INTERVAL).await;
        if updater.strong_count() == 0 {
            return;
        }
    }
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("holt/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())
}

async fn latest_release() -> Result<Release, String> {
    client()?
        .get(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

/// `major.minor.patch` numeric compare; a pre-release suffix sorts as its
/// base version.
fn is_newer(candidate: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split(['-', '+'])
            .next()
            .unwrap_or_default()
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    }
    parts(candidate) > parts(current)
}

/// Debug builds only: `HOLT_FAKE_UPDATE` offers a fake release, so the
/// update flow can be exercised from `cargo run`.
fn fake_update() -> bool {
    cfg!(debug_assertions) && std::env::var_os("HOLT_FAKE_UPDATE").is_some()
}

/// `…/Holt.app` when the running binary sits at `Holt.app/Contents/MacOS/`.
fn bundle_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let bundle = exe.parent()?.parent()?.parent()?;
    (bundle.extension()? == "app").then(|| bundle.to_path_buf())
}

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
    use super::is_newer;

    #[test]
    fn newer_compares_numerically() {
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.5", "0.1.5"));
        assert!(!is_newer("0.1.4", "0.1.5"));
        assert!(!is_newer("0.1.5-beta", "0.1.5"));
    }
}
