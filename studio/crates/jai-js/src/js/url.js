// URL and URLSearchParams for JAI Studio's QuickJS host (studio/DESIGN.md
// section 8). Parsing, serialisation and the setters are the Rust url crate's
// WHATWG implementation (url::quirks, form_urlencoded); this file keeps the
// component cache and the live link between a URL and its searchParams.
(function (native, internal) {
  'use strict';
  const { define, usv } = internal;

  // Component order of native.urlParse and native.urlSet.
  const HREF = 0, ORIGIN = 1, PROTOCOL = 2, USERNAME = 3, PASSWORD = 4, HOST = 5, HOSTNAME = 6, PORT = 7, PATHNAME = 8, SEARCH = 9, HASH = 10;

  /** [k1, v1, k2, v2, ...] -> [[k1, v1], [k2, v2], ...] */
  const pairs = flat => {
    const out = [];
    for (let i = 0; i < flat.length; i += 2) out.push([flat[i], flat[i + 1]]);
    return out;
  };

  const invalidURL = input => {
    const error = new TypeError('Invalid URL');
    error.code = 'ERR_INVALID_URL';
    error.input = input;
    return error;
  };

  const parse = (url, base) => {
    const input = usv(url);
    return base === undefined ? native.urlParse(input) : native.urlParse(input, usv(base));
  };

  let linkedParams, replaceParams, queryChanged;

  class URLSearchParams {
    #list = [];
    /** The URL whose query this object mirrors, or null. */
    #url = null;

    constructor(init = '') {
      if (init instanceof URLSearchParams) {
        this.#list = init.#list.map(([k, v]) => [k, v]);
      } else if ((typeof init === 'object' && init !== null) || typeof init === 'function') {
        const iterator = init[Symbol.iterator];
        if (iterator !== undefined && iterator !== null) {
          if (typeof iterator !== 'function') throw new TypeError("Failed to construct 'URLSearchParams': The object must have a callable @@iterator property.");
          for (const pair of init) {
            if ((typeof pair !== 'object' && typeof pair !== 'function') || pair === null) {
              throw new TypeError("Failed to construct 'URLSearchParams': The provided value cannot be converted to a sequence.");
            }
            const p = Array.from(pair);
            if (p.length !== 2) throw new TypeError("Failed to construct 'URLSearchParams': Sequence initializer must only contain pair elements");
            this.#list.push([usv(p[0]), usv(p[1])]);
          }
        } else {
          for (const key of Reflect.ownKeys(init)) {
            if (typeof key !== 'string') continue;
            const desc = Reflect.getOwnPropertyDescriptor(init, key);
            if (desc?.enumerable) this.#list.push([usv(key), usv(init[key])]);
          }
        }
      } else {
        let query = usv(init);
        if (query.startsWith('?')) query = query.slice(1);
        this.#list = pairs(native.formParse(query));
      }
    }

    get size() { return this.#list.length; }

    append(name, value) {
      this.#list.push([usv(name), usv(value)]);
      this.#update();
    }

    delete(name, value = undefined) {
      const n = usv(name);
      const v = value === undefined ? undefined : usv(value);
      this.#list = this.#list.filter(([k, x]) => !(k === n && (v === undefined || x === v)));
      this.#update();
    }

    get(name) {
      const n = usv(name);
      const pair = this.#list.find(([k]) => k === n);
      return pair ? pair[1] : null;
    }

    getAll(name) {
      const n = usv(name);
      return this.#list.filter(([k]) => k === n).map(([, v]) => v);
    }

    has(name, value = undefined) {
      const n = usv(name);
      const v = value === undefined ? undefined : usv(value);
      return this.#list.some(([k, x]) => k === n && (v === undefined || x === v));
    }

    set(name, value) {
      const n = usv(name);
      const v = usv(value);
      const i = this.#list.findIndex(([k]) => k === n);
      if (i < 0) this.#list.push([n, v]);
      else this.#list = this.#list.filter(([k], j) => j <= i || k !== n).map((p, j) => (j === i ? [n, v] : p));
      this.#update();
    }

    sort() {
      // Code-unit order, stable: what the URL Standard asks for.
      this.#list.sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
      this.#update();
    }

    forEach(callback, thisArg = undefined) {
      if (typeof callback !== 'function') throw new TypeError("Failed to execute 'forEach' on 'URLSearchParams': The callback provided as parameter 1 is not a function.");
      for (let i = 0; i < this.#list.length; i++) {
        const [k, v] = this.#list[i];
        callback.call(thisArg, v, k, this);
      }
    }

    *keys() { for (let i = 0; i < this.#list.length; i++) yield this.#list[i][0]; }
    *values() { for (let i = 0; i < this.#list.length; i++) yield this.#list[i][1]; }
    *entries() { for (let i = 0; i < this.#list.length; i++) yield [this.#list[i][0], this.#list[i][1]]; }

    toString() { return native.formSerialize(this.#list.flat()); }

    get [Symbol.toStringTag]() { return 'URLSearchParams'; }

    #update() {
      if (this.#url) queryChanged(this.#url, this.toString());
    }

    static {
      linkedParams = (url, query) => {
        const params = new URLSearchParams(query);
        params.#url = url;
        return params;
      };
      replaceParams = (params, query) => { params.#list = pairs(native.formParse(query)); };
    }
  }
  Object.defineProperty(URLSearchParams.prototype, Symbol.iterator, { value: URLSearchParams.prototype.entries, writable: true, configurable: true });

  class URL {
    #c;
    #params = null;

    constructor(url, base = undefined) {
      if (arguments.length === 0) throw new TypeError("Failed to construct 'URL': 1 argument required, but only 0 present.");
      const c = parse(url, base);
      if (c == null) throw invalidURL(usv(url));
      this.#c = c;
    }

    static canParse(url, base = undefined) { return parse(url, base) != null; }

    static parse(url, base = undefined) {
      try { return new URL(url, base); } catch { return null; }
    }

    get href() { return this.#c[HREF]; }
    set href(value) {
      const c = parse(value);
      if (c == null) throw invalidURL(usv(value));
      this.#c = c;
      this.#syncParams();
    }

    get origin() { return this.#c[ORIGIN]; }
    get protocol() { return this.#c[PROTOCOL]; }
    set protocol(value) { this.#set(PROTOCOL, value); }
    get username() { return this.#c[USERNAME]; }
    set username(value) { this.#set(USERNAME, value); }
    get password() { return this.#c[PASSWORD]; }
    set password(value) { this.#set(PASSWORD, value); }
    get host() { return this.#c[HOST]; }
    set host(value) { this.#set(HOST, value); }
    get hostname() { return this.#c[HOSTNAME]; }
    set hostname(value) { this.#set(HOSTNAME, value); }
    get port() { return this.#c[PORT]; }
    set port(value) { this.#set(PORT, value); }
    get pathname() { return this.#c[PATHNAME]; }
    set pathname(value) { this.#set(PATHNAME, value); }
    get search() { return this.#c[SEARCH]; }
    set search(value) { this.#set(SEARCH, value); this.#syncParams(); }
    get searchParams() { return (this.#params ??= linkedParams(this, this.#c[SEARCH].slice(1))); }
    get hash() { return this.#c[HASH]; }
    set hash(value) { this.#set(HASH, value); }

    toString() { return this.#c[HREF]; }
    toJSON() { return this.#c[HREF]; }

    get [Symbol.toStringTag]() { return 'URL'; }

    #set(index, value) {
      const c = native.urlSet(this.#c[HREF], index, usv(value));
      if (c != null) this.#c = c;
    }

    #syncParams() {
      if (this.#params) replaceParams(this.#params, this.#c[SEARCH].slice(1));
    }

    static {
      queryChanged = (url, query) => url.#set(SEARCH, query);
    }
  }
  for (const name of ['href', 'origin', 'protocol', 'username', 'password', 'host', 'hostname', 'port', 'pathname', 'search', 'searchParams', 'hash']) {
    Object.defineProperty(URL.prototype, name, { ...Object.getOwnPropertyDescriptor(URL.prototype, name), enumerable: true });
  }

  define('URL', URL);
  define('URLSearchParams', URLSearchParams);
})
