import { useEffect } from 'react';

// React controller for the hero page (/). The page is the prototype's own
// Billboard markup rendered statically by Astro; this island renders nothing
// and only resolves the version stamp from the latest GitHub release. The
// download button is a plain link to /download, where the per-chip dmgs are
// resolved.

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
