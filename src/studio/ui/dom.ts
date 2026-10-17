// Small DOM and formatting helpers for the JAI Studio UI. Text always goes in
// through text nodes, never innerHTML. Styles are set through classes or the
// CSSOM, never style attributes (the app's CSP has no 'unsafe-inline').

/** The element with this id; index.html must have it. */
export function $<T extends HTMLElement = HTMLElement>(id: string): T {
  const e = document.getElementById(id);
  if (!e) throw new Error(`index.html has no #${id}`);
  return e as T;
}

type Kid = Node | string | number | null | undefined | false;

function append(parent: Element, kids: Kid[]): void {
  for (const k of kids) if (k !== null && k !== undefined && k !== false) parent.append(typeof k === 'number' ? String(k) : k);
}

/** Creates an element with properties and children; strings become text nodes. */
export function el<K extends keyof HTMLElementTagNameMap>(tag: K, props: Partial<HTMLElementTagNameMap[K]> = {}, ...kids: Kid[]): HTMLElementTagNameMap[K] {
  const e = Object.assign(document.createElement(tag), props);
  append(e, kids);
  return e;
}

/** Creates an SVG element with attributes and children. */
export function svg<K extends keyof SVGElementTagNameMap>(tag: K, attrs: Record<string, string | number> = {}, ...kids: Kid[]): SVGElementTagNameMap[K] {
  const e = document.createElementNS('http://www.w3.org/2000/svg', tag);
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, String(v));
  append(e, kids);
  return e;
}

/** Decimal units, as disks and download sizes are sold: 30.3 GB. */
export function fmtBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return '–';
  if (n >= 1e12) return `${(n / 1e12).toFixed(1)} TB`;
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)} GB`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(n >= 1e8 ? 0 : 1)} MB`;
  if (n >= 1e3) return `${Math.round(n / 1e3)} kB`;
  return `${Math.round(n)} bytes`;
}

export const fmtSpeed = (bps: number): string => `${fmtBytes(bps)}/s`;

/** A rough duration: 45 s, 4 min, 1 h 5 min. */
export function fmtDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return '';
  if (seconds < 60) return `${Math.max(1, Math.round(seconds))} s`;
  if (seconds < 3600) return `${Math.round(seconds / 60)} min`;
  const h = Math.floor(seconds / 3600), m = Math.round((seconds % 3600) / 60);
  return m ? `${h} h ${m} min` : `${h} h`;
}

/** Elapsed time as m:ss. */
export function fmtClock(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

export const fmtFitness = (f: number): string => f.toFixed(3);

export function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** The last component of a Windows or Unix path. */
export function baseName(path: string): string {
  return path.split(/[\\/]/).pop() || path;
}

/** A file name without its extension. */
export function stem(name: string): string {
  return name.replace(/\.[^.\\/]+$/, '') || name;
}

export function randomSeed(): number {
  return crypto.getRandomValues(new Uint32Array(1))[0] ?? 0;
}

export function bytesToBase64(bytes: Uint8Array): string {
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
}

/** Copies text, with a fallback for webviews without the async clipboard. */
export async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    return;
  } catch { /* fall back below */ }
  const area = el('textarea', { value: text, readOnly: true, className: 'sr-only' });
  document.body.append(area);
  area.select();
  const ok = document.execCommand('copy');
  area.remove();
  if (!ok) throw new Error('The clipboard is not available.');
}

let toastTimer = 0;
/** A short message at the bottom of the window, announced to screen readers. */
export function toast(message: string, kind: 'ok' | 'err' = 'ok'): void {
  const t = $('toast');
  t.textContent = message;
  t.dataset.kind = kind;
  t.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => t.classList.remove('show'), kind === 'err' ? 7000 : 2600);
}

/** Sets a status line: role="status" elements announce the change. */
export function say(target: HTMLElement, message: string, kind: '' | 'ok' | 'err' | 'warn' = ''): void {
  target.textContent = message;
  target.className = target.className.replace(/\b(ok|err|warn)\b/g, '').trim();
  if (kind) target.classList.add(kind);
}
