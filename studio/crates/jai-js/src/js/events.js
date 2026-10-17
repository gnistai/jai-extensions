// Event, EventTarget, AbortSignal and AbortController for JAI Studio's QuickJS
// host (studio/DESIGN.md section 8). Listeners that throw are reported, not
// rethrown, as in browsers.
(function (native, internal) {
  'use strict';
  const { define, report, setTimeout } = internal;

  let beginDispatch, endDispatch, isDispatching, immediateStopped;

  class Event {
    #type;
    #bubbles;
    #cancelable;
    #composed;
    #target = null;
    #defaultPrevented = false;
    #stopImmediate = false;
    #dispatching = false;
    #timeStamp = performance.now();

    constructor(type, init = undefined) {
      if (arguments.length === 0) throw new TypeError("Failed to construct 'Event': 1 argument required, but only 0 present.");
      this.#type = String(type);
      this.#bubbles = !!init?.bubbles;
      this.#cancelable = !!init?.cancelable;
      this.#composed = !!init?.composed;
    }

    get type() { return this.#type; }
    get target() { return this.#target; }
    get srcElement() { return this.#target; }
    get currentTarget() { return this.#dispatching ? this.#target : null; }
    get eventPhase() { return this.#dispatching ? Event.AT_TARGET : Event.NONE; }
    get bubbles() { return this.#bubbles; }
    get cancelable() { return this.#cancelable; }
    get composed() { return this.#composed; }
    get defaultPrevented() { return this.#defaultPrevented; }
    get returnValue() { return !this.#defaultPrevented; }
    get isTrusted() { return false; }
    get timeStamp() { return this.#timeStamp; }
    get cancelBubble() { return this.#stopImmediate; }
    set cancelBubble(value) { if (value) this.#stopImmediate = true; }
    composedPath() { return this.#dispatching ? [this.#target] : []; }
    preventDefault() { if (this.#cancelable) this.#defaultPrevented = true; }
    stopPropagation() {}
    stopImmediatePropagation() { this.#stopImmediate = true; }
    get [Symbol.toStringTag]() { return 'Event'; }

    static {
      beginDispatch = (event, target) => { event.#target = target; event.#dispatching = true; event.#stopImmediate = false; };
      endDispatch = event => { event.#dispatching = false; };
      isDispatching = event => event.#dispatching;
      immediateStopped = event => event.#stopImmediate;
    }
  }
  for (const [name, value] of [['NONE', 0], ['CAPTURING_PHASE', 1], ['AT_TARGET', 2], ['BUBBLING_PHASE', 3]]) {
    Object.defineProperty(Event, name, { value, enumerable: true });
    Object.defineProperty(Event.prototype, name, { value, enumerable: true });
  }

  const flattenOptions = options => (typeof options === 'object' && options !== null ? options : { capture: !!options });

  class EventTarget {
    /** type -> [{ callback, capture, once, removed }] in registration order */
    #listeners = new Map();

    addEventListener(type, callback, options = undefined) {
      if (callback === null || callback === undefined) return;
      if (typeof callback !== 'function' && typeof callback !== 'object') {
        throw new TypeError("Failed to execute 'addEventListener' on 'EventTarget': parameter 2 is not of type 'Object'.");
      }
      const opts = flattenOptions(options);
      const capture = !!opts.capture;
      const signal = opts.signal;
      if (signal !== undefined && !(signal instanceof AbortSignal)) {
        throw new TypeError("Failed to execute 'addEventListener' on 'EventTarget': member signal is not of type AbortSignal.");
      }
      if (signal?.aborted) return;
      const key = String(type);
      let list = this.#listeners.get(key);
      if (!list) this.#listeners.set(key, (list = []));
      if (list.some(l => l.callback === callback && l.capture === capture)) return;
      list.push({ callback, capture, once: !!opts.once, removed: false });
      if (signal) signal.addEventListener('abort', () => this.removeEventListener(key, callback, { capture }), { once: true });
    }

    removeEventListener(type, callback, options = undefined) {
      const capture = !!flattenOptions(options).capture;
      const list = this.#listeners.get(String(type));
      if (!list) return;
      const i = list.findIndex(l => l.callback === callback && l.capture === capture);
      if (i < 0) return;
      list[i].removed = true;
      list.splice(i, 1);
    }

    dispatchEvent(event) {
      if (!(event instanceof Event)) throw new TypeError("Failed to execute 'dispatchEvent' on 'EventTarget': parameter 1 is not of type 'Event'.");
      if (isDispatching(event)) throw new DOMException('The event is already being dispatched.', 'InvalidStateError');
      beginDispatch(event, this);
      try {
        const list = this.#listeners.get(event.type);
        for (const l of list ? list.slice() : []) {
          if (l.removed) continue;
          if (l.once) this.removeEventListener(event.type, l.callback, { capture: l.capture });
          try {
            if (typeof l.callback === 'function') l.callback.call(this, event);
            else if (typeof l.callback.handleEvent === 'function') l.callback.handleEvent(event);
          } catch (e) {
            report(e);
          }
          if (immediateStopped(event)) break;
        }
      } finally {
        endDispatch(event);
      }
      return !event.defaultPrevented;
    }

    get [Symbol.toStringTag]() { return 'EventTarget'; }
  }

  let constructing = false;
  let signalAbort, newSignal;

  class AbortSignal extends EventTarget {
    #aborted = false;
    #reason = undefined;
    #onabort = null;
    #onabortListener = null;
    /** Signals made by AbortSignal.any() that follow this one, held weakly. */
    #dependents = new Set();
    /** For a signal made by AbortSignal.any(): the plain signals it follows (never dependent ones). */
    #sources = null;

    constructor() {
      if (!constructing) throw new TypeError('Illegal constructor');
      super();
    }

    get aborted() { return this.#aborted; }
    get reason() { return this.#reason; }
    throwIfAborted() { if (this.#aborted) throw this.#reason; }

    get onabort() { return this.#onabort; }
    set onabort(handler) {
      this.#onabort = typeof handler === 'function' ? handler : null;
      if (this.#onabort && !this.#onabortListener) {
        this.#onabortListener = event => { if (typeof this.#onabort === 'function') this.#onabort.call(this, event); };
        this.addEventListener('abort', this.#onabortListener);
      }
    }

    get [Symbol.toStringTag]() { return 'AbortSignal'; }

    static abort(reason = undefined) {
      const signal = newSignal();
      signalAbort(signal, reason);
      return signal;
    }

    static timeout(ms) {
      const n = Number(ms);
      if (!Number.isFinite(n) || n < 0) throw new TypeError(`Failed to execute 'timeout' on 'AbortSignal': Value is outside the 'unsigned long long' value range.`);
      const signal = newSignal();
      setTimeout(() => signalAbort(signal, new DOMException('The operation was aborted due to timeout', 'TimeoutError')), n);
      return signal;
    }

    static any(signals) {
      const sources = Array.from(signals);
      for (const s of sources) {
        if (!(s instanceof AbortSignal)) throw new TypeError("Failed to execute 'any' on 'AbortSignal': Failed to convert value to 'AbortSignal'.");
      }
      const signal = newSignal();
      const aborted = sources.find(s => s.#aborted);
      if (aborted) {
        signal.#aborted = true;
        signal.#reason = aborted.#reason;
        return signal;
      }
      // Follow the sources of dependent signals instead (the spec's flattening),
      // so a dependent signal never has dependents of its own.
      signal.#sources = [];
      const ref = new WeakRef(signal);
      for (const s of sources) {
        for (const source of s.#sources ?? [s]) {
          if (signal.#sources.includes(source)) continue;
          signal.#sources.push(source);
          source.#dependents.add(ref);
        }
      }
      return signal;
    }

    static {
      newSignal = () => {
        constructing = true;
        try { return new AbortSignal(); } finally { constructing = false; }
      };
      signalAbort = (signal, reason) => {
        if (signal.#aborted) return;
        signal.#aborted = true;
        signal.#reason = reason === undefined ? new DOMException('This operation was aborted', 'AbortError') : reason;
        const dependents = [...signal.#dependents];
        signal.#dependents.clear();
        const followers = [];
        for (const ref of dependents) {
          const d = ref.deref();
          if (d && !d.#aborted) {
            d.#aborted = true;
            d.#reason = signal.#reason;
            d.#dependents.clear();
            followers.push(d);
          }
        }
        signal.dispatchEvent(new Event('abort'));
        for (const d of followers) d.dispatchEvent(new Event('abort'));
      };
    }
  }

  class AbortController {
    #signal = newSignal();
    get signal() { return this.#signal; }
    abort(reason = undefined) { signalAbort(this.#signal, reason); }
    get [Symbol.toStringTag]() { return 'AbortController'; }
  }

  define('Event', Event);
  define('EventTarget', EventTarget);
  define('AbortSignal', AbortSignal);
  define('AbortController', AbortController);
  Object.assign(internal, { AbortSignal });
})
