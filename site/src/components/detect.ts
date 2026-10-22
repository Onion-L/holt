// Visitor platform + chip detection shared by the hero and download
// controllers.

export type Arch = 'arm64' | 'x64';
export type Platform = 'macos' | 'windows' | 'linux';

type NavigatorUAData = Navigator & {
  userAgentData?: {
    platform?: string;
    getHighEntropyValues(hints: string[]): Promise<{ architecture?: string }>;
  };
};

// Chromium reports the real architecture via UA hints; Apple Silicon Safari
// does not (both chip generations claim "Intel Mac OS X"), so fall back to
// the WebGL renderer: "Apple M…" / "Apple GPU" on silicon, "Intel"/"AMD"/
// "NVIDIA" on Intel-era Macs.
export async function detectArch(): Promise<Arch> {
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

// UA hints first; the UA string alone still says "Macintosh" for iPads, which
// is harmless here — macOS is the fallback either way.
export function detectPlatform(): Platform {
  const uad = (navigator as NavigatorUAData).userAgentData;
  const hint = `${uad?.platform ?? ''} ${navigator.userAgent}`.toLowerCase();
  if (hint.includes('win')) return 'windows';
  if (hint.includes('linux') || hint.includes('android')) return 'linux';
  return 'macos';
}
