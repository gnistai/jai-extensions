// OpenAI-style chat completions client, for JAI Studio's local server.
// UI-free and environment-neutral: it needs only fetch (tests inject one),
// AbortController and setTimeout, which JAI Studio's QuickJS host provides too.
// See studio/DESIGN.md section 9.
import type { FetchLike } from './replicate-client.ts';

export type ChatContentPart = { type: 'text'; text: string } | { type: 'image_url'; image_url: { url: string } };

export interface ChatMessage { role: 'system' | 'user' | 'assistant'; content: string | ChatContentPart[] }

export interface ChatRequest {
  messages: ChatMessage[];
  max_tokens?: number;
  temperature?: number;
  top_p?: number;
  top_k?: number;
  presence_penalty?: number;
  response_format?: unknown;
  chat_template_kwargs?: Record<string, unknown>;
  seed?: number;
}

export interface ChatResult { text: string; finishReason: string | null; usage: Record<string, number> | null; timings: Record<string, number> | null }

export type ProviderErrorKind = 'auth' | 'refused' | 'provider' | 'network' | 'timeout' | 'canceled';

/** A failed call to the local server. `status` and `code` come from the HTTP reply, when there was one. */
export class ProviderError extends Error {
  constructor(message: string, readonly kind: ProviderErrorKind, readonly status: number | null = null, readonly code: string | null = null) {
    super(message);
    this.name = 'ProviderError';
  }
}

/**
 * Ten minutes, the wait for the engine included: the server answers once the
 * scheduler has swapped the LLM in and the requests ahead are done. The encoder
 * keeps only a few requests of a kind in flight, so that wait stays short.
 */
const CHAT_TIMEOUT_MS = 600_000;

/** The request fields `complete()` forwards besides `messages`. */
export const CHAT_FIELDS = ['max_tokens', 'temperature', 'top_p', 'top_k', 'presence_penalty', 'response_format', 'chat_template_kwargs', 'seed'] as const;

/** Error codes of an HTTP client's own time limits (undici, behind Node's fetch). */
const HTTP_CLIENT_TIMEOUTS = new Set(['UND_ERR_HEADERS_TIMEOUT', 'UND_ERR_BODY_TIMEOUT']);

/** The error reply's message: OpenAI's {error: {message}}, else a short piece of the body. */
function errorMessage(body: unknown, text: string): string {
  const o = body && typeof body === 'object' ? (body as Record<string, unknown>) : null;
  const err = o?.error;
  const nested = err && typeof err === 'object' ? (err as Record<string, unknown>).message : undefined;
  if (typeof nested === 'string') return nested;
  if (typeof err === 'string') return err;
  if (typeof o?.message === 'string') return o.message;
  return o ? '' : text.replace(/\s+/g, ' ').trim().slice(0, 200);
}

function errorCode(body: unknown): string | null {
  const err = body && typeof body === 'object' ? (body as Record<string, unknown>).error : null;
  const code = err && typeof err === 'object' ? (err as Record<string, unknown>).code : null;
  return typeof code === 'string' ? code : null;
}

/** An error's message; errors from the host may come from another realm, so no instanceof. */
export function errorText(e: unknown): string {
  const m = (e as { message?: unknown } | null)?.message;
  return typeof m === 'string' ? m : String(e);
}

function kindOf(status: number): ProviderErrorKind {
  if (status === 401 || status === 403) return 'auth';
  if (status === 408 || status === 504) return 'timeout';
  if (status === 400 || status === 413 || status === 415 || status === 422) return 'refused';
  return 'provider';
}

/**
 * POST a JSON body with a Bearer token and return the reply's JSON object.
 * `signal` cancels (kind 'canceled'); `timeoutMs` bounds the whole call,
 * reading the reply included (kind 'timeout'). Non-2xx replies throw with the
 * server's message. Shared by OpenAIChatClient and LocalRenderer.
 */
