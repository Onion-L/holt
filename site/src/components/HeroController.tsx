import { useEffect } from 'react';

// React controller for the hero page (/). The page is the prototype's own
// Billboard markup rendered statically by Astro; this island renders nothing
// and resolves version + dmg links from the latest GitHub release, exactly
// like the prototype's script.

export default function HeroController() {
  useEffect(() => {
    fetch('https://api.github.com/repos/Onion-L/holt/releases/latest')
      .then((r) => (r.ok ? r.json() : Promise.reject()))
      .then((rel: { tag_name: string; assets: { name: string; browser_download_url: string }[] }) => {
        document
          .querySelectorAll('[data-version]')
          .forEach((el) => (el.textContent = rel.tag_name.replace(/^v/, '')));
        document.querySelectorAll('a[data-arch]').forEach((a) => {
          const arch = (a as HTMLAnchorElement).dataset.arch;
          const dmg = rel.assets.find((x) => x.name.endsWith(`-${arch}.dmg`));
          if (dmg) (a as HTMLAnchorElement).href = dmg.browser_download_url;
        });
      })
      .catch(() => {});
  }, []);

  return null;
}
