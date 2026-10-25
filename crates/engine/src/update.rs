//! App self-update behind `UpdateStatus` / `ApplyUpdate` / `CheckUpdate`:
//! polls the latest GitHub release, and on apply downloads this platform's
//! release asset (`update_platform` owns what that means per OS), checks
//! its sha256 against the release's `manifest.json`, and hands it to the
//! platform arm to swap in place of the running install. The download
//! streams progress on the status and can be cancelled until the install
//! starts; restarting onto the new build is the UI's job. Any
//! non-prerelease build reports availability, but only a managed install
//! (an `.app` bundle, a release-marked Windows directory, an AppImage) may
//! apply one. A prerelease build (`-alpha.N`) never polls — testers stay
//! on the build they installed until they replace it by hand, though a
//! manual `CheckUpdate` still runs one round on demand. Debug builds can
//! fake a release with `HOLT_FAKE_UPDATE`.

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use futures::StreamExt;
use holt_proto::{UpdatePhase, UpdateStatus};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, sync::Notify, sync::watch};
use tokio_util::sync::CancellationToken;

use crate::update_platform::{self, InstallTarget};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/Onion-L/holt/releases/latest";
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Minimum gap between download-progress frames.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const CANCELLED: &str = "update cancelled";
/// The release asset carrying per-asset sha256 digests.
const MANIFEST_ASSET: &str = "manifest.json";
/// Upper bound on the manifest download — it is a handful of asset names
/// and hex strings; anything bigger is not ours.
const MANIFEST_LIMIT: usize = 64 * 1024;
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const ARCH: &str = if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "x64"
};

pub(crate) struct Updater {
    tx: watch::Sender<serde_json::Value>,
    inner: Mutex<Inner>,
    /// Wakes the background loop for an immediate round (`CheckUpdate`).
    /// Cloned out so the loop can await it without holding the `Updater`
    /// alive across its sleep.
    wake: std::sync::Arc<Notify>,
    /// Completed-round counter, bumped by [`run_check`] on every round,
    /// successful or not. `check_now` records the current count and waits
    /// for it to advance — reading the current value each pass, so a round
    /// that already settled is never missed.
    rounds: watch::Sender<u64>,
}

#[derive(Default)]
struct Inner {
    status: UpdateStatus,
    /// Download URL of `status.available`'s release asset.
    asset_url: Option<String>,
    /// Manifest digest (hex sha256) expected of that asset. `None` when
    /// the release carries no manifest — then only a platform whose DMG
    /// path verifies a signature may proceed (see
    /// `update_platform::digest_required`).
    asset_digest: Option<String>,
    /// `subscribe` has run once — its one-shot start latch.
    checking: bool,
    /// The background check loop is actually running (it is not, on a
    /// prerelease build or before any subscriber); a `check_now` outside
    /// it runs its round inline.
    loop_running: bool,
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

/// The `manifest.json` release asset: the release's own version plus a
/// sha256 hex per asset name. Published by the release pipeline; without
/// it the checker falls back to the API's tag and downloads without a
/// digest, which only macOS (`update_platform::digest_required`) accepts.
#[derive(Deserialize)]
struct Manifest {
    version: String,
    #[serde(default)]
    artifacts: HashMap<String, String>,
}

impl Manifest {
    fn digest_for(&self, asset: &str) -> Option<&str> {
        self.artifacts.get(asset).map(|hex| hex.as_str())
    }
}

impl Updater {
    pub(crate) fn new() -> Arc<Self> {
        // Best-effort: drop the previous update's leftovers (the old
        // binary that could not be deleted while it ran).
        update_platform::cleanup_stale();
        let status = UpdateStatus {
            current_version: CURRENT_VERSION.to_string(),
            // A debug fake reports managed so the whole apply flow can be
            // exercised; otherwise the running install decides.
            managed: fake_update() || update_platform::managed_install().is_some(),
            ..Default::default()
        };
        let (tx, _) = watch::channel(serde_json::to_value(&status).unwrap_or_default());
        let (rounds, _) = watch::channel(0u64);
        Arc::new(Self {
            tx,
            inner: Mutex::new(Inner {
                status,
                ..Default::default()
            }),
            wake: std::sync::Arc::new(Notify::new()),
            rounds,
        })
    }

