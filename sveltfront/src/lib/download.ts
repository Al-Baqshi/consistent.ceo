// Desktop download metadata and OS detection for the /download page.
//
// The installers are produced by `.github/workflows/release-desktop.yml` on a
// version tag, under *stable* names with no version in the filename, and
// attached to that tag's GitHub release. `releases/latest/download/<name>`
// therefore always points at the newest published build.
//
// Overridable with `VITE_DOWNLOAD_BASE` for a fork or a self-hoster's mirror.

const REPO = 'Al-Baqshi/consistent.ceo';

const BASE = (
  import.meta.env.VITE_DOWNLOAD_BASE || `https://github.com/${REPO}/releases/latest/download`
).replace(/\/$/, '');

/** Every build we ship, with release notes. */
export const RELEASES_URL = `https://github.com/${REPO}/releases`;

/** The source, for platforms without a prebuilt installer. */
export const SOURCE_URL = `https://github.com/${REPO}#from-source`;

export type PlatformId = 'macos-arm' | 'macos-intel' | 'windows' | 'linux';

export type Build = {
  id: PlatformId;
  /** Human label for the button/card. */
  label: string;
  /** Longer descriptor, e.g. the chip architecture. */
  detail: string;
  /** Direct installer URL, or null when we don't ship a prebuilt binary. */
  url: string | null;
  /** File kind shown on the button. */
  kind: string;
};

export const BUILDS: Record<PlatformId, Build> = {
  'macos-arm': {
    id: 'macos-arm',
    label: 'Download for Mac',
    detail: 'Apple Silicon (M1–M4)',
    url: `${BASE}/Consistent-CEO-macOS-AppleSilicon.dmg`,
    kind: '.dmg'
  },
  'macos-intel': {
    id: 'macos-intel',
    label: 'Download for Mac',
    detail: 'Intel',
    url: `${BASE}/Consistent-CEO-macOS-Intel.dmg`,
    kind: '.dmg'
  },
  windows: {
    id: 'windows',
    label: 'Download for Windows',
    detail: 'Windows 10 & 11 · 64-bit',
    url: `${BASE}/Consistent-CEO-Windows-Setup.exe`,
    kind: '.exe'
  },
  linux: {
    id: 'linux',
    label: 'Build for Linux',
    detail: 'Run from source',
    // No prebuilt Linux desktop bundle yet; point at the source instructions.
    url: null,
    kind: 'source'
  }
};

export type DetectedOs = {
  /** The best-guess build for this visitor. */
  primary: PlatformId;
  /** A short label like "macOS" for the hero copy. */
  osLabel: string;
};

/**
 * Which chip a Mac visitor is on, or `null` when the browser won't say.
 *
 * The user-agent is useless here — it reports "Intel Mac OS X" on every Mac,
 * M-series included — which is why this page used to tell Intel owners they
 * were on Apple Silicon and hand them a .dmg that cannot run. Two signals do
 * work, in order of reliability:
 *
 * 1. `userAgentData.getHighEntropyValues(['architecture'])` — Chromium only,
 *    but it is authoritative there: "arm" or "x86".
 * 2. The WebGL renderer string — Safari and Firefox report "Apple M1/M2/…" on
 *    Apple Silicon and "Intel Iris…"/"AMD Radeon…" on Intel Macs.
 *
 * Returns `null` rather than guessing when neither answers, so the caller can
 * keep its own default instead of acting on a coin flip.
 */
export async function detectMacArch(): Promise<'macos-arm' | 'macos-intel' | null> {
  if (typeof navigator === 'undefined') return null;

  const uaData = (
    navigator as Navigator & {
      userAgentData?: {
        getHighEntropyValues?: (h: string[]) => Promise<{ architecture?: string }>;
      };
    }
  ).userAgentData;
  try {
    const hints = await uaData?.getHighEntropyValues?.(['architecture']);
    if (hints?.architecture === 'arm') return 'macos-arm';
    if (hints?.architecture === 'x86') return 'macos-intel';
  } catch (_) {
    // Rejected or unsupported; fall through to the WebGL probe.
  }

  try {
    const canvas = document.createElement('canvas');
    const gl = canvas.getContext('webgl') as WebGLRenderingContext | null;
    const ext = gl?.getExtension('WEBGL_debug_renderer_info');
    const renderer = String(
      ext ? gl?.getParameter(ext.UNMASKED_RENDERER_WEBGL) : gl?.getParameter(gl.RENDERER)
    );
    if (/apple\s*m\d/i.test(renderer)) return 'macos-arm';
    if (/intel|radeon|amd/i.test(renderer)) return 'macos-intel';
  } catch (_) {
    // No WebGL (headless, blocked, or a locked-down browser).
  }

  return null;
}

/**
 * Best-effort guess of the visitor's platform, to feature the right button.
 *
 * On a Mac this returns Apple Silicon as the provisional answer — every Mac
 * sold since 2020 — and the caller refines it with [`detectMacArch`], which is
 * async and so cannot run here. The page always lists every platform, so a
 * wrong guess costs one click.
 */
export function detectOs(): DetectedOs {
  if (typeof navigator === 'undefined') {
    return { primary: 'macos-arm', osLabel: 'your device' };
  }

  const ua = navigator.userAgent;
  // navigator.platform is deprecated but still the most reliable OS signal.
  const platform = (navigator.platform || '').toLowerCase();

  if (/win/.test(platform) || /Windows/.test(ua)) {
    return { primary: 'windows', osLabel: 'Windows' };
  }
  if (/mac/.test(platform) || /Mac OS X/.test(ua)) {
    return { primary: 'macos-arm', osLabel: 'macOS' };
  }
  if (/linux/.test(platform) || /Linux/.test(ua)) {
    return { primary: 'linux', osLabel: 'Linux' };
  }
  return { primary: 'macos-arm', osLabel: 'your device' };
}
