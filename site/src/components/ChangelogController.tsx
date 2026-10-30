import { useEffect } from 'react';

// React controller for the changelog page (/changelog).
// The entry list is rendered statically by Astro; this island renders
// nothing and resolves the current version from the latest GitHub release.

export default function ChangelogController() {
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
