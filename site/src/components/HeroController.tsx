import { useEffect } from 'react';

// React controller for the hero page (/). The page is the prototype's own
// Billboard markup rendered statically by Astro; this island renders nothing
// and resolves version + dmg links from the latest GitHub release, exactly
// like the prototype's script. The download button gets the dmg matching the
// visitor's chip, defaulting to arm64 when detection fails.

type Arch = 'arm64' | 'x64';

type NavigatorUAData = Navigator & {
  userAgentData?: {
    getHighEntropyValues(hints: string[]): Promise<{ architecture?: string }>;
  };
};

// Chromium reports the real architecture via UA hints; Apple Silicon Safari
// does not (both chip generations claim "Intel Mac OS X"), so fall back to
// the WebGL renderer: "Apple M…" / "Apple GPU" on silicon, "Intel"/"AMD"/
// "NVIDIA" on Intel-era Macs.
async function detectArch(): Promise<Arch> {
  try {
    const uad = (navigator as NavigatorUAData).userAgentData;
    if (uad?.getHighEntropyValues) {
      const { architecture } = await uad.getHighEntropyValues(['architecture']);
      if (architecture === 'arm') return 'arm64';
      if (architecture === 'x86') return 'x64';
    }
  } catch {
    // fall through to WebGL
  }
  try {
    const gl = document.createElement('canvas').getContext('webgl');
    const ext = gl?.getExtension('WEBGL_debug_renderer_info');
    if (gl && ext) {
      const renderer = String(gl.getParameter(ext.UNMASKED_RENDERER_WEBGL));
      if (/apple m\d/i.test(renderer) || /^apple gpu/i.test(renderer)) return 'arm64';
      if (/intel|amd|nvidia/i.test(renderer)) return 'x64';
    }
  } catch {
    // fall through to the default
  }
  return 'arm64';
}

export default function HeroController() {
  useEffect(() => {
    (async () => {
      const arch = await detectArch();
      try {
        const r = await fetch('https://api.github.com/repos/Onion-L/holt/releases/latest');
        if (!r.ok) return;
        const rel: { tag_name: string; assets: { name: string; browser_download_url: string }[] } =
          await r.json();
        document
          .querySelectorAll('[data-version]')
          .forEach((el) => (el.textContent = rel.tag_name.replace(/^v/, '')));
        // One download button: retarget it to the detected chip, then point
        // every data-arch anchor at its matching dmg.
        document.querySelector<HTMLAnchorElement>('a[data-arch]')?.setAttribute('data-arch', arch);
        document.querySelectorAll('a[data-arch]').forEach((a) => {
          const el = a as HTMLAnchorElement;
          const dmg = rel.assets.find((x) => x.name.endsWith(`-${el.dataset.arch}.dmg`));
          if (dmg) el.href = dmg.browser_download_url;
        });
      } catch {
        // offline or rate-limited: static hrefs (releases page) remain
      }
    })();
  }, []);

  return null;
}
