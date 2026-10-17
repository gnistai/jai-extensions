// Price estimates for token-priced models on Replicate, in USD per million
// tokens. Used to hold a worst case before a call and to charge the actual
// usage when Replicate reports token counts in the prediction metrics.
// Estimates only: Replicate bills what it bills.
interface TokenPrice { input: number; output: number; worstInput: number; worstOutput: number; typical: number }

export const TOKEN_PRICES: Record<string, TokenPrice> = {
  // max_output_tokens 8192 plus a thinking budget of up to 1024 tokens; up to 10 images and 20,000 characters of text.
  'google/gemini-2.5-flash': { input: 0.30, output: 2.50, worstInput: 30_000, worstOutput: 9_216, typical: 0.01 },
  // max_tokens 4096; one image and up to 20,000 characters of text.
  'anthropic/claude-sonnet-5': { input: 3, output: 15, worstInput: 20_000, worstOutput: 4_096, typical: 0.04 },
  // GPT-5.6 luna (OpenAI's cost-optimised tier): estimated prices; max_completion_tokens 4096, up to 10 images.
  'openai/gpt-5.6-luna': { input: 0.4, output: 2, worstInput: 30_000, worstOutput: 4_096, typical: 0.002 },
};

/** The most one call can cost under the input limits the encoder uses. */
export function worstCaseUSD(model: string): number {
  const p = TOKEN_PRICES[model];
  return p ? (p.worstInput * p.input + p.worstOutput * p.output) / 1e6 : Infinity;
}

/** Cost from reported token counts, else the typical estimate; null for models not priced by token. */
export function tokenCostUSD(model: string, metrics?: Record<string, unknown> | null): number | null {
  const p = TOKEN_PRICES[model];
  if (!p) return null;
  // Replicate reports token_input_count / token_output_count; accept the other order too.
  const i = Number(metrics?.token_input_count ?? metrics?.input_token_count), o = Number(metrics?.token_output_count ?? metrics?.output_token_count);
  if (Number.isFinite(i) && Number.isFinite(o) && i >= 0 && o >= 0) return (i * p.input + o * p.output) / 1e6;
  return p.typical;
}