    /// Subscribe to status frames; the first subscriber starts the check
    /// loop (it needs the runtime, which `assemble` does not have). Any
    /// non-prerelease build checks — an unmanaged install (`cargo run`, a
    /// hand-copied binary) still reports availability; only applying is
    /// gated on a managed install.
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
        } else if start && !CURRENT_VERSION.contains('-') {
            self.lock().loop_running = true;
            tokio::spawn(check_loop(Arc::downgrade(self)));
        }
        self.tx.subscribe()
    }

    /// One immediate availability round (`CheckUpdate`), replying with the
    /// status after that round. With the background loop running, wake it
    /// and wait for its next completed round; without one (a prerelease
    /// build, or no subscriber ever started it), run the shared round
    /// function inline. A `HOLT_FAKE_UPDATE` debug install is an
    /// immediate no-op — its fake release is already in place.
    pub(crate) async fn check_now(self: &Arc<Self>) -> UpdateStatus {
        if self.lock().loop_running {
            let mut rounds = self.rounds.subscribe();
            let before = *rounds.borrow_and_update();
            self.wake.notify_one();
            // Read the current value each pass, not only the next change:
            // the woken round may already have completed (or this call may
            // have landed inside a round that was already in flight).
            while *rounds.borrow() <= before {
                if rounds.changed().await.is_err() {
                    break;
                }
            }
        } else if !fake_update() {
            run_check(self).await;
        }
        self.lock().status.clone()
    }

    /// Download and install the available release. Returns once the new
    /// build is in place (`Ready`), or `Ok` on a cancel.
    pub(crate) async fn apply(&self) -> Result<(), String> {
        let target = if fake_update() {
            None
        } else {
            Some(
                update_platform::managed_install()
                    .ok_or("Holt is not running from a managed install")?,
            )
        };
        let token = CancellationToken::new();
        let (version, url, digest) = {
            let mut inner = self.lock();
            if inner.status.phase != UpdatePhase::Idle {
                return Err("an update is already in progress".into());
            }
            let (Some(version), Some(url)) =
                (inner.status.available.clone(), inner.asset_url.clone())
            else {
                return Err("no update available".into());
            };
            let digest = inner.asset_digest.clone();
            inner.status.phase = UpdatePhase::Downloading;
            inner.status.downloaded = 0;
            inner.status.total = None;
            inner.status.error = None;
            inner.cancel = Some(token.clone());
            (version, url, digest)
        };
        self.publish();
        let result = match target {
            Some(target) => {
                self.install(&version, &url, digest.as_deref(), target, &token)
                    .await
            }
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
        digest: Option<&str>,
        target: InstallTarget,
        token: &CancellationToken,
    ) -> Result<(), String> {
        let work = std::env::temp_dir().join(format!("holt-update-{version}"));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        let downloaded = work.join("release-download");
        let verified = async {
            self.download(url, &downloaded, digest, token).await?;
            // The digest gate precedes the no-cancel point: only a fully
            // verified download may begin installing.
            self.begin_install(token)
        }
        .await;
        if let Err(error) = verified {
            let _ = std::fs::remove_dir_all(&work);
            return Err(error);
        }
        // The platform arms clean `work` up on success and failure alike.
        tokio::task::spawn_blocking(move || update_platform::install(&downloaded, &work, target))
            .await
            .map_err(|e| e.to_string())?
    }

    async fn download(
        &self,
        url: &str,
        to: &Path,
        expected: Option<&str>,
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
        let mut file = tokio::fs::File::create(to)
            .await
            .map_err(|e| e.to_string())?;
        let mut hasher = Sha256::new();
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
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            downloaded += chunk.len() as u64;
            if published.elapsed() >= PROGRESS_INTERVAL {
                published = Instant::now();
                self.set_progress(downloaded, total);
            }
        }
        file.flush().await.map_err(|e| e.to_string())?;
        self.set_progress(downloaded, total);
        if let Some(expected) = expected
            && !digest_matches(&hasher.finalize(), expected)
        {
            let _ = std::fs::remove_file(to);
            return Err("digest mismatch".into());
        }
        Ok(())
    }

    /// `HOLT_FAKE_UPDATE`'s stand-in for [`Self::install`]: ticks a 48 MB
    /// download over ~6 s, then "installs" without touching the install.
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
    /// no longer land (the swap must not stop halfway).
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
    let Some(start) = updater.upgrade() else {
        return;
    };
    // The wake handle outlives each upgrade: holding the `Updater` across
    // the sleep would keep it alive forever (the loop is its own last
    // strong reference otherwise).
    let wake = start.wake.clone();
    drop(start);
    let mut failures = 0u32;
    loop {
        let succeeded = match updater.upgrade() {
            Some(updater) => run_check(&updater).await,
            None => return,
        };
        failures = if succeeded { 0 } else { failures + 1 };
        tokio::select! {
            // A stored permit survives until the next `notified()` — a
            // wake that lands mid-round is consumed after it.
            _ = wake.notified() => {}
            _ = tokio::time::sleep(next_delay(failures)) => {}
        }
        if updater.strong_count() == 0 {
            return;
        }
    }
}

