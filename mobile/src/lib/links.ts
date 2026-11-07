import { Linking } from 'react-native';

import { log } from '@/lib/logger';

/**
 * The public pages the app links to. The `www` host on purpose: the apex
 * domain answers with a redirect, and these are the URLs App Store Connect has.
 */
export const PRIVACY_URL = 'https://www.consistent.ceo/privacy';
export const SUPPORT_URL = 'https://www.consistent.ceo/support';

/** Open a page in the system browser. Never throws. */
export async function openExternal(url: string): Promise<void> {
  try {
    await Linking.openURL(url);
  } catch (e) {
    log('warn', 'system', `couldn't open ${url}: ${String(e)}`);
  }
}

/**
 * A calm word about how this phone reaches the computer, or null when there is
 * nothing worth saying.
 *
 * The everyday setup is the computer and the phone on the same home Wi-Fi,
 * over plain `http://` to a private address. That is how pairing works out of
 * the box, so it gets a reassuring line, never a nudge to install anything.
 * Only plain `http://` to a public address — which really does cross the open
 * internet unencrypted — gets a gentle note.
 */
export function plainHttpNote(url: string): string | null {
  const m = /^http:\/\/([^/:?#]+|\[[^\]]+\])/i.exec(url.trim());
  if (!m) return null; // https, or not a URL at all
  const host = m[1].toLowerCase();
  if (host === 'localhost' || host === '[::1]' || host.startsWith('127.')) return null;
  if (host.endsWith('.ts.net')) return null;
  const v4 = /^(\d{1,3})\.(\d{1,3})\.\d{1,3}\.\d{1,3}$/.exec(host);
  if (v4 && Number(v4[1]) === 100 && Number(v4[2]) >= 64 && Number(v4[2]) <= 127) return null;
  if (isLocalNetworkUrl(url)) return 'Connected over your home Wi-Fi.';
  return 'This address isn’t encrypted, so it’s best used only on networks you trust.';
}

/** True for addresses on the local network, where iOS asks for Local Network access. */
export function isLocalNetworkUrl(url: string): boolean {
  const m = /^https?:\/\/([^/:?#]+)/i.exec(url.trim());
  if (!m) return false;
  const host = m[1].toLowerCase();
  if (host.endsWith('.local')) return true;
  const v4 = /^(\d{1,3})\.(\d{1,3})\.\d{1,3}\.\d{1,3}$/.exec(host);
  if (!v4) return false;
  const [a, b] = [Number(v4[1]), Number(v4[2])];
  return a === 10 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168) || (a === 169 && b === 254);
}
