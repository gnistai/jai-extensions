// node:fs for dist/studio/conformance.mjs inside QuickJS, backed by
// globalThis.__host. Synchronous UTF-8 reads only.

export function readFileSync(path: string, options?: string | { encoding?: string | null }): string {
  const encoding = typeof options === 'string' ? options : options?.encoding;
  if (encoding !== 'utf8' && encoding !== 'utf-8') throw new TypeError(`fs shim: only UTF-8 text reads are supported (${String(encoding)})`);
  return globalThis.__host.readFile(String(path));
}

export function readdirSync(path: string): string[] {
  return globalThis.__host.readDir(String(path));
}

export function existsSync(path: string): boolean {
  return globalThis.__host.exists(String(path));
}
