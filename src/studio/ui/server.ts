// Server tab (studio/DESIGN.md section 11): the local server's URL and token
// (copy, show, regenerate), engine state, swaps and MTP with Unload models,
// engine logs with an LLM/Image toggle, and the settings form.
import { api, ENGINE_WORDS, engineView, type BackendPref, type Config, type EngineKind, type ServerInfo, type StatusReply } from './api.ts';
import { $, copyText, errorText, say, toast } from './dom.ts';
import type { TabContext } from './main.ts';
import { checkPort } from './rules.ts';

const LOOPBACK = /^(127\.\d+\.\d+\.\d+|::1|\[::1\]|localhost)$/i;
const BACKENDS: Record<string, string> = { cuda: 'CUDA (NVIDIA)', vulkan: 'Vulkan', cpu: 'CPU' };

export function initServer(ctx: TabContext): void {
  const url = $<HTMLInputElement>('srv-url'), token = $<HTMLInputElement>('srv-token'), showToken = $<HTMLButtonElement>('srv-token-show');
  const confirm = $('srv-regen-confirm'), regen = $<HTMLButtonElement>('srv-token-regen');
  const logs = $('srv-logs'), logsBox = $<HTMLDetailsElement>('srv-logs-box');
  const form = $<HTMLFormElement>('settings-form'), setMsg = $('set-msg');
  const f = {
    idle: $<HTMLInputElement>('set-idle'), tray: $<HTMLInputElement>('set-tray'), login: $<HTMLInputElement>('set-login'),
    assoc: $<HTMLInputElement>('set-assoc'), port: $<HTMLInputElement>('set-port'), backend: $<HTMLSelectElement>('set-backend'),
  };
  let server: ServerInfo | null = null;
  let config: Config | null = null;
  let logKind: EngineKind = 'llm';
  let lastStatus: StatusReply | null = null;

  async function loadServer(): Promise<void> {
    try {
      server = await api.serverInfo();
      url.value = server.url;
      token.value = server.token;
      say($('srv-state'), server.running ? `Running at ${server.url}` : 'Not running', server.running ? 'ok' : 'warn');
      // Why the server is not running, or which other JAI process runs it.
      const note = $('srv-note');
      note.hidden = !server.note;
      note.textContent = server.note ?? '';
      note.classList.toggle('warn', !server.running);
      $('srv-lan').hidden = LOOPBACK.test(server.host);
      $('srv-lan-host').textContent = server.host;
    } catch (e) {
      say($('srv-state'), `Could not read the server details: ${errorText(e)}`, 'err');
    }
  }

  function fillSettings(c: Config): void {
    f.idle.value = String(c.idleUnloadMinutes);
    f.tray.checked = c.app.closeToTray;
    f.login.checked = c.app.startAtLogin;
    f.assoc.checked = c.app.fileAssociation;
    f.port.value = String(c.server.port);
    f.backend.value = c.backend;
  }

  async function loadSettings(): Promise<void> {
    try {
      config = await api.settingsGet();
      fillSettings(config);
    } catch (e) {
      say(setMsg, `Could not read the settings: ${errorText(e)}`, 'err');
    }
  }

  function renderEngines(s: StatusReply): void {
    lastStatus = s;
    const e = s.engines;
    for (const kind of ['llm', 'image'] as const) {
      const v = engineView(e, kind);
      $(`srv-${kind}`).dataset.state = v;
      $(`srv-${kind}-state`).textContent = s.installed ? ENGINE_WORDS[v] : 'not installed';
    }
    $('srv-swaps').textContent = String(e.swaps);
    const wanted = config?.llm.mtp ?? true;
    $('srv-mtp').textContent = e.mtp ? 'On: multi-token prediction drafts tokens ahead'
      : wanted ? 'Off for this session: the LLM did not start with it' : 'Off';
    $('srv-requests').textContent = `${e.active} running · ${e.waitingLlm + e.waitingImage} waiting`;
    const err = $('srv-error');
    err.hidden = !e.lastError;
    err.textContent = e.lastError ? `Last error: ${e.lastError}` : '';
    $<HTMLButtonElement>('srv-unload').disabled = !e.loaded;
  }

  function renderInfo(): void {
    const info = ctx.info();
    if (!info) return;
    $('srv-gpu').textContent = info.gpu ? `${info.gpu.name}, ${(info.gpu.vramTotalMb / 1024).toFixed(0)} GB` : 'No supported GPU found';
    $('srv-backend').textContent = BACKENDS[info.backend] ?? info.backend;
    const cuda = $('srv-cuda-note');
    cuda.hidden = !info.cudaNote;
    cuda.textContent = info.cudaNote ? `Not CUDA: ${info.cudaNote}.` : '';  // the automatic backend's reason
    const problem = $('set-config-problem');
    problem.hidden = !info.configProblem;
    problem.textContent = info.configProblem ? `${info.configProblem}. Saving the settings writes a new config.json.` : '';
    $('srv-version').textContent = `JAI Studio ${info.version} · data in ${info.dataDir}`;
    const windows = info.os === 'windows';
    for (const [input, row] of [[f.login, 'set-login-row'], [f.assoc, 'set-assoc-row']] as const) {
      input.disabled = !windows;
      $(row).classList.toggle('unavailable', !windows);
    }
    $('set-windows-only').hidden = windows;
  }

  async function loadLogs(): Promise<void> {
    if (!logsBox.open || !ctx.isShown('server')) return;
    try {
      const lines = await api.engineLogs(logKind);
      const atEnd = logs.scrollTop + logs.clientHeight >= logs.scrollHeight - 4;
      logs.textContent = lines.length ? lines.join('\n') : `No output from the ${logKind === 'llm' ? 'LLM' : 'image'} engine yet. It starts when a request needs it.`;
      if (atEnd) logs.scrollTop = logs.scrollHeight;
    } catch (e) {
      logs.textContent = `Could not read the logs: ${errorText(e)}`;
    }
  }

  const copy = (what: string, value: string): void => {
    copyText(value).then(() => toast(`Copied the ${what}`), e => toast(errorText(e), 'err'));
  };
  $('srv-url-copy').addEventListener('click', () => copy('URL', url.value));
  $('srv-token-copy').addEventListener('click', () => copy('token', token.value));
  showToken.addEventListener('click', () => {
    const show = token.type === 'password';
    token.type = show ? 'text' : 'password';
    showToken.setAttribute('aria-pressed', String(show));
    showToken.textContent = show ? 'Hide' : 'Show';
  });
  regen.addEventListener('click', () => { confirm.hidden = false; $('srv-regen-yes').focus(); });
  $('srv-regen-no').addEventListener('click', () => { confirm.hidden = true; regen.focus(); });
  $('srv-regen-yes').addEventListener('click', async () => {
    try {
      const r = await api.regenerateToken();
      token.value = r.token;
      confirm.hidden = true;
      regen.focus();
      toast('New token made. Paste it into the apps that use this server.');
    } catch (e) {
      toast(`Could not make a new token: ${errorText(e)}`, 'err');
    }
  });
  $('srv-unload').addEventListener('click', async () => {
    try { await api.unloadModels(); toast('The models are unloading; video memory is freed when they stop.'); } catch (e) { toast(`Could not unload: ${errorText(e)}`, 'err'); }
    ctx.refreshStatus();
  });
  for (const r of document.querySelectorAll<HTMLInputElement>('input[name="srv-log-kind"]')) {
    r.addEventListener('change', () => { if (r.checked) { logKind = r.value === 'image' ? 'image' : 'llm'; logs.textContent = ''; void loadLogs(); } });
  }
  logsBox.addEventListener('toggle', () => void loadLogs());
  $('srv-logs-refresh').addEventListener('click', () => void loadLogs());

  // The open-source licences, read once when first opened (about 300 KB of text).
  const licensesBox = $<HTMLDetailsElement>('srv-licenses-box'), licenses = $('srv-licenses');
  let licensesRead = false;
  licensesBox.addEventListener('toggle', async () => {
    if (!licensesBox.open || licensesRead) return;
    licensesRead = true;
    licenses.textContent = 'Reading…';
    try {
      licenses.textContent = await api.thirdPartyNotices();
    } catch (e) {
      licensesRead = false;
      licenses.textContent = `Could not read the licences: ${errorText(e)}`;
    }
  });

  form.addEventListener('submit', async e => {
    e.preventDefault();
    if (!config) return;
    for (const i of [f.idle, f.port]) i.removeAttribute('aria-invalid');
    const idle = Number(f.idle.value), port = checkPort(f.port.value, config.server.port);
    if (!f.idle.value.trim() || !Number.isInteger(idle) || idle < 0 || idle > 1440) { f.idle.setAttribute('aria-invalid', 'true'); f.idle.focus(); say(setMsg, 'Unload after: a whole number of minutes from 0 to 1440.', 'err'); return; }
    if (port === null) { f.port.setAttribute('aria-invalid', 'true'); f.port.focus(); say(setMsg, 'Port: a whole number from 1024 to 65535.', 'err'); return; }
    const next: Config = {
      ...config, idleUnloadMinutes: idle, backend: f.backend.value as BackendPref, server: { ...config.server, port },
      app: { ...config.app, closeToTray: f.tray.checked, startAtLogin: f.login.checked, fileAssociation: f.assoc.checked },
    };
    const before = config;
    say(setMsg, 'Saving…');
    try {
      config = await api.settingsSet(next);
      fillSettings(config);
      const moved = config.server.port !== before.server.port;
      const backend = config.backend !== before.backend;
      say(setMsg, ['Saved.', moved ? `The server moved to port ${config.server.port}.` : '', backend ? 'The new backend applies the next time an engine starts; check the Models tab for its download.' : ''].filter(Boolean).join(' '), 'ok');
      if (moved) await loadServer();
      if (lastStatus) renderEngines(lastStatus);
    } catch (err) {
      say(setMsg, `Could not save: ${errorText(err)}`, 'err');
    }
  });

  ctx.onStatus(renderEngines);
  ctx.onShown('server', () => { renderInfo(); void loadServer(); void loadLogs(); });
  setInterval(() => void loadLogs(), 2000);
  renderInfo();
  void loadServer();
  void loadSettings();
}