/// One availability round, shared by the background loop and
/// [`Updater::check_now`]: fetch the latest release and its manifest, fold
/// the verdict into the status, then count the round as completed (what a
/// waiting `check_now` returns on). Returns whether the round succeeded,
/// for the loop's backoff.
async fn run_check(updater: &Updater) -> bool {
    let succeeded = match latest_release().await {
        Ok(release) => {
            let manifest = fetch_manifest(&release.assets).await;
            let version = manifest
                .as_ref()
                .map(|m| m.version.clone())
                .unwrap_or_else(|| release.tag_name.trim_start_matches('v').to_string());
            let name = asset_name(&version, ARCH, update_platform::asset_suffix());
            let asset = release.assets.into_iter().find(|a| a.name == name);
            let digest = manifest
                .as_ref()
                .and_then(|m| m.digest_for(&name))
                .map(str::to_string);
            let newer = is_newer(&version, CURRENT_VERSION);
            // Fail closed: without a digest, only a platform whose
            // install arm verifies a signature may offer the update.
            let provable = digest.is_some() || !update_platform::digest_required();
            let changed = {
                let mut inner = updater.lock();
                let available = (newer && asset.is_some() && provable).then_some(version);
                let changed =
                    inner.status.phase == UpdatePhase::Idle && inner.status.available != available;
                if changed {
                    inner.status.available = available;
                    inner.asset_url = asset.map(|a| a.browser_download_url);
                    inner.asset_digest = digest;
                }
                changed
            };
            if changed {
                updater.publish();
            }
            true
        }
        Err(error) => {
            tracing::debug!(%error, "update check failed");
            false
        }
    };
    // Even a failed round is a completed one — a manual check must not
    // wait out the failure backoff for its reply.
    updater.rounds.send_modify(|rounds| *rounds += 1);
    succeeded
}

/// Download and parse the release's `manifest.json` when it carries one.
/// Any failure is logged and treated as absence — the caller falls back
/// to the API path, and digest-requiring platforms simply stay
/// unavailable (fail closed).
async fn fetch_manifest(assets: &[Asset]) -> Option<Manifest> {
    let url = assets
        .iter()
        .find(|a| a.name == MANIFEST_ASSET)?
        .browser_download_url
        .clone();
    let bytes = client()
        .ok()?
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| tracing::debug!(%e, "update manifest fetch failed"))
        .ok()?
        .bytes()
        .await
        .map_err(|e| tracing::debug!(%e, "update manifest read failed"))
        .ok()?;
    if bytes.len() > MANIFEST_LIMIT {
        tracing::debug!("update manifest exceeds {} bytes", MANIFEST_LIMIT);
        return None;
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| tracing::debug!(%e, "update manifest unparsable"))
        .ok()
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

/// `Holt-<version>-<arch><suffix>` — the release asset naming the site's
/// download page already matches on.
fn asset_name(version: &str, arch: &str, suffix: &str) -> String {
    format!("Holt-{version}-{arch}{suffix}")
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

/// Sleep before the next check after `consecutive_failures` failures in a
/// row: the usual `CHECK_INTERVAL` after a success, else 15 minutes
/// doubling per failure and capped at `CHECK_INTERVAL`; a success resets
/// the count.
fn next_delay(consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return CHECK_INTERVAL;
    }
    const BASE: Duration = Duration::from_secs(15 * 60);
    let shift = (consecutive_failures - 1).min(16);
    BASE.saturating_mul(1u32 << shift).min(CHECK_INTERVAL)
}

/// Hex sha256 comparison, case-insensitive (our manifests are lowercase;
/// a hand-assembled one may not be).
fn digest_matches(computed: &[u8], expected: &str) -> bool {
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(computed.len() * 2);
    for byte in computed {
        let _ = write!(hex, "{byte:02x}");
    }
    hex.eq_ignore_ascii_case(expected)
}

