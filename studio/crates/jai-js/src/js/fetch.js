// Headers, Response and fetch for JAI Studio's QuickJS host (studio/DESIGN.md
// section 8). native.fetch (fetch.rs) does the HTTP request with reqwest and
// enforces the allow-list: only the local server's origin is reachable.
// Bodies are buffered; there are no streams.
(function (native, internal) {
  'use strict';
  const { define, usv, bytesOf, AbortSignal, TextDecoder } = internal;

  const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

  const headerName = name => {
    const s = String(name);
    if (!TOKEN.test(s)) throw new TypeError(`Invalid header name: "${s}"`);
    return s.toLowerCase();
  };

  const headerValue = value => {
    const s = String(value).replace(/^[\t\n\r ]+|[\t\n\r ]+$/g, '');
    if (/[\0\r\n]|[^\x00-\xff]/.test(s)) throw new TypeError(`Invalid header value: "${s}"`);
    return s;
  };

  let headersList, makeHeaders;

  class Headers {
    /** [lowercase name, value] in insertion order */
    #list = [];
    #immutable = false;

    constructor(init = undefined) {
      if (init === undefined) return;
      if (init instanceof Headers) {
        for (const [n, v] of init.#list) this.#list.push([n, v]);
      } else if ((typeof init === 'object' && init !== null) || typeof init === 'function') {
        const iterator = init[Symbol.iterator];
        if (iterator !== undefined && iterator !== null) {
          for (const pair of init) {
            const p = Array.from(pair);
            if (p.length !== 2) throw new TypeError("Failed to construct 'Headers': Invalid value");
            this.append(p[0], p[1]);
          }
        } else {
          for (const key of Reflect.ownKeys(init)) {
            if (typeof key !== 'string') continue;
            const desc = Reflect.getOwnPropertyDescriptor(init, key);
            if (desc?.enumerable) this.append(key, init[key]);
          }
        }
      } else {
        throw new TypeError("Failed to construct 'Headers': The provided value is not of type '(record<ByteString, ByteString> or sequence<sequence<ByteString>>)'.");
      }
    }

    append(name, value) {
      const n = headerName(name);
      const v = headerValue(value);
      this.#mutable();
      this.#list.push([n, v]);
    }

    delete(name) {
      const n = headerName(name);
      this.#mutable();
      this.#list = this.#list.filter(([k]) => k !== n);
    }

    get(name) {
      const n = headerName(name);
      const values = this.#list.filter(([k]) => k === n).map(([, v]) => v);
      return values.length ? values.join(', ') : null;
    }

    getSetCookie() {
      return this.#list.filter(([k]) => k === 'set-cookie').map(([, v]) => v);
    }

    has(name) {
      const n = headerName(name);
      return this.#list.some(([k]) => k === n);
    }

    set(name, value) {
      const n = headerName(name);
      const v = headerValue(value);
      this.#mutable();
      const i = this.#list.findIndex(([k]) => k === n);
      if (i < 0) this.#list.push([n, v]);
      else this.#list = this.#list.filter(([k], j) => j <= i || k !== n).map((p, j) => (j === i ? [n, v] : p));
    }

    forEach(callback, thisArg = undefined) {
      if (typeof callback !== 'function') throw new TypeError("Failed to execute 'forEach' on 'Headers': The callback provided as parameter 1 is not a function.");
      for (const [n, v] of this.#sorted()) callback.call(thisArg, v, n, this);
    }

    *entries() { yield* this.#sorted(); }
    *keys() { for (const [n] of this.#sorted()) yield n; }
    *values() { for (const [, v] of this.#sorted()) yield v; }

    get [Symbol.toStringTag]() { return 'Headers'; }

    /** The Fetch Standard's "sort and combine": names sorted, values joined, set-cookie kept apart. */
    #sorted() {
      const names = [...new Set(this.#list.map(([n]) => n))].sort();
      const out = [];
      for (const n of names) {
        if (n === 'set-cookie') {
          for (const [k, v] of this.#list) if (k === n) out.push([n, v]);
        } else {
          out.push([n, this.#list.filter(([k]) => k === n).map(([, v]) => v).join(', ')]);
        }
      }
      return out;
    }

    #mutable() {
      if (this.#immutable) throw new TypeError('Headers are immutable');
    }

    static {
      headersList = headers => headers.#list;
      makeHeaders = (list, immutable) => {
        const headers = new Headers();
        headers.#list = list;
        headers.#immutable = immutable;
        return headers;
      };
    }
  }
  Object.defineProperty(Headers.prototype, Symbol.iterator, { value: Headers.prototype.entries, writable: true, configurable: true });

  /** [bytes, default content-type] for a request or response body. */
  const extractBody = body => {
    if (typeof body === 'string') return [native.utf8Encode(usv(body)), 'text/plain;charset=UTF-8'];
    if (body instanceof URLSearchParams) return [native.utf8Encode(body.toString()), 'application/x-www-form-urlencoded;charset=UTF-8'];
    const bytes = bytesOf(body);
    if (bytes) return [bytes.slice(), null];
    return [native.utf8Encode(usv(body)), 'text/plain;charset=UTF-8'];
  };

  const decodeText = bytes => new TextDecoder().decode(bytes);

  const NULL_BODY_STATUS = new Set([101, 103, 204, 205, 304]);
  let makeResponse;

  class Response {
    #type = 'default';
    #url = '';
    #redirected = false;
    #status = 200;
    #statusText = '';
    #headers;
    #body = null;
    #used = false;
    /** The fetch's signal: aborting it after the response arrived fails the body reads. */
    #signal = null;

    constructor(body = null, init = undefined) {
      const opts = init ?? {};
      const status = opts.status === undefined ? 200 : Number(opts.status);
      if (!Number.isInteger(status) || status < 200 || status > 599) {
        throw new RangeError(`Failed to construct 'Response': The status provided (${opts.status}) is outside the range [200, 599].`);
      }
      const statusText = opts.statusText === undefined ? '' : String(opts.statusText);
      if (!/^[\t\x20-\x7e\x80-\xff]*$/.test(statusText)) throw new TypeError("Failed to construct 'Response': Invalid statusText");
      this.#status = status;
      this.#statusText = statusText;
      this.#headers = new Headers(opts.headers);
      if (body !== null && body !== undefined) {
        if (NULL_BODY_STATUS.has(status)) throw new TypeError("Failed to construct 'Response': Response with null body status cannot have body");
        const [bytes, type] = extractBody(body);
        this.#body = bytes;
        if (type && !this.#headers.has('content-type')) this.#headers.append('content-type', type);
      }
    }

    get type() { return this.#type; }
    get url() { return this.#url; }
    get redirected() { return this.#redirected; }
    get status() { return this.#status; }
    get ok() { return this.#status >= 200 && this.#status <= 299; }
    get statusText() { return this.#statusText; }
    get headers() { return this.#headers; }
    get bodyUsed() { return this.#used; }

    async arrayBuffer() {
      const b = this.#consume();
      return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
    }

    async bytes() { return this.#consume().slice(); }
    async text() { return decodeText(this.#consume()); }
    async json() { return JSON.parse(decodeText(this.#consume())); }

    clone() {
      if (this.#used) throw new TypeError("Failed to execute 'clone' on 'Response': Response body is already used");
      const list = headersList(this.#headers).map(([n, v]) => [n, v]);
      return makeResponse(this.#type, this.#status, this.#statusText, list, this.#body, this.#url, this.#redirected, this.#signal);
    }

    get [Symbol.toStringTag]() { return 'Response'; }

    #consume() {
      if (this.#signal?.aborted) throw this.#signal.reason;
      if (this.#used) throw new TypeError('Body is unusable: Body has already been read');
      if (this.#body === null) return new Uint8Array(0);
      this.#used = true;
      return this.#body;
    }

    static error() {
      return makeResponse('error', 0, '', [], null, '', false, null);
    }

    static json(data, init = undefined) {
      const text = JSON.stringify(data);
      if (text === undefined) throw new TypeError("Failed to execute 'json' on 'Response': The data is not JSON serializable");
      const headers = new Headers(init?.headers);
      if (!headers.has('content-type')) headers.set('content-type', 'application/json');
      return new Response(text, { status: init?.status, statusText: init?.statusText, headers });
    }

    static {
      makeResponse = (type, status, statusText, list, body, url, redirected, signal) => {
        const r = new Response();
        r.#type = type;
        r.#status = status;
        r.#statusText = statusText;
        r.#headers = makeHeaders(list, type !== 'default');
        r.#body = body;
        r.#url = url;
        r.#redirected = redirected;
        r.#signal = signal;
        return r;
      };
    }
  }

  // Methods the Fetch Standard normalises to upper case, and the forbidden ones.
  const NORMALIZED = new Set(['DELETE', 'GET', 'HEAD', 'OPTIONS', 'POST', 'PUT']);
  const FORBIDDEN = new Set(['CONNECT', 'TRACE', 'TRACK']);
  let nextFetch = 1;

  /** The request parts native.fetch needs, checked as the Request constructor would. */
  const prepare = (input, init) => {
    const href = input instanceof URL ? input.href
      : typeof input === 'object' && input !== null && typeof input.url === 'string' ? input.url
        : usv(input);
    const url = URL.parse(href);
    if (url === null) throw new TypeError(`Failed to parse URL from ${href}`);
    if (url.username || url.password) throw new TypeError(`Request cannot be constructed from a URL that includes credentials: ${href}`);
    let method = init.method === undefined ? 'GET' : String(init.method);
    if (!TOKEN.test(method)) throw new TypeError(`'${method}' is not a valid HTTP method.`);
    const upper = method.toUpperCase();
    if (FORBIDDEN.has(upper)) throw new TypeError(`'${method}' HTTP method is unsupported.`);
    if (NORMALIZED.has(upper)) method = upper;
    const headers = new Headers(init.headers);
    let body = null;
    if (init.body !== undefined && init.body !== null) {
      if (method === 'GET' || method === 'HEAD') throw new TypeError('Request with GET/HEAD method cannot have body.');
      const [bytes, type] = extractBody(init.body);
      body = bytes;
      if (type && !headers.has('content-type')) headers.set('content-type', type);
    }
    const signal = init.signal ?? null;
    if (signal !== null && !(signal instanceof AbortSignal)) throw new TypeError("Failed to execute 'fetch': member signal is not of type AbortSignal.");
    return { url: url.href, method, headers: headersList(headers).flat(), body, signal };
  };

  function fetch(input, init = undefined) {
    return new Promise((resolve, reject) => {
      let request;
      try {
        request = prepare(input, init ?? {});
      } catch (e) {
        reject(e);
        return;
      }
      const { signal } = request;
      if (signal?.aborted) {
        reject(signal.reason);
        return;
      }
      const id = nextFetch++;
      let settled = false;
      const onAbort = () => {
        if (settled) return;
        settled = true;
        native.fetchAbort(id);
        reject(signal.reason);
      };
      const finish = () => {
        settled = true;
        signal?.removeEventListener('abort', onAbort);
      };
      signal?.addEventListener('abort', onAbort);
      native.fetch(id, request.url, request.method, request.headers, request.body).then(
        r => {
          if (settled) return;
          finish();
          const list = [];
          for (let i = 0; i < r.headers.length; i += 2) list.push([r.headers[i], r.headers[i + 1]]);
          resolve(makeResponse('basic', r.status, r.statusText, list, r.body, r.url, r.redirected, signal));
        },
        e => {
          if (settled) return;
          finish();
          reject(e);
        },
      );
    });
  }

  define('Headers', Headers);
  define('Response', Response);
  define('fetch', fetch);
})
