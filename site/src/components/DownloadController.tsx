import { useEffect } from 'react';
import { detectArch, detectPlatform, isMobile } from './detect';

// React controller for the download page (/download). The page is static
// Astro markup — one column per OS, one row per build — and this island
// renders nothing. It only
//  - marks the visitor's column and, inside it, the build for their chip,
//  - fills every version stamp and every link from the latest GitHub release,
//  - hides a build (and then a column) the release carries no asset for, so
//    the page never offers a download that is not there.
// A link matches its asset by the `data-asset` suffix — the tail of the
// release asset name (`-arm64.dmg`, `-x64-win.zip`, `-x64.AppImage`) — so
// shipping a new format only needs its anchor in the markup.

type Asset = { name: string; size: number; browser_download_url: string };
type Release = { tag_name: string; assets: Asset[] };

const RELEASE_API = 'https://api.github.com/repos/Onion-L/holt/releases/latest';

/** The `hidden` attribute, which CSS cannot outrank on its own. */
function hide(el: Element) {
  el.setAttribute('hidden', '');
}

function visibleAssetLinks(root: ParentNode): HTMLAnchorElement[] {
  return Array.from(root.querySelectorAll<HTMLAnchorElement>('a[data-asset]:not([hidden])'));
}

export default function DownloadController() {
  useEffect(() => {
    (async () => {
      const cols = Array.from(document.querySelectorAll<HTMLElement>('.col[data-platform]'));

      // The visitor's own column, and the build for the chip we could detect.
      // A phone gets no "your system" mark: every artifact here is a desktop
      // binary it cannot run.
      if (!isMobile()) {
        const yours = cols.find((col) => col.dataset.platform === detectPlatform());
        if (yours) {
          yours.setAttribute('data-yours', '');
          const arch = await detectArch();
          if (arch) {
            yours
              .querySelector<HTMLElement>(`[data-arch="${arch}"]`)
              ?.setAttribute('data-pick', '');
          }
        }
      }

      try {
        const response = await fetch(RELEASE_API);
        if (!response.ok) return;
        const release: Release = await response.json();

        document.querySelectorAll('[data-version]').forEach((el) => {
          el.textContent = release.tag_name.replace(/^v/, '');
        });

        document.querySelectorAll<HTMLAnchorElement>('a[data-asset]').forEach((anchor) => {
          const suffix = anchor.dataset.asset;
          const asset = suffix
            ? release.assets.find((candidate) => candidate.name.endsWith(suffix))
            : undefined;
          if (asset) {
            anchor.href = asset.browser_download_url;
            // A format the markup ships hidden (one we may not build yet)
            // appears only once the release carries it.
            anchor.removeAttribute('hidden');
            anchor.removeAttribute('data-unavailable');
            return;
          }
          // Formats we have not committed to stay out of the layout; the
          // builds we do ship stay visible but go grey and inert, so the
          // platform matrix never collapses to whatever is published today.
          if (anchor.classList.contains('fmt') && anchor.hasAttribute('hidden')) return;
          anchor.setAttribute('data-unavailable', '');
        });

        document.querySelectorAll<HTMLElement>('.row').forEach((row) => {
          const anchor = row.querySelector<HTMLAnchorElement>('a[data-asset]');
          if (anchor?.hasAttribute('data-unavailable')) row.setAttribute('data-unavailable', '');
        });
        document.querySelectorAll<HTMLElement>('.card').forEach((card) => {
          const formats = Array.from(
            card.querySelectorAll<HTMLAnchorElement>('.fmt:not([hidden])'),
          );
          if (formats.length && formats.every((f) => f.hasAttribute('data-unavailable'))) {
            card.setAttribute('data-unavailable', '');
          }
        });
        cols.forEach((col) => {
          const offered = col.querySelector(
            'a[data-asset]:not([hidden]):not([data-unavailable])',
          );
          const soon = col.querySelector<HTMLElement>('.soon');
          if (offered) {
            soon?.setAttribute('hidden', '');
            return;
          }
          col.setAttribute('data-unavailable', '');
          soon?.removeAttribute('hidden');
        });

        // The format to lead with inside the visitor's Linux card: the first
        // one this release actually shipped (`.deb` before `.AppImage`).
        cols.forEach((col) => {
          const picked = col.querySelector<HTMLElement>('[data-arch][data-pick]');
          picked
            ?.querySelector<HTMLAnchorElement>('.fmt:not([hidden]):not([data-unavailable])')
            ?.setAttribute('data-pick', '');
        });
      } catch {
        // offline or rate-limited: the static hrefs (the releases page) remain
      }
    })();
  }, []);

  return null;
}
