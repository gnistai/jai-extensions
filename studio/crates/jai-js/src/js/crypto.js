// crypto.getRandomValues, crypto.randomUUID and crypto.subtle.digest for JAI
// Studio's QuickJS host (studio/DESIGN.md section 8). Randomness and hashing
// are native (web.rs).
(function (native, internal) {
  'use strict';
  const { define, bytesOf } = internal;

  const INTEGER_ARRAYS = [Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array, Int32Array, Uint32Array, BigInt64Array, BigUint64Array];
  /** Lowercase WebCrypto digest names -> native names; null marks the ones not supported here. */
  const DIGESTS = new Map([['sha-1', null], ['sha-256', 'SHA-256'], ['sha-384', 'SHA-384'], ['sha-512', 'SHA-512']]);

  class SubtleCrypto {
    constructor() { throw new TypeError('Illegal constructor'); }

    digest(algorithm, data) {
      return new Promise((resolve, reject) => {
        const name = typeof algorithm === 'string' ? algorithm : algorithm?.name;
        if (typeof name !== 'string') {
          reject(new TypeError("Failed to execute 'digest' on 'SubtleCrypto': Algorithm: name: Missing or not a string"));
          return;
        }
        const key = name.toLowerCase();
        if (!DIGESTS.has(key)) {
          reject(new DOMException('Unrecognized algorithm name', 'NotSupportedError'));
          return;
        }
        const nativeName = DIGESTS.get(key);
        if (nativeName === null) {
          reject(new DOMException(`${name} is not supported`, 'NotSupportedError'));
          return;
        }
        const bytes = bytesOf(data);
        if (bytes === null) {
          reject(new TypeError("Failed to execute 'digest' on 'SubtleCrypto': parameter 2 is not of type '(ArrayBuffer or ArrayBufferView)'."));
          return;
        }
        resolve(native.digest(nativeName, bytes));
      });
    }

    get [Symbol.toStringTag]() { return 'SubtleCrypto'; }
  }

  const subtle = Object.create(SubtleCrypto.prototype);

  class Crypto {
    constructor() { throw new TypeError('Illegal constructor'); }

    get subtle() { return subtle; }

    getRandomValues(array) {
      if (!ArrayBuffer.isView(array) || array instanceof DataView) {
        throw new TypeError("Failed to execute 'getRandomValues' on 'Crypto': parameter 1 is not of type 'ArrayBufferView'.");
      }
      if (!INTEGER_ARRAYS.some(C => array instanceof C)) {
        throw new DOMException(`Failed to execute 'getRandomValues' on 'Crypto': The provided ArrayBufferView is of type '${array.constructor.name}', which is not an integer array type.`, 'TypeMismatchError');
      }
      if (array.byteLength > 65536) {
        throw new DOMException(`Failed to execute 'getRandomValues' on 'Crypto': The ArrayBufferView's byte length (${array.byteLength}) exceeds the number of bytes of entropy available via this API (65536).`, 'QuotaExceededError');
      }
      native.randomFill(new Uint8Array(array.buffer, array.byteOffset, array.byteLength));
      return array;
    }

    randomUUID() { return native.randomUUID(); }

    get [Symbol.toStringTag]() { return 'Crypto'; }
  }

  define('Crypto', Crypto);
  define('SubtleCrypto', SubtleCrypto);
  define('crypto', Object.create(Crypto.prototype));
})
