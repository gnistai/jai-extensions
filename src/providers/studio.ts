// Finding JAI Studio's server and checking that it holds the token before the
// token goes to it: GET /health?nonce=<random> answers with
// base64url(HMAC-SHA256(token, "jai-health:<port>:<nonce>")), where <port> is
// the server's own bound port (studio/DESIGN.md section 7). Only the nonce is
// sent to a server that has not proved it. Environment-neutral (fetch and
// WebCrypto), shared by the browser extension and the Node CLI.

/** Studio's port and the nine it moves to when that one is busy (studio/DESIGN.md section 0). */
export const STUDIO_PORTS: readonly number[] = Array.from({ length: 10 }, (_, i) => 5240 + i);

/** A server that proved it holds the token. */
export interface StudioServer { url: string; version: string }

/** Why a probe found no server: nothing answers as JAI Studio there, or the server there could not prove the token. */
export type StudioMiss = 'absent' | 'unproven';

const PROBE_MS = 2000;

const base64url = (bytes: Uint8Array): string => btoa(String.fromCharCode(...bytes)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');

async function healthProof(token: string, port: number, nonce: string): Promise<string> {
  const text = new TextEncoder();
  const key = await crypto.subtle.importKey('raw', text.encode(token), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  return base64url(new Uint8Array(await crypto.subtle.sign('HMAC', key, text.encode(`jai-health:${port}:${nonce}`))));
}

/** Equal strings, compared in a time that depends only on their length. */
function sameText(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let d = 0;
  for (let i = 0; i < a.length; i++) d |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return d === 0;
}

/** Asks the server at `url`, listening on `port`, to prove that it holds `token`. */
export async function proveStudio(url: string, port: number, token: string): Promise<StudioServer | StudioMiss> {
  const nonce = base64url(crypto.getRandomValues(new Uint8Array(32)));
  const ac = new AbortController();
  const timer = setTimeout(() => ac.abort(), PROBE_MS);
  let health: Record<string, unknown> | null = null;
  try {
    const res = await fetch(`${url}/health?nonce=${nonce}`, { redirect: 'error', signal: ac.signal });
    if (res.ok) health = (await res.json()) as Record<string, unknown>;
  } catch {
    health = null;
  } finally {
    clearTimeout(timer);
  }
  if (health?.name !== 'jai') return 'absent';
  if (!sameText(typeof health.proof === 'string' ? health.proof : '', await healthProof(token, port, nonce))) return 'unproven';
  return { url, version: typeof health.version === 'string' ? health.version : '' };
}

/** The JAI Studio on 127.0.0.1 that proves `token`, on the lowest such port; null when none does. All ports are asked at once. */
export async function findStudio(token: string, ports: readonly number[] = STUDIO_PORTS): Promise<StudioServer | null> {
  const found = await Promise.all(ports.map(p => proveStudio(`http://127.0.0.1:${p}`, p, token)));
  return found.find((f): f is StudioServer => typeof f === 'object') ?? null;
}