/// Debug builds only: `HOLT_FAKE_UPDATE` offers a fake release, so the
/// update flow can be exercised from `cargo run`.
fn fake_update() -> bool {
    cfg!(debug_assertions) && std::env::var_os("HOLT_FAKE_UPDATE").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_compares_numerically() {
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.5", "0.1.5"));
        assert!(!is_newer("0.1.4", "0.1.5"));
        assert!(!is_newer("0.1.5-beta", "0.1.5"));
        // A pre-release sorts as its base: newer base still wins.
        assert!(is_newer("0.2.0-beta", "0.1.9"));
        // Build metadata is not a version: same base is not newer.
        assert!(!is_newer("0.2.0+build.7", "0.2.0"));
    }

    #[test]
    fn asset_names_match_the_site_naming() {
        assert_eq!(asset_name("0.7.0", "arm64", ".dmg"), "Holt-0.7.0-arm64.dmg");
        assert_eq!(
            asset_name("0.7.0", "x64", "-win.zip"),
            "Holt-0.7.0-x64-win.zip"
        );
        assert_eq!(
            asset_name("0.7.0", "x64", ".AppImage"),
            "Holt-0.7.0-x64.AppImage"
        );
    }

    #[test]
    fn manifest_carries_version_and_per_asset_digests() {
        let manifest: Manifest = serde_json::from_str(
            r#"{"version":"0.7.0","artifacts":{"Holt-0.7.0-x64-win.zip":"abc0"}}"#,
        )
        .unwrap();
        assert_eq!(manifest.version, "0.7.0");
        assert_eq!(manifest.digest_for("Holt-0.7.0-x64-win.zip"), Some("abc0"));
        assert_eq!(manifest.digest_for("missing"), None);
    }

    #[test]
    fn manifest_without_artifacts_still_parses() {
        let manifest: Manifest = serde_json::from_str(r#"{"version":"0.7.0"}"#).unwrap();
        assert_eq!(manifest.digest_for("anything"), None);
    }

    #[test]
    fn manifest_rejects_broken_json_and_a_missing_version() {
        assert!(serde_json::from_str::<Manifest>("{broken").is_err());
        assert!(serde_json::from_str::<Manifest>(r#"{"artifacts":{}}"#).is_err());
    }

    #[test]
    fn backoff_starts_at_15_minutes_doubles_and_caps() {
        assert_eq!(next_delay(0), CHECK_INTERVAL);
        assert_eq!(next_delay(1), Duration::from_secs(15 * 60));
        assert_eq!(next_delay(2), Duration::from_secs(30 * 60));
        assert_eq!(next_delay(5), Duration::from_secs(4 * 60 * 60));
        // 8 h would exceed the cap.
        assert_eq!(next_delay(6), CHECK_INTERVAL);
        assert_eq!(next_delay(1000), CHECK_INTERVAL);
    }

    /// The download gate's comparison against a known-answer digest of
    /// the bytes on disk.
    #[test]
    fn digests_compare_as_case_insensitive_hex() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asset");
        std::fs::write(&path, b"payload").unwrap();
        // sha256("payload"), computed independently of the code under test.
        let expected = "239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5";
        let digest = Sha256::digest(b"payload");
        assert!(digest_matches(&digest, expected));
        assert!(digest_matches(&digest, &expected.to_uppercase()));
        // Tampered content does not match the payload's digest, and an
        // all-zero hex of the right length does not either.
        let tampered = Sha256::digest(b"tampered");
        assert_ne!(tampered, digest);
        assert!(!digest_matches(&tampered, expected));
        assert!(!digest_matches(&digest, &"0".repeat(64)));
    }

    /// The `CheckUpdate` handshake: with the loop running, `check_now`
    /// returns once a round completes — including one that settles while
    /// the caller is between reading the counter and waiting (the wait
    /// re-reads the current value each pass, never only the next change).
    /// No network here: the round counter is advanced by hand.
    #[tokio::test]
    async fn check_now_returns_when_a_round_completes() {
        let updater = Updater::new();
        updater.lock().loop_running = true;
        let check = {
            let updater = updater.clone();
            tokio::spawn(async move { updater.check_now().await })
        };
        // Let the caller reach its wait, then complete a round.
        tokio::task::yield_now().await;
        updater.rounds.send_modify(|rounds| *rounds += 1);
        let status = tokio::time::timeout(Duration::from_secs(5), check)
            .await
            .expect("the completed round releases the caller")
            .unwrap();
        assert_eq!(status.current_version, CURRENT_VERSION);
        assert_eq!(status.phase, UpdatePhase::Idle);
    }
}
