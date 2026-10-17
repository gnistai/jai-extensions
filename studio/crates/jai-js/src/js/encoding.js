// TextEncoder and TextDecoder (UTF-8 only) for JAI Studio's QuickJS host
// (studio/DESIGN.md section 8). The byte work is native (web.rs); the BOM and
// streaming state live here.
(function (native, internal) {
  'use strict';
  const { define, usv, bytesOf } = internal;

  const UTF8_LABELS = new Set(['unicode-1-1-utf-8', 'unicode11utf8', 'unicode20utf8', 'utf-8', 'utf8', 'x-unicode20utf8']);

  class TextEncoder {
    get encoding() { return 'utf-8'; }

    encode(input = '') {
      return native.utf8Encode(usv(input));
    }

    encodeInto(source, destination) {
      if (!(destination instanceof Uint8Array)) {
        throw new TypeError("Failed to execute 'encodeInto' on 'TextEncoder': parameter 2 is not of type 'Uint8Array'.");
      }
      const text = usv(source);
      let read = 0;
      let written = 0;
      for (const ch of text) {
        const cp = ch.codePointAt(0);
        const n = cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
        if (written + n > destination.length) break;
        if (n === 1) {
          destination[written] = cp;
        } else {
          let shift = 6 * (n - 1);
          destination[written] = (n === 2 ? 0xc0 : n === 3 ? 0xe0 : 0xf0) | (cp >> shift);
          for (let i = 1; i < n; i++) {
            shift -= 6;
            destination[written + i] = 0x80 | ((cp >> shift) & 0x3f);
          }
        }
        read += ch.length;
        written += n;
      }
      return { read, written };
    }

    get [Symbol.toStringTag]() { return 'TextEncoder'; }
  }

  class TextDecoder {
    #fatal;
    #ignoreBOM;
    /** Bytes of an incomplete sequence held back by a streaming decode. */
    #pending = null;
    #bomSeen = false;

    constructor(label = 'utf-8', options = undefined) {
      const name = String(label).replace(/^[\t\n\f\r ]+|[\t\n\f\r ]+$/g, '').toLowerCase();
      if (!UTF8_LABELS.has(name)) throw new RangeError(`Failed to construct 'TextDecoder': The encoding label provided ('${label}') is invalid.`);
      this.#fatal = !!options?.fatal;
      this.#ignoreBOM = !!options?.ignoreBOM;
    }

    get encoding() { return 'utf-8'; }
    get fatal() { return this.#fatal; }
    get ignoreBOM() { return this.#ignoreBOM; }

    decode(input = undefined, options = undefined) {
      let bytes = input === undefined ? new Uint8Array(0) : bytesOf(input);
      if (bytes === null) {
        throw new TypeError("Failed to execute 'decode' on 'TextDecoder': The provided value is not of type '(ArrayBuffer or ArrayBufferView)'.");
      }
      const stream = !!options?.stream;
      if (this.#pending) {
        const joined = new Uint8Array(this.#pending.length + bytes.length);
        joined.set(this.#pending);
        joined.set(bytes, this.#pending.length);
        bytes = joined;
        this.#pending = null;
      }
      let decoded;
      try {
        decoded = native.utf8Decode(bytes, this.#fatal, stream);
      } catch (e) {
        this.#bomSeen = false;
        throw e;
      }
      let [text, keep] = decoded;
      if (keep > 0) this.#pending = bytes.slice(bytes.length - keep);
      if (!this.#ignoreBOM && !this.#bomSeen && text.length > 0) {
        this.#bomSeen = true;
        if (text.charCodeAt(0) === 0xfeff) text = text.slice(1);
      }
      if (!stream) this.#bomSeen = false;
      return text;
    }

    get [Symbol.toStringTag]() { return 'TextDecoder'; }
  }

  define('TextEncoder', TextEncoder);
  define('TextDecoder', TextDecoder);
  Object.assign(internal, { TextDecoder });
})