export async function postJSON(fetchFn: FetchLike, url: string, token: string, payload: unknown, opts: { what: string; timeoutMs: number; signal?: AbortSignal }): Promise<Record<string, unknown>> {
  const { signal } = opts;
  if (signal?.aborted) throw new ProviderError('Canceled', 'canceled');
  const body = JSON.stringify(payload);
  // AbortSignal.any and AbortSignal.timeout are not in every runtime (Node 18,
  // the QuickJS host), so one controller carries both the caller's cancel and the timeout.
  const ac = new AbortController();
  let timedOut = false;
  const onAbort = (): void => ac.abort();
  signal?.addEventListener('abort', onAbort);
  const timer = setTimeout(() => { timedOut = true; ac.abort(); }, opts.timeoutMs);
  const failure = (e: unknown): ProviderError => {
    if (signal?.aborted) return new ProviderError('Canceled', 'canceled');
    if (timedOut) return new ProviderError(`${opts.what} timed out after ${Math.round(opts.timeoutMs / 1000)} s`, 'timeout');
    const cause = (e as { cause?: { code?: unknown } } | null)?.cause?.code;
    // Node's fetch (undici) has its own limits, such as 300 s for the reply headers.
    if (typeof cause === 'string' && HTTP_CLIENT_TIMEOUTS.has(cause)) return new ProviderError(`${opts.what} timed out: the HTTP client stopped waiting for the server (${cause})`, 'timeout');
    const detail = typeof cause === 'string' ? cause : errorText(e);
    return new ProviderError(`Could not reach the local server at ${url.replace(/^([a-z][a-z0-9+.-]*:\/\/[^/]*).*$/is, '$1')}${detail ? ` (${detail})` : ''}`, 'network');
  };
  try {
    let res: Response;
    let text: string;
    try {
      res = await fetchFn(url, { method: 'POST', headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' }, body, signal: ac.signal });
      text = await res.text();
    } catch (e) {
      throw failure(e);
    }
    let json: unknown = null;
    try { json = text ? JSON.parse(text) : null; } catch { json = null; }
    if (!res.ok) {
      const message = errorMessage(json, text);
      throw new ProviderError(`${opts.what} failed (HTTP ${res.status})${message ? `: ${message}` : ''}`, kindOf(res.status), res.status, errorCode(json));
    }
    if (!json || typeof json !== 'object' || Array.isArray(json)) throw new ProviderError(`${opts.what} failed: the server's reply is not a JSON object`, 'provider', res.status);
    return json as Record<string, unknown>;
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener('abort', onAbort);
  }
}

/** The finite numbers of an object (usage, timings); null when it is not an object. */
function numbers(v: unknown): Record<string, number> | null {
  if (!v || typeof v !== 'object' || Array.isArray(v)) return null;
  const out: Record<string, number> = {};
  for (const [k, x] of Object.entries(v)) if (typeof x === 'number' && Number.isFinite(x)) out[k] = x;
  return out;
}

export class OpenAIChatClient {
  private readonly base: string;

  constructor(baseUrl: string, private readonly token: string, private readonly opts: { fetch?: FetchLike; timeoutMs?: number } = {}) {
    this.base = baseUrl.replace(/\/+$/, '');
  }

  private get fetch(): FetchLike { return this.opts.fetch ?? ((u, i) => fetch(u, i)); }

  /** One chat completion (not streamed); throws ProviderError. */
  async complete(req: ChatRequest, opts: { signal?: AbortSignal } = {}): Promise<ChatResult> {
    const payload: Record<string, unknown> = { messages: req.messages };
    for (const k of CHAT_FIELDS) if (req[k] !== undefined) payload[k] = req[k];
    const body = await postJSON(this.fetch, `${this.base}/v1/chat/completions`, this.token, payload, { what: 'Chat completion', timeoutMs: this.opts.timeoutMs ?? CHAT_TIMEOUT_MS, signal: opts.signal });
    const choice = Array.isArray(body.choices) ? (body.choices[0] as Record<string, unknown> | undefined) : undefined;
    if (!choice || typeof choice !== 'object') throw new ProviderError('Chat completion failed: the reply has no choices', 'provider');
    const message = (choice.message && typeof choice.message === 'object' ? choice.message : {}) as Record<string, unknown>;
    if (typeof message.refusal === 'string' && message.refusal) throw new ProviderError(`The model refused: ${message.refusal}`, 'refused');
    const content = message.content;
    const text = typeof content === 'string' ? content
      : Array.isArray(content) ? content.map(p => (p && typeof p === 'object' && typeof (p as { text?: unknown }).text === 'string' ? (p as { text: string }).text : '')).join('')
      : '';
    return { text, finishReason: typeof choice.finish_reason === 'string' ? choice.finish_reason : null, usage: numbers(body.usage), timings: numbers(body.timings) };
  }
}
