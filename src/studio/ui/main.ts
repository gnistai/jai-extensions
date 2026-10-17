// JAI Studio webview UI entry (studio/DESIGN.md section 11): the tabs (View,
// Encode, Models, Server), the engine status pill, first-run routing to Models,
// open-path events and file drops.
import { api, ENGINE_WORDS, engineView, events, hasTauri, onFileDrop, usesMock, type AppInfo, type StatusReply } from './api.ts';
import { $, errorText, toast } from './dom.ts';
import { initEncode, type EncodeTab } from './encode.ts';
import { initModels } from './models.ts';
import { IMAGE_FILE, IMAGE_FORMATS, JAI_FILE } from './rules.ts';
import { initServer } from './server.ts';
import { initView, type ViewTab } from './view.ts';

export type TabId = 'view' | 'encode' | 'models' | 'server';

/** What main.ts gives each tab module. */
export interface TabContext {
  /** Shows a tab; with `focus`, keyboard focus moves to its tab button. */
  show(tab: TabId, focus?: boolean): void;
  /** Opens a document in the viewer and shows View. */
  openDocument(path: string): void;
  /** Subscribes to the engine status (polled, faster while engines change); called at once when it is known. */
  onStatus(fn: (s: StatusReply) => void): void;
  /** Polls the status now, after an action that changes it. */
  refreshStatus(): void;
  /** Runs `fn` whenever `tab` becomes visible. */
  onShown(tab: TabId, fn: () => void): void;
  isShown(tab: TabId): boolean;
  /** app_info, loaded at start; null if it failed. */
  info(): AppInfo | null;
  /** Reads app_info again (after installing the VC++ runtime); `quiet` shows no toast when that fails. */
  reloadInfo(opts?: { quiet?: boolean }): Promise<AppInfo | null>;
}

const TABS: TabId[] = ['view', 'encode', 'models', 'server'];
const tabButton = (t: TabId): HTMLButtonElement => $<HTMLButtonElement>(`tab-${t}`);
const panel = (t: TabId): HTMLElement => $(`panel-${t}`);
const scroller = $('main');

let current: TabId = 'view';
/** Set once the user (or an open-path) picked a tab, so first-run routing leaves it alone. */
let navigated = false;
let appInfo: AppInfo | null = null;
let status: StatusReply | null = null;
let view: ViewTab | null = null;
let encode: EncodeTab | null = null;
/** Paths that arrived before the tabs were ready, in order. */
const pendingPaths: string[] = [];
const shownFns = new Map<TabId, Array<() => void>>();
const statusFns: Array<(s: StatusReply) => void> = [];
const scrollTops = new Map<TabId, number>();

document.documentElement.dataset.host = hasTauri() ? 'tauri' : 'browser';
$('mock-tag').hidden = !usesMock();

function show(tab: TabId, focus = false): void {
  if (tab !== current) {
    scrollTops.set(current, scroller.scrollTop);
    for (const t of TABS) {
      const on = t === tab;
      tabButton(t).setAttribute('aria-selected', String(on));
      tabButton(t).tabIndex = on ? 0 : -1;
      panel(t).hidden = !on;
    }
    current = tab;
    scroller.scrollTop = scrollTops.get(tab) ?? 0;
  }
  if (focus) tabButton(tab).focus();
  for (const fn of shownFns.get(tab) ?? []) fn();
}

for (const t of TABS) tabButton(t).addEventListener('click', () => { navigated = true; show(t); });
$('tabs').addEventListener('keydown', e => {
  const i = TABS.indexOf(current), n = TABS.length;
  const moves: Record<string, TabId | undefined> = { ArrowRight: TABS[(i + 1) % n], ArrowLeft: TABS[(i + n - 1) % n], Home: TABS[0], End: TABS[n - 1] };
  const next = moves[e.key];
  if (!next) return;
  e.preventDefault();
  navigated = true;
  show(next, true);
});

// ---------------------------------------------------------------- engine status pill

function renderPill(s: StatusReply): void {
  $('pill-missing').hidden = s.installed;
  for (const kind of ['llm', 'image'] as const) {
    const v = engineView(s.engines, kind);
    $(`pill-${kind}`).hidden = !s.installed;
    $(`pill-${kind}`).dataset.state = v;
    $(`pill-${kind}-state`).textContent = ENGINE_WORDS[v];
  }
  const pill = $('status-pill');
  pill.dataset.error = s.engines.state === 'failed' ? 'true' : 'false';
  pill.title = !s.installed ? 'Models are not installed: open the Models tab'
    : s.engines.lastError ? `Last engine error: ${s.engines.lastError}` : 'Engine status: open the Server tab';
}

