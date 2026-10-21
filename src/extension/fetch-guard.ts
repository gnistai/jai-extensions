// Rules for the background's privileged .jai fetch (spec §Permitted inputs).
// The background can read what the page cannot (host permissions bypass CORS),
// so it must never become a way for a page to probe other servers: only .jai
// paths, only http(s), no redirects, no loopback or private-network targets from
// public pages, no loopback targets from other private pages (with JAI Studio
// connected, the extension may reach every loopback port), and a per-page rate limit.
import { jaiResourceURL } from '../jai/index.ts';

/** The IPv4 address inside an IPv4-mapped IPv6 address, in either form (URL parsers write ::ffff:7f00:1). */
function mappedV4(h: string): string | null {
  const dotted = /^::ffff:(\d+\.\d+\.\d+\.\d+)$/.exec(h);
  if (dotted) return dotted[1]!;
  const hex = /^::ffff:([0-9a-f]{1,4}):([0-9a-f]{1,4})$/.exec(h);
  if (!hex) return null;
  const [hi, lo] = [parseInt(hex[1]!, 16), parseInt(hex[2]!, 16)];
  return `${hi >> 8}.${hi & 255}.${lo >> 8}.${lo & 255}`;
}

const bare = (hostname: string): string => hostname.toLowerCase().replace(/^\[|\]$/g, '').replace(/\.$/, '');

/** Loopback, link-local, private-network and single-label (intranet) host names. */
export function isPrivateHost(hostname: string): boolean {
  const h = bare(hostname);
  if (h === 'localhost' || h.endsWith('.localhost') || h.endsWith('.local') || h.endsWith('.internal') || h.endsWith('.lan') || h.endsWith('.home.arpa')) return true;
  const v4 = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(h);
  if (v4) {
    const [a, b] = [Number(v4[1]), Number(v4[2])];
    return a === 0 || a === 10 || a === 127 || (a === 100 && b >= 64 && b <= 127) || (a === 169 && b === 254) || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168);
  }
  if (h.includes(':')) {
    if (h === '::' || h === '::1') return true;
    if (/^f[cd][0-9a-f]{2}:/.test(h) || /^fe[89ab][0-9a-f]:/.test(h)) return true; // unique local, link-local
    const mapped = mappedV4(h);
    return mapped ? isPrivateHost(mapped) : false;
  }
  return !h.includes('.'); // single-label names resolve on the local network
}

/** This computer: localhost names, loopback addresses, and the unspecified addresses (0.0.0.0/8, ::), which reach it too. */
export function isLoopbackHost(hostname: string): boolean {
  const h = bare(hostname);
  const v4 = mappedV4(h) ?? h;
  return h === 'localhost' || h.endsWith('.localhost') || h === '::1' || h === '::' || /^(127|0)\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(v4);
}

/**
 * Whether the background may fetch `url` for a page at `pageOrigin`: an http(s)
 * .jai path; no local or private address for a public page; and loopback only
 * for a page that is itself on loopback.
 */
export function privilegedFetchAllowed(url: string, pageOrigin: string | null): boolean {
  const abs = jaiResourceURL(url);
  if (!abs) return false;
  let page: string | null = null;
  try { page = pageOrigin ? new URL(pageOrigin).hostname : null; } catch { /* treat as public */ }
  const target = new URL(abs).hostname;
  if (isLoopbackHost(target)) return page !== null && isLoopbackHost(page);
  return (page !== null && isPrivateHost(page)) || !isPrivateHost(target);
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
