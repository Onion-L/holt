import { useEffect } from 'react';

// React controller for the landing page (/). The page renders static Astro
// markup; this island renders nothing and only resolves the version stamp
// from the latest GitHub release. Download links point to /download, where
// per-platform assets are resolved.

export default function HeroController() {
  useEffect(() => {
    fetch('https://api.github.com/repos/Onion-L/holt/releases/latest')
      .then((r) => (r.ok ? r.json() : Promise.reject()))
      .then((rel: { tag_name: string }) => {
        document
          .querySelectorAll('[data-version]')
          .forEach((el) => (el.textContent = rel.tag_name.replace(/^v/, '')));
      })
      .catch(() => {});
  }, []);

  return null;
}
