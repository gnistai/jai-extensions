// Rules for the background's privileged .jai fetch (spec §Permitted inputs).
// The background can read what the page cannot (host permissions bypass CORS),
// so it must never become a way for a page to probe other servers: only .jai
// paths, only http(s), no redirects, no loopback or private-network targets from
// public pages, and a per-page rate limit.
import { jaiResourceURL } from '../jai/index.ts';

/** Loopback, link-local, private-network and single-label (intranet) host names. */
export function isPrivateHost(hostname: string): boolean {
  const h = hostname.toLowerCase().replace(/^\[|\]$/g, '').replace(/\.$/, '');
  if (h === 'localhost' || h.endsWith('.localhost') || h.endsWith('.local') || h.endsWith('.internal') || h.endsWith('.lan') || h.endsWith('.home.arpa')) return true;
  const v4 = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(h);
  if (v4) {
    const [a, b] = [Number(v4[1]), Number(v4[2])];
    return a === 0 || a === 10 || a === 127 || (a === 100 && b >= 64 && b <= 127) || (a === 169 && b === 254) || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168);
  }
  if (h.includes(':')) {
    if (h === '::' || h === '::1') return true;
    if (/^f[cd][0-9a-f]{2}:/.test(h) || /^fe[89ab][0-9a-f]:/.test(h)) return true; // unique local, link-local
    const mapped = /^::ffff:(\d+\.\d+\.\d+\.\d+)$/.exec(h);
    return mapped ? isPrivateHost(mapped[1]!) : false;
  }
  return !h.includes('.'); // single-label names resolve on the local network
}

/** Why the background must refuse to fetch `url` for a page at `pageOrigin`, or null when allowed. */
export function refuseJaiFetch(url: string, pageOrigin: string | null): string | null {
  const abs = jaiResourceURL(url);
  if (!abs) return 'Only http(s) URLs whose path ends in .jai';
  const target = new URL(abs);
  let pagePrivate = false;
  try { pagePrivate = pageOrigin ? isPrivateHost(new URL(pageOrigin).hostname) : false; } catch { /* treat as public */ }
  if (isPrivateHost(target.hostname) && !pagePrivate) return 'A public page may not make the viewer fetch from a local or private address';
  return null;
}

/** At most `max` privileged fetches per key (tab) per window. */
export class RateLimit {
  private hits = new Map<string, number[]>();
  constructor(private readonly max = 20, private readonly windowMs = 60_000) {}
  allow(key: string, now = Date.now()): boolean {
    const recent = (this.hits.get(key) ?? []).filter(t => now - t < this.windowMs);
    if (recent.length >= this.max) { this.hits.set(key, recent); return false; }
    recent.push(now);
    this.hits.set(key, recent);
    if (this.hits.size > 500) this.hits.clear();
    return true;
  }
}
