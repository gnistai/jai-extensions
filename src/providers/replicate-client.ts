// Minimal Replicate predictions client, shared by the renderer and the encoder.
// UI-free and environment-neutral: it needs only fetch. Apps that must keep the
// token in one context (the extension's background) implement ReplicateClient
// over a message port instead.
import { RenderError } from '../renderers/types.ts';

export type FetchLike = (url: string, init?: RequestInit) => Promise<Response>;

/** An official model is called by owner/name; a community model needs its version. */
export interface ModelRef { id: string; version?: string }

export interface Prediction {
  id: string;
  status: string;
  output?: unknown;
  error?: unknown;
  metrics?: { predict_time?: number; [k: string]: unknown };
}

export interface PredictOptions { signal?: AbortSignal; onStatus?: (status: string) => void; timeoutMs?: number }

export interface ReplicateClient {
  /** Run a prediction to completion; throws RenderError on failure, refusal, timeout or cancel. */
  predict(model: ModelRef, input: Record<string, unknown>, opts?: PredictOptions): Promise<Prediction>;
}

export class HttpReplicateClient implements ReplicateClient {
  constructor(private readonly token: string, private readonly opts: { fetch?: FetchLike; pollMs?: number; timeoutMs?: number } = {}) {}

  private get fetch(): FetchLike { return this.opts.fetch ?? ((u, i) => fetch(u, i)); }

  private async api<T>(path: string, init: RequestInit = {}, signal?: AbortSignal): Promise<T> {
    let res: Response;
    try {
      res = await this.fetch('https://api.replicate.com/v1' + path, {
        ...init, signal, referrerPolicy: 'no-referrer', credentials: 'omit',
        headers: { Authorization: `Bearer ${this.token}`, 'Content-Type': 'application/json', ...(init.headers as Record<string, string> | undefined) },
      });
    } catch (e) {
      if ((e as Error).name === 'AbortError') throw new RenderError('Canceled', 'canceled');
      throw new RenderError('Network error contacting Replicate', 'network');
    }
    const body = (await res.json().catch(() => ({}))) as Record<string, unknown>;
    if (res.status === 401 || res.status === 403) throw new RenderError('Replicate rejected the API token', 'auth');
    if (!res.ok) throw new RenderError(`Replicate error ${res.status}: ${String(body.detail ?? body.title ?? '')}`.trim(), res.status === 422 ? 'refused' : 'provider');
    return body as T;
  }

  async cancel(id: string): Promise<void> {
    try { await this.api(`/predictions/${encodeURIComponent(id)}/cancel`, { method: 'POST' }); } catch { /* best effort */ }
  }

  async predict(model: ModelRef, input: Record<string, unknown>, opts: PredictOptions = {}): Promise<Prediction> {
    const path = model.version ? '/predictions' : `/models/${model.id}/predictions`;
    const payload = model.version ? { version: model.version, input } : { input };
    if (opts.signal?.aborted) throw new RenderError('Canceled', 'canceled');
    opts.onStatus?.('starting');
    // The create request is never aborted mid-flight: aborting it would leave a
    // prediction running whose id we never learn. Cancel by id once it is known.
    let p = await this.api<Prediction>(path, { method: 'POST', body: JSON.stringify(payload), headers: { Prefer: 'wait=5' } });
    if (opts.signal?.aborted) { if (!['succeeded', 'failed', 'canceled'].includes(p.status)) void this.cancel(p.id); throw new RenderError('Canceled', 'canceled', p.id); }
    const started = Date.now();
    const timeout = opts.timeoutMs ?? this.opts.timeoutMs ?? 180_000;
    const onAbort = (): void => { void this.cancel(p.id); };
    opts.signal?.addEventListener('abort', onAbort, { once: true });
    try {
      while (!['succeeded', 'failed', 'canceled'].includes(p.status)) {
        if (opts.signal?.aborted) { void this.cancel(p.id); throw new RenderError('Canceled', 'canceled', p.id); }
        if (Date.now() - started > timeout) { void this.cancel(p.id); throw new RenderError('Timed out', 'timeout', p.id); }
        opts.onStatus?.(p.status);
        await new Promise(r => setTimeout(r, this.opts.pollMs ?? 1500));
        p = await this.api<Prediction>(`/predictions/${encodeURIComponent(p.id)}`, {}, opts.signal);
      }
    } finally {
      opts.signal?.removeEventListener('abort', onAbort);
    }
    if (p.status === 'canceled') throw new RenderError('Canceled', 'canceled', p.id);
    if (p.status === 'failed') {
      const msg = String(p.error ?? 'failed');
      throw new RenderError(msg, /nsfw|safety|flagged|content/i.test(msg) ? 'refused' : 'provider', p.id);
    }
    return p;
  }
}
