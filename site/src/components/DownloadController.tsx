import { useEffect } from 'react';
import { detectArch, detectPlatform } from './detect';

// React controller for the download page (/download). The page is static
// Astro markup; this island renders nothing and only
//  - marks the visitor's platform card and, on a Mac, the chip that matches,
//  - fills the version stamp and every link from the latest GitHub release.
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

export default function DownloadController() {
  useEffect(() => {
    (async () => {
      const platform = detectPlatform();
      document.querySelectorAll<HTMLElement>('[data-platform]').forEach((card) => {
        card.toggleAttribute('data-yours', card.dataset.platform === platform);
      });
      if (platform === 'macos') {
        const arch = await detectArch();
        document.querySelectorAll<HTMLAnchorElement>('a[data-arch]').forEach((a) => {
          a.toggleAttribute('data-pick', a.dataset.arch === arch);
        });
      }
      try {
        const r = await fetch(RELEASE_API);
        if (!r.ok) return;
        const rel: Release = await r.json();
        document
          .querySelectorAll('[data-version]')
          .forEach((el) => (el.textContent = rel.tag_name.replace(/^v/, '')));
        document.querySelectorAll<HTMLAnchorElement>('a[data-asset]').forEach((a) => {
          const suffix = a.dataset.asset;
          const asset = suffix ? rel.assets.find((x) => x.name.endsWith(suffix)) : undefined;
          if (!asset) return;
          a.href = asset.browser_download_url;
          const cell = a.closest('[data-download]') ?? a;
          cell.querySelector<HTMLElement>('[data-size]')?.replaceChildren(` · ${megabytes(asset.size)}`);
        });
      } catch {
        // offline or rate-limited: static hrefs (the releases page) remain
      }
    })();
  }, []);

  return null;
}
