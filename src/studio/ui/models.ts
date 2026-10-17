// Models tab (studio/DESIGN.md section 11): what JAI Studio downloads and how
// much space it needs, files it can reuse from LM Studio, one Download button
// with per-file progress, speed, time left and Cancel, and the VC++ runtime
// warning on Windows.
import { api, events, type ComponentRow, type DlState, type ModelsStatus, type Progress } from './api.ts';
import { $, el, errorText, fmtBytes, fmtDuration, fmtSpeed, say } from './dom.ts';
import type { TabContext } from './main.ts';
import { vcInstallNote } from './rules.ts';

const ENGINE_ROLES = new Set<string>(['llm-engine', 'llm-runtime', 'image-engine', 'image-runtime']);
const STATE_TEXT: Record<ComponentRow['state'], string> = { installed: 'Installed', partial: 'Partly downloaded', missing: 'Not downloaded', reusable: 'Found on this PC' };
const DL_TEXT: Record<DlState, string> = { queued: 'Waiting', reusing: 'Checking the found file', downloading: 'Downloading', verifying: 'Verifying', unpacking: 'Unpacking', done: 'Installed', failed: 'Failed', canceled: 'Canceled' };
const DL_KIND: Record<DlState, string> = { queued: 'muted', reusing: 'info', downloading: 'info', verifying: 'info', unpacking: 'info', done: 'ok', failed: 'err', canceled: 'warn' };
const GIB = 2 ** 30;

/** Where a reusable file was found, by its path. */
const foundIn = (path: string): string => (/lm[ -]?studio/i.test(path) ? 'LM Studio' : 'another app on this PC');
/** How found files are installed: hard-linked when that is safe (same disk, no other account can change them), else copied. */
const reuseVerb = (rows: ComponentRow[]): string => rows.every(c => c.reuseLinks !== false) ? 'linked' : rows.every(c => c.reuseLinks === false) ? 'copied' : 'linked or copied';

interface Row { li: HTMLLIElement; state: HTMLElement; barBox: HTMLElement; bar: HTMLProgressElement; barText: HTMLElement; note: HTMLElement; error: HTMLElement }

