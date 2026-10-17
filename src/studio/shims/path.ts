// node:path for dist/studio/conformance.mjs inside QuickJS. POSIX-style paths
// with forward slashes; backslashes and Windows drive letters are accepted.
// Covers what the spec's tests/run.mjs uses.

const ROOT = /^(?:[A-Za-z]:)?\//;

function split(p: string): { root: string; segments: string[] } {
  const s = p.replace(/\\/g, '/');
  const root = ROOT.exec(s)?.[0] ?? '';
  return { root, segments: s.slice(root.length).split('/') };
}

export function isAbsolute(p: string): boolean {
  return ROOT.test(p.replace(/\\/g, '/'));
}

export function normalize(p: string): string {
  const { root, segments } = split(p);
  const out: string[] = [];
  for (const seg of segments) {
    if (seg === '' || seg === '.') continue;
    if (seg === '..') {
      if (out.length > 0 && out[out.length - 1] !== '..') out.pop();
      else if (!root) out.push('..');
      continue;
    }
    out.push(seg);
  }
  return root + out.join('/') || '.';
}

export function join(...parts: string[]): string {
  return normalize(parts.filter(p => p !== '').join('/'));
}

export function resolve(...parts: string[]): string {
  let path = '';
  for (let i = parts.length - 1; i >= 0 && !isAbsolute(path); i--) {
    const part = parts[i] ?? '';
    if (part) path = path ? `${part}/${path}` : part;
  }
  if (!isAbsolute(path)) path = `${globalThis.__host.cwd()}/${path}`;
  return normalize(path);
}

export function dirname(p: string): string {
  const { root, segments } = split(p);
  const rest = segments.filter(s => s !== '');
  rest.pop();
  return root + rest.join('/') || '.';
}

export function basename(p: string): string {
  return split(p).segments.filter(s => s !== '').pop() ?? '';
}
