// Rules of the webview UI that mirror the app's own checks, kept free of the
// DOM so that test/studio-ui.test.mjs can run them (built to dist/test/).
import type { EncodeSettings, Thinking, VcInstallReply } from './api.ts';

/** Images the app can decode (imageops: PNG, JPEG, WebP, GIF), by file name. */
export const IMAGE_FILE = /\.(png|jpe?g|webp|gif)$/i;
/** The same formats in words, for the UI's texts. */
export const IMAGE_FORMATS = 'PNG, JPEG, WebP or GIF';
/** A JAI document file, by its name: a .jai file holds the file form and is the only document file Studio opens. */
export const JAI_FILE = /\.jai$/i;

/**
 * encode_start's ranges (studio/app/src/encoding.rs, EncodeSettings::check),
 * which the app enforces; the UI offers at most three attempts of its five.
 */
export const ENCODE_LIMITS = {
  generations: [1, 30], population: [1, 9], seeds: 8, seedMax: 4294967295, restarts: [1, 3], scale: [0.25, 1.25],
} as const;

export type SettingsField = 'generations' | 'population' | 'seeds' | 'restarts' | 'goal' | 'scale';
export type SettingsInput = Record<SettingsField, string> & { thinking: string };
export type SettingsCheck = { ok: true; settings: EncodeSettings } | { ok: false; field: SettingsField; message: string };

const THINKING: readonly Thinking[] = ['off', 'propose', 'all'];

/** The Encode tab's fields as encode_start takes them, or the first field that is out of range, with what to enter. */
export function checkEncodeSettings(v: SettingsInput): SettingsCheck {
  const L = ENCODE_LIMITS;
  const bad = (field: SettingsField, message: string): SettingsCheck => ({ ok: false, field, message });
  const whole = (s: string, [lo, hi]: readonly [number, number]): number | null => { const n = Number(s); return s.trim() && Number.isInteger(n) && n >= lo && n <= hi ? n : null; };
  const generations = whole(v.generations, L.generations);
  if (generations === null) return bad('generations', `Generations: a whole number from ${L.generations[0]} to ${L.generations[1]}.`);
  const population = whole(v.population, L.population);
  if (population === null) return bad('population', `Candidates per generation: a whole number from ${L.population[0]} to ${L.population[1]}.`);
  const seeds = v.seeds.split(/[\s,]+/).filter(Boolean).map(Number);
  if (!seeds.length || seeds.length > L.seeds || seeds.some(s => !Number.isInteger(s) || s < 0 || s > L.seedMax)) return bad('seeds', `Seeds: 1 to ${L.seeds} whole numbers from 0 to ${L.seedMax}, separated by commas.`);
  const restarts = whole(v.restarts, L.restarts);
  if (restarts === null) return bad('restarts', `Attempts: a whole number from ${L.restarts[0]} to ${L.restarts[1]}.`);
  const goal = Number(v.goal);
  if (!v.goal.trim() || !(goal > 0 && goal <= 1)) return bad('goal', 'Goal: a fitness above 0 and at most 1, such as 0.9.');
  const scale = Number(v.scale);
  if (!v.scale.trim() || !(scale >= L.scale[0] && scale <= L.scale[1])) return bad('scale', `Render scale: from ${L.scale[0]} to ${L.scale[1]}.`);
  const thinking = THINKING.includes(v.thinking as Thinking) ? (v.thinking as Thinking) : 'off';
  return { ok: true, settings: { generations, population, seeds, restarts, goal, scale, thinking } };
}

const STOPS: Record<string, string> = {
  goal: 'the goal was reached', plateau: 'fitness stopped improving', generations: 'every generation ran', time: 'the time limit was reached',
  canceled: 'you canceled', error: 'of an error', complete: 'it finished', budget: 'the budget ran out',
};

/** Why a run that has a result stopped, in one sentence: with the error's message when it stopped on one. */
export function stopSentence(stopReason: string | undefined, error: string | undefined | null): string {
  if (!stopReason) return '';
  const message = error?.trim().replace(/[.\s]+$/, '');
  if (stopReason === 'error' && message) return `Stopped because of an error: ${message}.`;
  return `Stopped because ${STOPS[stopReason] ?? stopReason}.`;
}

/**
 * The settings form's port, or null when it is not one to save. A port the
 * user types must be from 1024 to 65535 (lower ones are reserved for system
 * services); the port config.json already holds stays allowed whatever it is,
 * as the app accepts 1 to 65535, so a save of other settings never fails on it.
 */
export function checkPort(value: string, stored: number): number | null {
  const n = Number(value);
  if (!value.trim() || !Number.isInteger(n) || n < 1 || n > 65535) return null;
  return n >= 1024 || n === stored ? n : null;
}

/**
 * What the Models tab says after install_vc_runtime while the runtime still
 * looks out of date, and whether it keeps checking (a restart, or an
 * installer still at work, may yet bring it).
 */
export function vcInstallNote(r: VcInstallReply): { message: string; kind: 'warn' | 'err'; watch: boolean } {
  if (r.outcome === 'declined') return { message: 'Nothing was installed. You can install it again.', kind: 'warn', watch: false };
  if (r.outcome === 'started') return { message: 'When the installer has finished, this warning goes away.', kind: 'warn', watch: true };
  switch (r.exitCode) {
    case 0: return { message: 'The installer has finished. If this warning stays, restart Windows and open JAI Studio again.', kind: 'warn', watch: true };
    case 3010: case 1641: return { message: 'Installed. Restart Windows to finish, then open JAI Studio again.', kind: 'warn', watch: true };
    case 1602: return { message: 'The installer was canceled. You can install it again.', kind: 'warn', watch: false };
    case 1638: return { message: 'Windows says a newer version is installed already. Restart Windows; if this warning stays, repair "Microsoft Visual C++ Redistributable (x64)" under Settings › Apps.', kind: 'warn', watch: true };
    default: return { message: `The installer failed (exit code ${r.exitCode ?? 'unknown'}). You can install it again.`, kind: 'err', watch: false };
  }
}