let polling = false, pollTimer = 0;
/** Reads the status, then schedules the next read: soon while engines change, less often when idle. */
async function poll(): Promise<void> {
  if (polling) return;
  polling = true;
  clearTimeout(pollTimer);
  try {
    status = await api.status();
    renderPill(status);
    for (const fn of statusFns) fn(status);
  } catch (e) {
    $('status-pill').title = `Could not read the engine status: ${errorText(e)}`;
  } finally {
    polling = false;
    const e = status?.engines;
    const busy = !!e && (e.state === 'starting' || e.state === 'stopping' || e.active > 0 || e.waitingLlm + e.waitingImage > 0);
    pollTimer = window.setTimeout(() => void poll(), document.hidden ? 5000 : busy ? 700 : 2000);
  }
}
document.addEventListener('visibilitychange', () => { if (!document.hidden) void poll(); });

$('status-pill').addEventListener('click', () => { navigated = true; show(status && !status.installed ? 'models' : 'server'); });

// ---------------------------------------------------------------- documents and images from outside

/** A .jai file goes to View, an image to Encode; Studio opens nothing else. */
function route(path: string): void {
  if (!view || !encode) { pendingPaths.push(path); return; }
  if (IMAGE_FILE.test(path)) { show('encode'); encode.useImage(path); return; }
  if (!JAI_FILE.test(path)) { toast(`JAI Studio opens .jai files and ${IMAGE_FORMATS} images.`, 'err'); return; }
  show('view');
  view.open(path);
}

// Registered first; then the app sends the paths that waited for the page
// (the command line's, a second launch's, the file association's).
void events.openPath(p => { navigated = true; route(p.path); })
  .then(() => api.uiReady())
  .catch(e => console.warn(`JAI Studio: open-path is not set up: ${errorText(e)}`));

const dragging = (on: boolean): void => { document.body.classList.toggle('dragging', on); };
if (hasTauri()) {
  // Tauri reports file drops with their paths; keep the webview from navigating to a dropped file.
  window.addEventListener('dragover', e => e.preventDefault());
  window.addEventListener('drop', e => e.preventDefault());
  void onFileDrop({ enter: () => dragging(true), leave: () => dragging(false), drop: paths => { dragging(false); if (paths[0]) { navigated = true; route(paths[0]); } } });
} else {
  // Browser preview: HTML5 drops have no paths, so the mock gets the file name.
  let depth = 0;
  window.addEventListener('dragenter', e => { e.preventDefault(); depth++; dragging(true); });
  window.addEventListener('dragleave', () => { depth = Math.max(0, depth - 1); if (!depth) dragging(false); });
  window.addEventListener('dragover', e => e.preventDefault());
  window.addEventListener('drop', e => {
    e.preventDefault();
    depth = 0;
    dragging(false);
    const f = e.dataTransfer?.files?.[0];
    if (f) { navigated = true; route(f.name); }
  });
}

document.addEventListener('keydown', e => {
  if (!(e.ctrlKey || e.metaKey) || e.shiftKey || e.altKey || e.key.toLowerCase() !== 'o') return;
  if (current === 'view') { e.preventDefault(); view?.openDialog(); }
  if (current === 'encode') { e.preventDefault(); encode?.openDialog(); }
});

// ---------------------------------------------------------------- start

const ctx: TabContext = {
  show,
  openDocument: path => { navigated = true; show('view'); view?.open(path); },
  onStatus: fn => { statusFns.push(fn); if (status) fn(status); },
  refreshStatus: () => { void poll(); },
  onShown: (tab, fn) => { shownFns.set(tab, [...(shownFns.get(tab) ?? []), fn]); },
  isShown: tab => current === tab,
  info: () => appInfo,
  reloadInfo: async ({ quiet = false } = {}) => {
    try { appInfo = await api.appInfo(); } catch (e) { if (!quiet) toast(`Could not read the app details: ${errorText(e)}`, 'err'); }
    return appInfo;
  },
};

async function start(): Promise<void> {
  await ctx.reloadInfo();
  view = initView(ctx);
  encode = initEncode(ctx);
  initModels(ctx);
  initServer(ctx);
  await poll();
  if (pendingPaths.length) for (const path of pendingPaths.splice(0)) route(path);
  else if (!navigated) show(status && !status.installed ? 'models' : 'view');
}

void start();
