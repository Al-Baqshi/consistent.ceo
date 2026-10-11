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
 * How a computer address will be reached, for the gentle "is this safe beyond
 * your Wi-Fi?" note. Plain `http://` to a home-network address is fine at home
 * but sends the sign-in token and vault traffic unencrypted, so anything other
 * than the phone itself, Tailscale (100.64.0.0/10, `*.ts.net`) or https gets a
 * nudge towards Tailscale or the relay.
 */
export function plainHttpNote(url: string): string | null {
  const m = /^http:\/\/([^/:?#]+|\[[^\]]+\])/i.exec(url.trim());
  if (!m) return null; // https, or not a URL at all
  const host = m[1].toLowerCase();
  if (host === 'localhost' || host === '[::1]' || host.startsWith('127.')) return null;
  if (host.endsWith('.ts.net')) return null;
  const v4 = /^(\d{1,3})\.(\d{1,3})\.\d{1,3}\.\d{1,3}$/.exec(host);
  if (v4 && Number(v4[1]) === 100 && Number(v4[2]) >= 64 && Number(v4[2]) <= 127) return null;
  return 'This address isn’t encrypted. That’s fine on your home Wi-Fi, but to use the app anywhere else, connect through Tailscale or the relay in Consistent CEO on your computer.';
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
