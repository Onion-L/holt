// Visitor platform + chip detection for the download controller.

export type Arch = 'arm64' | 'x64';
export type Platform = 'macos' | 'windows' | 'linux';

type NavigatorUAData = Navigator & {
  userAgentData?: {
    platform?: string;
    mobile?: boolean;
    getHighEntropyValues(hints: string[]): Promise<{ architecture?: string }>;
  };
};

// Chromium reports the real architecture via UA hints (all three desktop
// platforms); Safari does not (both Mac chip generations claim "Intel Mac OS
// X"), so fall back to the WebGL renderer: "Apple M…" / "Apple GPU" on
// silicon, "Intel"/"AMD"/"NVIDIA" elsewhere. `null` when neither signal
// answers — the page then leaves its static default instead of guessing a
// binary that cannot run on the machine.
export async function detectArch(): Promise<Arch | null> {
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
  return null;
}

// UA hints first; the UA string alone still says "Macintosh" for iPads, which
// is why `isMobile` exists — a phone gets no "your system" card at all rather
// than being told to download a desktop binary it cannot run.
export function detectPlatform(): Platform {
  const uad = (navigator as NavigatorUAData).userAgentData;
  const hint = `${uad?.platform ?? ''} ${navigator.userAgent}`.toLowerCase();
  if (hint.includes('win')) return 'windows';
  if (hint.includes('linux') || hint.includes('android')) return 'linux';
  return 'macos';
}

export function isMobile(): boolean {
  const uad = (navigator as NavigatorUAData).userAgentData;
  if (typeof uad?.mobile === 'boolean') return uad.mobile;
  return /iphone|ipad|ipod|android|mobile/i.test(navigator.userAgent);
}
