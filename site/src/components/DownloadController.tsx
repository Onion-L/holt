import { useEffect } from 'react';
import { detectArch, detectPlatform, isMobile } from './detect';

// React controller for the download page (/download): one panel per OS with a
// chip selector and a single action. The page is static Astro markup; this
// island renders nothing and only
//  - marks the visitor's OS row and preselects their chip,
//  - fills the version, its date and the changelog link from the latest
//    GitHub release,
//  - points each Download button at the selected asset, and takes the whole
//    row's action away when the release carries no build for it.
// A selector option's value IS its asset suffix (`-arm64.dmg`,
// `-x64-win.zip`, `-x64-setup.exe`, `-x64.AppImage`), so shipping another
// format means adding an option to the markup and nothing else.

type Asset = { name: string; size: number; browser_download_url: string };
type Release = { tag_name: string; published_at: string; assets: Asset[] };

const RELEASE_API = 'https://api.github.com/repos/Onion-L/holt/releases/latest';

function assetFor(release: Release, suffix: string): Asset | undefined {
  return release.assets.find((asset) => asset.name.endsWith(suffix));
}

/** Point the row's one button at the selected option's asset. */
function syncRow(row: HTMLElement, release: Release | null) {
  const select = row.querySelector<HTMLSelectElement>('select[data-arch-select]');
  const button = row.querySelector<HTMLAnchorElement>('a[data-download]');
  if (!select || !button) return;
  const asset = release ? assetFor(release, select.value) : undefined;
  if (asset) {
    button.href = asset.browser_download_url;
    button.removeAttribute('aria-disabled');
    row.removeAttribute('data-unavailable');
  } else {
    // No build for this platform in this release: the row stays (the platform
    // matrix is the page) but its action is not offered.
    button.setAttribute('aria-disabled', 'true');
    row.setAttribute('data-unavailable', '');
  }
}

export default function DownloadController() {
  useEffect(() => {
    let release: Release | null = null;

    const rows = Array.from(document.querySelectorAll<HTMLElement>('.os[data-platform]'));
    rows.forEach((row) => {
      row
        .querySelector<HTMLSelectElement>('select[data-arch-select]')
        ?.addEventListener('change', () => syncRow(row, release));
    });

    (async () => {
      // The visitor's own row, with their chip preselected where we can tell.
      const platform = isMobile() ? null : detectPlatform();
      const yours = platform ? rows.find((row) => row.dataset.platform === platform) : undefined;
      const arch = yours ? await detectArch() : null;
      if (yours) {
        yours.setAttribute('data-yours', '');
        const select = yours.querySelector<HTMLSelectElement>('select[data-arch-select]');
        const match = arch
          ? Array.from(select?.options ?? []).find((option) => option.value.includes(arch))
          : undefined;
        if (select && match) select.value = match.value;
      }

      try {
        const response = await fetch(RELEASE_API);
        if (!response.ok) return;
        release = (await response.json()) as Release;

        document.querySelectorAll('[data-version]').forEach((el) => {
          el.textContent = release!.tag_name.replace(/^v/, '');
        });
        const date = new Date(release.published_at);
        if (!Number.isNaN(date.getTime())) {
          document.querySelectorAll('[data-date]').forEach((el) => {
            el.textContent = date.toLocaleDateString('en-US', {
              month: 'long',
              day: 'numeric',
              year: 'numeric',
            });
          });
        }

        rows.forEach((row) => {
          const select = row.querySelector<HTMLSelectElement>('select[data-arch-select]');
          if (select) {
            let firstAvailable: HTMLOptionElement | undefined;
            for (const option of Array.from(select.options)) {
              const asset = assetFor(release!, option.value);
              option.disabled = !asset;
              if (asset && !firstAvailable) firstAvailable = option;
            }
            // A preselected chip the release does not carry falls back to the
            // first one it does — on the visitor's own row, first to another
            // build of their arch (an ARM64 Windows visitor keeps the native
            // zip over an emulated x64 installer).
            const current = Array.from(select.options).find((o) => o.value === select.value);
            if (current?.disabled && firstAvailable) {
              const sameArch =
                row === yours && arch
                  ? Array.from(select.options).find((o) => !o.disabled && o.value.includes(arch))
                  : undefined;
              select.value = (sameArch ?? firstAvailable).value;
            }
            // Nothing to choose from: the selector greys out with the button.
            select.disabled = !firstAvailable;
          }
          syncRow(row, release);
        });
      } catch {
        // offline or rate-limited: the static hrefs (the releases page) remain
      }
    })();
  }, []);

  return null;
}
