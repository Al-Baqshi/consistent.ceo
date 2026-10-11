/**
 * `href` values that came from data — a record's evidence, a scraped doc URL —
 * are not trusted markup. A `javascript:` URL in an `<a href>` runs in this
 * origin, where the session token lives in sessionStorage. Only plain web
 * links are rendered as links; anything else is shown as text.
 *
 * Returns the normalized URL for `http:`/`https:`, or `null`.
 */
export function safeHttpUrl(value: unknown): string | null {
  if (typeof value !== 'string') return null;
  const raw = value.trim();
  if (!raw) return null;
  try {
    const url = new URL(raw);
    return url.protocol === 'http:' || url.protocol === 'https:' ? url.href : null;
  } catch {
    return null;
  }
}
