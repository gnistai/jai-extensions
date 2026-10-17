// node:url for dist/studio/conformance.mjs inside QuickJS: file URLs only.
import { resolve } from './path.ts';

export function fileURLToPath(url: string | { href: string }): string {
  const href = typeof url === 'string' ? url : url.href;
  if (!href.startsWith('file:')) throw new TypeError(`The URL must be of scheme file: ${href}`);
  let rest = href.slice('file:'.length).replace(/[?#].*$/s, '');
  if (rest.startsWith('//')) {
    const slash = rest.indexOf('/', 2);
    rest = slash < 0 ? '/' : rest.slice(slash);
  }
  const path = decodeURIComponent(rest);
  return /^\/[A-Za-z]:\//.test(path) ? path.slice(1) : path;
}

export function pathToFileURL(path: string): { href: string; toString(): string } {
  const abs = resolve(path);
  const href = 'file://' + encodeURI(abs.startsWith('/') ? abs : `/${abs}`).replace(/\?/g, '%3F').replace(/#/g, '%23');
  return { href, toString: () => href };
}