export function initModels(ctx: TabContext): void {
  const reuse = $<HTMLInputElement>('models-reuse'), download = $<HTMLButtonElement>('models-download'), cancel = $<HTMLButtonElement>('models-cancel');
  const msg = $('models-msg');
  let status: ModelsStatus | null = null;
  let running = false;
  const progress = new Map<string, Progress>();
  const rows = new Map<string, Row>();
  /** At download start: bytes already on disk per component, and the files reused instead of downloaded. */
  let startBytes = new Map<string, number>();
  let reused = new Set<string>();

  /** Bytes the download will fetch: missing and partial files, minus files reused from another app. */
  const toDownload = (s: ModelsStatus): number => s.components
    .filter(c => c.state !== 'installed' && !(c.state === 'reusable' && reuse.checked))
    .reduce((a, c) => a + c.size - (c.partialBytes ?? 0), 0);

  function renderHead(): void {
    const s = status;
    if (!s) return;
    const need = toDownload(s), reusable = s.components.filter(c => c.state === 'reusable');
    const installed = s.components.every(c => c.state === 'installed');
    $('models-total').textContent = installed ? 'Models are installed' : `Download models: ${fmtBytes(need)}`;
    const info = ctx.info();
    $('models-disk').textContent = [
      `${fmtBytes(s.totalBytes)} in all`,
      s.diskFree !== null ? `${fmtBytes(s.diskFree)} free on disk` : 'free disk space unknown',
      info ? `stored in ${info.dataDir}` : '',
    ].filter(Boolean).join(' · ');
    const short = s.diskFree !== null && !installed && s.diskFree < s.neededBytes + GIB;
    $('models-space').hidden = !short;
    if (short && s.diskFree !== null) $('models-space').textContent = `Not enough free disk space: the download needs ${fmtBytes(s.neededBytes + GIB - s.diskFree)} more, including 1 GiB to spare.`;
    $('models-reuse-row').hidden = !reusable.length || installed;
    if (reusable.length) $('models-reuse-note').textContent = reuse.checked
      ? `${reusable.length === 1 ? 'One file' : `${reusable.length} files`} (${fmtBytes(reusable.reduce((a, c) => a + c.size, 0))}) will be checked and ${reuseVerb(reusable)} instead of downloaded.`
      : 'Files found on this PC will be downloaded again.';
    download.hidden = running || installed;
    download.disabled = running;
    download.textContent = s.components.some(c => c.state === 'partial') ? 'Resume download' : 'Download models';
    $('dl-overall').hidden = !running;
    const vc = info !== null && info.os === 'windows' && !info.vcRuntimeOk;
    $('vc-warning').hidden = !vc;
  }

  function buildRow(c: ComponentRow): Row {
    const state = el('span', { className: 'chip' });
    const bar = el('progress', { max: 1, value: 0 });
    const barText = el('span', { className: 'bar-text muted' });
    const barBox = el('div', { className: 'comp-progress', hidden: true }, bar, barText);
    bar.setAttribute('aria-label', `${c.label} progress`);
    const note = el('p', { className: 'comp-note', hidden: true });
    const error = el('p', { className: 'comp-error err', hidden: true });
    const li = el('li', { className: 'comp' },
      el('div', { className: 'comp-main' }, el('span', { className: 'comp-label' }, c.label), el('span', { className: 'comp-size' }, fmtBytes(c.size)), state),
      el('p', { className: 'comp-meta muted' }, `${c.source} · ${c.license}`), note, barBox, error);
    return { li, state, barBox, bar, barText, note, error };
  }

  function renderRow(c: ComponentRow, row: Row): void {
    const p = progress.get(c.id);
    if (p) {
      row.state.textContent = DL_TEXT[p.state];
      row.state.className = `chip ${DL_KIND[p.state]}`;
      const active = p.state === 'downloading' || p.state === 'reusing' || p.state === 'verifying' || p.state === 'unpacking' || (p.state === 'queued' && p.bytes > 0);
      row.barBox.hidden = !active;
      row.bar.value = p.total ? p.bytes / p.total : 0;
      const share = p.total ? Math.floor((p.bytes / p.total) * 100) : 0;
      row.barText.textContent = p.state === 'downloading' ? `${share}% · ${fmtBytes(p.bytes)} of ${fmtBytes(p.total)} · ${fmtSpeed(p.speedBps)}`
        : p.state === 'reusing' ? `${share}% checked` : p.state === 'queued' ? `${fmtBytes(p.bytes)} already here` : `${DL_TEXT[p.state]}…`;
      row.error.hidden = !p.error;
      row.error.textContent = p.error ?? '';
    } else {
      row.state.textContent = c.state === 'partial' && c.partialBytes ? `${STATE_TEXT.partial} (${fmtBytes(c.partialBytes)})` : STATE_TEXT[c.state];
      row.state.className = `chip ${c.state === 'installed' ? 'ok' : c.state === 'reusable' ? 'info' : c.state === 'partial' ? 'warn' : 'muted'}`;
      row.barBox.hidden = true;
      row.error.hidden = true;
    }
    const reusable = c.state === 'reusable' && c.reusablePath && (!p || p.state === 'queued' || p.state === 'reusing');
    row.note.hidden = !reusable;
    if (reusable && c.reusablePath) {
      const how = c.reuseLinks === false ? ' (copied: it is on another disk, or other accounts can change it)' : '';
      row.note.replaceChildren(el('b', {}, `Found in ${foundIn(c.reusablePath)}: ${reuse.checked ? `will reuse${how}` : 'not reused'}`), el('span', { className: 'muted path' }, ` ${c.reusablePath}`));
    }
  }

  function renderList(): void {
    if (!status) return;
    const groups: Array<[string, ComponentRow[]]> = [
      ['Models', status.components.filter(c => !ENGINE_ROLES.has(c.role))],
      ['Engines', status.components.filter(c => ENGINE_ROLES.has(c.role))],
    ];
    const list = $('models-list');
    list.replaceChildren();
    rows.clear();
    for (const [name, comps] of groups) {
      if (!comps.length) continue;
      list.append(el('li', { className: 'comp-group' }, el('h3', {}, name)));
      for (const c of comps) {
        const row = buildRow(c);
        rows.set(c.id, row);
        renderRow(c, row);
        list.append(row.li);
      }
    }
  }

  /** Downloaded bytes against what the heading promised: resumed files count from where they stopped, reused files not at all. */
  function renderOverall(): void {
    let total = 0, have = 0, speed = 0, checked = 0, toCheck = 0;
    for (const p of progress.values()) {
      const finished = p.state === 'done' || p.state === 'verifying' || p.state === 'unpacking';
      if (reused.has(p.id)) { toCheck += p.total; checked += finished ? p.total : p.bytes; continue; }
      const start = startBytes.get(p.id) ?? 0;
      total += p.total - start;
      have += finished ? p.total - start : Math.max(0, p.bytes - start);
      if (p.state === 'downloading') speed += p.speedBps;
    }
    const bar = $<HTMLProgressElement>('dl-overall-bar');
    if (!total) {
      bar.value = toCheck ? checked / toCheck : 0;
      $('dl-overall-text').textContent = 'Checking the files found on this PC…';
      return;
    }
    bar.value = have / total;
    const eta = speed > 0 ? `about ${fmtDuration((total - have) / speed)} left` : '';
    $('dl-overall-text').textContent = [`${fmtBytes(have)} of ${fmtBytes(total)} downloaded`, speed > 0 ? fmtSpeed(speed) : '', eta].filter(Boolean).join(' · ');
  }

  async function refresh(): Promise<void> {
    if (running) return;
    try {
      status = await api.modelsStatus();
      renderHead();
      renderList();
    } catch (e) {
      say(msg, `Could not read the model status: ${errorText(e)}`, 'err');
    }
  }

  void events.downloadProgress(p => {
    if (!running) { running = true; renderHead(); }
    if (p.state === 'downloading') reused.delete(p.id); // a found file that did not match is downloaded after all
    progress.set(p.id, p);
    const c = status?.components.find(x => x.id === p.id), row = rows.get(p.id);
    if (c && row) renderRow(c, row);
    renderOverall();
  });
  void events.downloadDone(d => {
    running = false;
    cancel.disabled = false;
    if (d.ok) say(msg, 'Everything is installed. Open a .jai file in View, or encode an image.', 'ok');
    else if (/cancel/i.test(d.error ?? '')) say(msg, 'Download canceled. Partly downloaded files are kept; the next download resumes them.', 'warn');
    else say(msg, `Download failed: ${d.error ?? 'unknown error'}. Downloading again resumes where it stopped.`, 'err');
    progress.clear();
    ctx.refreshStatus();
    void refresh();
  });

  download.addEventListener('click', async () => {
    if (running) return;
    running = true;
    progress.clear();
    startBytes = new Map(status?.components.map(c => [c.id, c.partialBytes ?? 0]) ?? []);
    reused = new Set(status?.components.filter(c => c.state === 'reusable' && reuse.checked).map(c => c.id) ?? []);
    say(msg, '');
    renderHead();
    renderOverall();
    cancel.focus();
    try {
      await api.downloadStart(reuse.checked);
    } catch (e) {
      running = false;
      say(msg, `Could not start the download: ${errorText(e)}`, 'err');
      renderHead();
      download.focus();
    }
  });
  cancel.addEventListener('click', async () => {
    cancel.disabled = true;
    say(msg, 'Canceling…');
    try { await api.downloadCancel(); } catch (e) { cancel.disabled = false; say(msg, `Could not cancel: ${errorText(e)}`, 'err'); }
  });
  reuse.addEventListener('change', () => { renderHead(); if (status) for (const c of status.components) { const row = rows.get(c.id); if (row) renderRow(c, row); } });

  // install_vc_runtime waits for the installer and says how it ended; the
  // runtime check then decides. When the runtime may still come (a restart is
  // needed, or there was no installer process to wait for), the tab checks it
  // again for a while, and whenever the tab is shown.
  const vcButton = $<HTMLButtonElement>('vc-install');
  const vcMsg = $('vc-msg');
  const VC_POLL_MS = 3000, VC_WAIT_MS = 15 * 60_000;
  /** The current watch; a new click ends the one before. */
  let vcWatch = 0, vcWatching = false;
  const vcMissing = (): boolean => { const info = ctx.info(); return info !== null && info.os === 'windows' && !info.vcRuntimeOk; };
  /** Reads app_info again; true once the runtime is up to date (the warning then goes). */
  async function vcInstalled(): Promise<boolean> {
    const info = await ctx.reloadInfo({ quiet: true });
    renderHead();
    if (!info?.vcRuntimeOk) return false;
    say(vcMsg, '');
    say(msg, 'The Visual C++ runtime is installed.', 'ok');
    return true;
  }
  async function watchVc(): Promise<void> {
    const watch = ++vcWatch, until = Date.now() + VC_WAIT_MS;
    vcWatching = true;
    try {
      while (Date.now() < until) {
        await new Promise(r => setTimeout(r, VC_POLL_MS));
        if (watch !== vcWatch || await vcInstalled()) return;
      }
      if (watch === vcWatch) say(vcMsg, 'The runtime still looks out of date. If the installer asked you to restart Windows, restart and open JAI Studio again; otherwise install it again.', 'warn');
    } finally {
      if (watch === vcWatch) vcWatching = false;
    }
  }
  vcButton.addEventListener('click', async () => {
    vcButton.disabled = true;
    vcWatch++;
    vcWatching = false;
    say(vcMsg, 'Downloading the Microsoft installer… Windows asks before it installs anything.');
    try {
      const reply = await api.installVcRuntime();
      if (await vcInstalled()) return;
      const note = vcInstallNote(reply);
      say(vcMsg, note.message, note.kind);
      if (note.watch) void watchVc();
    } catch (e) {
      say(vcMsg, `Could not install the runtime: ${errorText(e)}`, 'err');
    } finally {
      vcButton.disabled = false;
    }
  });

  ctx.onShown('models', () => {
    void refresh();
    // The runtime may have been installed outside JAI Studio, or the installer finished later.
    if (vcMissing() && !vcWatching) void vcInstalled();
  });
  void refresh();
}
