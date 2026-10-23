import { useEffect } from 'react';
import { detectArch, detectPlatform, isMobile } from './detect';

// React controller for the download page (/download). The page is static
// Astro markup; this island renders nothing and only
//  - marks the visitor's platform card and, on that card, the chip that
//    matches,
//  - fills the version stamp and every link from the latest GitHub release,
//  - demotes a platform the release carries no asset for (the stamp says so
//    and the highlight comes off), so the page never advertises a build that
//    is not downloadable.
// A link matches its asset by the `data-asset` suffix (the tail of the
// release asset name) and fills the size in its `data-download` cell, so a
// platform shipping builds only needs its markup updated — give the cell an
// anchor with the suffix.

type Asset = { name: string; size: number; browser_download_url: string };
type Release = { tag_name: string; assets: Asset[] };

const RELEASE_API = 'https://api.github.com/repos/Onion-L/holt/releases/latest';

function megabytes(bytes: number): string {
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/**
 * Mark a platform card as absent from this release: no asset matched any of
 * its links, so the card drops the highlight and says so instead of offering
 * a download that does not exist.
 */
function markMissing(card: HTMLElement) {
  card.setAttribute('data-missing', '');
  card
    .querySelectorAll<HTMLAnchorElement>('a[data-pick]')
    .forEach((a) => a.removeAttribute('data-pick'));
  const stamp = card.querySelector<HTMLElement>('[data-stamp]');
  if (stamp) stamp.textContent = 'next release';
}

export default function DownloadController() {
  useEffect(() => {
    (async () => {
      const cards = Array.from(
        document.querySelectorAll<HTMLElement>('[data-platform]'),
      );
      const yours = isMobile()
        ? null
        : (cards.find((card) => card.dataset.platform === detectPlatform()) ?? null);
      if (yours) {
        yours.setAttribute('data-yours', '');
        // The chip highlight only means something inside the visitor's own
        // card: an arm64 Windows box must not light up the Linux arm64 build.
        const arch = await detectArch();
        if (arch) {
          yours.querySelectorAll<HTMLAnchorElement>('a[data-arch]').forEach((a) => {
            a.toggleAttribute('data-pick', a.dataset.arch === arch);
          });
        }
      }
      try {
        const r = await fetch(RELEASE_API);
        if (!r.ok) return;
        const rel: Release = await r.json();
        document
          .querySelectorAll('[data-version]')
          .forEach((el) => (el.textContent = rel.tag_name.replace(/^v/, '')));
        const found = new Map<HTMLElement, boolean>();
        const missing = new Set<HTMLElement>();
        document.querySelectorAll<HTMLAnchorElement>('a[data-asset]').forEach((a) => {
          const suffix = a.dataset.asset;
          const card = a.closest<HTMLElement>('[data-platform]');
          const asset = suffix ? rel.assets.find((x) => x.name.endsWith(suffix)) : undefined;
          if (card) {
            found.set(card, (found.get(card) ?? false) || Boolean(asset));
            if (!asset) missing.add(card);
          }
          if (!asset) return;
          a.href = asset.browser_download_url;
          const cell = a.closest('[data-download]') ?? a;
          cell.querySelector<HTMLElement>('[data-size]')?.replaceChildren(` · ${megabytes(asset.size)}`);
        });
        // A card whose every link missed ships nothing in this release.
        missing.forEach((card) => {
          if (!found.get(card)) markMissing(card);
        });
      } catch {
        // offline or rate-limited: static hrefs (the releases page) remain
      }
    })();
  }, []);

  return null;
}
