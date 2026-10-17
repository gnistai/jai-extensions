// Shared helpers, console and timers for JAI Studio's QuickJS host
// (studio/DESIGN.md section 8). web.rs evaluates each shim file once per
// runtime and calls it with `native` (the Rust primitives) and `internal`
// (helpers shared between the shim files). Neither object is reachable from
// globalThis.
(function (native, internal) {
  'use strict';

  /** Installs a global the way WebIDL does: writable, configurable, not enumerable. */
  const define = (name, value) =>
    Object.defineProperty(globalThis, name, { value, writable: true, configurable: true, enumerable: false });

  /** WebIDL USVString: ToString, then every lone surrogate becomes U+FFFD. */
  const usv = value => String(value).toWellFormed();

  /** The bytes of a BufferSource as a Uint8Array view, or null for anything else. */
  const bytesOf = value => {
    if (value instanceof Uint8Array) return value;
    if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
    if (value instanceof ArrayBuffer) return new Uint8Array(value);
    if (typeof SharedArrayBuffer === 'function' && value instanceof SharedArrayBuffer) return new Uint8Array(value);
    return null;
  };

  if (typeof DOMException !== 'function') {
    // QuickJS-ng provides DOMException; this fallback only keeps the shims usable without it.
    define('DOMException', class DOMException extends Error {
      #name;
      constructor(message = '', name = 'Error') { super(String(message)); this.#name = String(name); }
      get name() { return this.#name; }
      get code() { return 0; }
    });
  }

  // ---------------------------------------------------------------- console

  const MAX_DEPTH = 2;
  const MAX_ITEMS = 100;
  const IDENT = /^[A-Za-z_$][\w$]*$/;
  const quote = s => `'${s.replace(/\\/g, '\\\\').replace(/'/g, "\\'").replace(/\n/g, '\\n')}'`;

  const isError = v => v instanceof Error || v instanceof DOMException;

  /** A short, Node-like rendering of a value for console output. */
  const inspect = (value, depth, seen) => {
    switch (typeof value) {
      case 'string': return depth === 0 ? value : quote(value);
      case 'number': return Object.is(value, -0) ? '-0' : String(value);
      case 'bigint': return `${value}n`;
      case 'symbol': return value.toString();
      case 'undefined': return 'undefined';
      case 'boolean': return String(value);
      case 'function': return `[Function: ${value.name || '(anonymous)'}]`;
      default: break;
    }
    if (value === null) return 'null';
    if (seen.has(value)) return '[Circular]';
    if (isError(value)) {
      const head = value.message ? `${value.name}: ${value.message}` : String(value.name);
      const stack = typeof value.stack === 'string' ? value.stack.trimEnd() : '';
      let text = stack ? `${head}\n${stack}` : head;
      if (value.cause !== undefined && depth < MAX_DEPTH) {
        seen.add(value);
        try { text += `\n  [cause]: ${inspect(value.cause, depth + 1, seen)}`; } finally { seen.delete(value); }
      }
      return text;
    }
    if (depth > MAX_DEPTH) return Array.isArray(value) ? '[Array]' : '[Object]';
    seen.add(value);
    try {
      const item = v => inspect(v, depth + 1, seen);
      const list = (items, total) => {
        const shown = items.slice(0, MAX_ITEMS);
        if (total > MAX_ITEMS) shown.push(`... ${total - MAX_ITEMS} more items`);
        return shown.length ? `[ ${shown.join(', ')} ]` : '[]';
      };
      if (Array.isArray(value)) return list(value.slice(0, MAX_ITEMS + 1).map(item), value.length);
      if (ArrayBuffer.isView(value) && !(value instanceof DataView)) {
        return `${value.constructor.name}(${value.length}) ${list(Array.from(value.subarray(0, MAX_ITEMS), item), value.length)}`;
      }
      if (value instanceof ArrayBuffer) return `ArrayBuffer { byteLength: ${value.byteLength} }`;
      if (value instanceof Map) return `Map(${value.size}) ${list([...value].slice(0, MAX_ITEMS + 1).map(([k, v]) => `${item(k)} => ${item(v)}`), value.size)}`;
      if (value instanceof Set) return `Set(${value.size}) ${list([...value].slice(0, MAX_ITEMS + 1).map(item), value.size)}`;
      if (value instanceof Date) return Number.isNaN(value.getTime()) ? 'Invalid Date' : value.toISOString();
      if (value instanceof RegExp) return String(value);
      if (value instanceof Promise) return 'Promise {}';
      if (typeof URL === 'function' && value instanceof URL) return `URL '${value.href}'`;
      const name = value.constructor?.name;
      const prefix = typeof name === 'string' && name !== 'Object' && name !== '' ? `${name} ` : '';
      const keys = Object.keys(value);
      const body = keys.slice(0, MAX_ITEMS).map(k => {
        let shown;
        try { shown = item(value[k]); } catch { shown = '[Getter threw]'; }
        return `${IDENT.test(k) ? k : quote(k)}: ${shown}`;
      });
      if (keys.length > MAX_ITEMS) body.push(`... ${keys.length - MAX_ITEMS} more items`);
      return body.length ? `${prefix}{ ${body.join(', ')} }` : `${prefix}{}`;
    } finally {
      seen.delete(value);
    }
  };

  /** console's format: printf-style substitutions in a leading string, then the remaining values. */
  const format = args => {
    const parts = [];
    let next = 0;
    if (typeof args[0] === 'string' && args.length > 1 && args[0].includes('%')) {
      next = 1;
      parts.push(args[0].replace(/%([sdifjoOc%])/g, (match, f) => {
        if (f === '%') return '%';
        if (next >= args.length) return match;
        const arg = args[next++];
        switch (f) {
          case 's': return typeof arg === 'object' && arg !== null ? inspect(arg, 1, new Set()) : typeof arg === 'bigint' ? `${arg}n` : String(arg);
          case 'd': case 'i': {
            if (typeof arg === 'bigint') return `${arg}n`;
            if (typeof arg === 'symbol') return 'NaN';
            const n = Number(arg);
            return String(f === 'i' ? Math.trunc(n) : n);
          }
          case 'f': return typeof arg === 'symbol' ? 'NaN' : String(Number(arg));
          case 'j': try { return JSON.stringify(arg); } catch { return '[Circular]'; }
          case 'c': return '';
          default: return inspect(arg, 1, new Set());
        }
      }));
    }
    for (; next < args.length; next++) parts.push(inspect(args[next], 0, new Set()));
    return parts.join(' ');
  };

  let indent = '';
  const write = (level, text) => native.log(level, usv(indent ? text.replace(/^/gm, indent) : text));
  const writer = level => (...args) => write(level, format(args));
  const counts = new Map();
  const started = new Map();
  const elapsed = label => {
    const t = started.get(label);
    return t === undefined ? null : `${label}: ${(performance.now() - t).toFixed(3)}ms`;
  };

  const console = {
    log: writer('log'),
    info: writer('info'),
    warn: writer('warn'),
    error: writer('error'),
    debug: writer('debug'),
    trace: (...args) => write('trace', `Trace${args.length ? `: ${format(args)}` : ''}\n${(new Error().stack ?? '').trimEnd()}`),
    assert: (condition, ...args) => { if (!condition) write('error', `Assertion failed${args.length ? `: ${format(args)}` : ''}`); },
    dir: value => write('log', inspect(value, 1, new Set())),
    table: writer('log'),
    count: (label = 'default') => {
      const key = String(label);
      const n = (counts.get(key) ?? 0) + 1;
      counts.set(key, n);
      write('info', `${key}: ${n}`);
    },
    countReset: (label = 'default') => { counts.delete(String(label)); },
    time: (label = 'default') => { started.set(String(label), performance.now()); },
    timeLog: (label = 'default', ...args) => {
      const line = elapsed(String(label));
      write(line === null ? 'warn' : 'info', line === null ? `No such label '${label}'` : [line, ...args.map(a => inspect(a, 0, new Set()))].join(' '));
    },
    timeEnd: (label = 'default') => {
      const line = elapsed(String(label));
      started.delete(String(label));
      write(line === null ? 'warn' : 'info', line ?? `No such label '${label}'`);
    },
    group: (...args) => { if (args.length) write('log', format(args)); indent += '  '; },
    groupCollapsed: (...args) => { if (args.length) write('log', format(args)); indent += '  '; },
    groupEnd: () => { indent = indent.slice(2); },
  };
  Object.defineProperty(console, Symbol.toStringTag, { value: 'console', configurable: true });
  define('console', console);

  /** Reports an exception nobody caught (timer callbacks, event listeners). */
  const report = error => write('error', `Uncaught ${inspect(error, 0, new Set())}`);
  define('reportError', function reportError(error) { report(error); });

  // ---------------------------------------------------------------- timers

  // Each timer is one cancellable tokio sleep (native.timerStart); the callback
  // and its arguments stay here, keyed by id.
  const MAX_DELAY = 2147483647;
  const timers = new Map();
  let nextTimer = 1;

  const delayOf = (ms, repeat) => {
    const n = Number(ms);
    const d = Number.isFinite(n) && n > 0 ? Math.min(n, MAX_DELAY) : 0;
    return repeat ? Math.max(d, 1) : d;
  };

  const arm = (id, timer) => {
    native.timerStart(id, timer.ms).then(fired => {
      if (!fired || timers.get(id) !== timer) return;
      if (!timer.repeat) timers.delete(id);
      try { timer.callback.apply(globalThis, timer.args); } catch (e) { report(e); }
      if (timer.repeat && timers.get(id) === timer) arm(id, timer);
    }, report);
  };

  const start = (callback, ms, args, repeat) => {
    if (typeof callback !== 'function') throw new TypeError('The "callback" argument must be a function');
    const id = nextTimer++;
    const timer = { callback, args, ms: delayOf(ms, repeat), repeat };
    timers.set(id, timer);
    arm(id, timer);
    return id;
  };

  const clear = id => {
    const n = Number(id);
    if (timers.delete(n)) native.timerCancel(n);
  };

  const setTimeout = function setTimeout(callback, ms, ...args) { return start(callback, ms, args, false); };
  define('setTimeout', setTimeout);
  define('setInterval', function setInterval(callback, ms, ...args) { return start(callback, ms, args, true); });
  define('clearTimeout', function clearTimeout(id) { clear(id); });
  define('clearInterval', function clearInterval(id) { clear(id); });

  Object.assign(internal, { define, usv, bytesOf, inspect, report, setTimeout });
})
