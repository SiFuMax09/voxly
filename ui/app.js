const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

if (/android|iphone|ipad/i.test(navigator.userAgent)) document.body.classList.add('mobile');

let settings = null;

// ---- recording ----------------------------------------------------------
const labels = { idle: 'Record', recording: 'Stop', transcribing: '…' };
function setStatus(s) {
  $('rec').classList.toggle('recording', s === 'recording');
  $('rec').disabled = s === 'transcribing';
  $('rec-label').textContent = labels[s];
  $('status').textContent = { idle: 'Idle', recording: 'Recording…', transcribing: 'Transcribing…' }[s];
  if (s !== 'idle') $('error').hidden = true;
}
$('rec').onclick = () => invoke('toggle');
$('paste-last').onclick = () => invoke('paste_last_cmd');
listen('status', (e) => setStatus(e.payload));
listen('transcript', (e) => { $('last').value = e.payload; });
listen('error', (e) => { $('error').textContent = e.payload; $('error').hidden = false; });

// ---- vocabulary ---------------------------------------------------------
function termRow(t = { term: '', sounds_like: [] }) {
  const row = document.createElement('div');
  row.className = 'term';
  row.innerHTML = '<input class="t" placeholder="useState"><input class="s" placeholder="sounds like: use state, youth state"><button type="button" aria-label="Remove">×</button>';
  row.querySelector('.t').value = t.term;
  row.querySelector('.s').value = t.sounds_like.join(', ');
  row.querySelector('button').onclick = () => { row.remove(); scheduleSave(); };
  $('terms').appendChild(row);
}
$('add-term').onclick = () => termRow();

// ---- hotkey capture -----------------------------------------------------
const isMac = /Mac/i.test(navigator.platform || navigator.userAgent);
const MODS = {
  CommandOrControl: isMac ? '⌘' : 'Ctrl', CmdOrCtrl: isMac ? '⌘' : 'Ctrl',
  Command: '⌘', Cmd: '⌘', Super: isMac ? '⌘' : 'Win', Meta: isMac ? '⌘' : 'Win',
  Control: isMac ? '⌃' : 'Ctrl', Ctrl: isMac ? '⌃' : 'Ctrl',
  Alt: isMac ? '⌥' : 'Alt', Option: '⌥', Shift: isMac ? '⇧' : 'Shift',
};
const KEYS = {
  Space: 'Space', Enter: '↩', Escape: '⎋', Backspace: '⌫', Delete: '⌦', Tab: '⇥',
  ArrowUp: '↑', ArrowDown: '↓', ArrowLeft: '←', ArrowRight: '→',
  Minus: '-', Equal: '=', Comma: ',', Period: '.', Slash: '/', Backslash: '\\',
  Semicolon: ';', Quote: "'", Backquote: '`', BracketLeft: '[', BracketRight: ']',
};
function keyLabel(k) {
  return KEYS[k] ?? k.replace(/^Key/, '').replace(/^Digit/, '').replace(/^Numpad/, 'Num ');
}
function setHotkey(el, value) {
  el.dataset.value = value || '';
  el.classList.toggle('empty', !value);
  el.replaceChildren(...(value ? value.split('+') : []).map((p) => {
    const k = document.createElement('kbd');
    k.textContent = MODS[p] ?? keyLabel(p);
    return k;
  }));
}
// Plugin accepts these names. On macOS Cmd and Ctrl are different keys.
function combo(e) {
  if (['Control', 'Shift', 'Alt', 'Meta'].includes(e.key)) return null;
  const mods = [];
  if (isMac) {
    if (e.metaKey) mods.push('Command');
    if (e.ctrlKey) mods.push('Control');
  } else {
    if (e.ctrlKey) mods.push('Control');
    if (e.metaKey) mods.push('Super');
  }
  if (e.altKey) mods.push('Alt');
  if (e.shiftKey) mods.push('Shift');
  return mods.length ? [...mods, e.code].join('+') : null;
}
document.querySelectorAll('.hotkey').forEach((el) => {
  el.addEventListener('keydown', (e) => {
    e.preventDefault();
    if (e.key === 'Backspace' || e.key === 'Delete') { setHotkey(el, ''); return scheduleSave(); }
    const c = combo(e);
    if (c) { setHotkey(el, c); scheduleSave(); }
  });
});

// ---- local models -------------------------------------------------------
let models = [];
async function refreshModels() {
  try { models = await invoke('list_models'); } catch { models = []; }
  const sel = $('local-model');
  const cur = sel.value || settings?.local_model;
  sel.replaceChildren(...models.map((m) => new Option(m.label + (m.downloaded ? ' ✓' : ''), m.id)));
  sel.value = cur;
  updateBackendUi();
}
function updateBackendUi() {
  const local = $('backend').value === 'local';
  $('openai-box').hidden = local;
  $('local-box').hidden = !local;
  const m = models.find((x) => x.id === $('local-model').value);
  $('download').hidden = !local || !m || m.downloaded;
}
$('backend').onchange = updateBackendUi;
$('local-model').onchange = updateBackendUi;
$('download').onclick = async () => {
  const id = $('local-model').value;
  $('download').disabled = true;
  $('dl-status').hidden = false;
  $('dl-status').textContent = 'Starting download…';
  try {
    await invoke('download_model', { id });
    $('dl-status').hidden = true;
  } catch (err) {
    $('dl-status').textContent = 'Download failed: ' + err;
  }
  $('download').disabled = false;
  await refreshModels();
};
listen('download', (e) => {
  const [done, total] = e.payload;
  const mb = (n) => (n / 1048576).toFixed(0);
  $('dl-status').textContent = total ? `Downloading ${mb(done)} / ${mb(total)} MB (${Math.round((done / total) * 100)}%)` : `Downloading ${mb(done)} MB`;
});

// ---- load / save --------------------------------------------------------
function render() {
  $('backend').value = settings.backend;
  $('local-model').value = settings.local_model;
  $('api-key').value = settings.api_key;
  $('model').value = settings.model;
  $('language').value = settings.language;
  setHotkey($('hk-hold'), settings.hotkeys.hold);
  setHotkey($('hk-toggle'), settings.hotkeys.toggle);
  setHotkey($('hk-paste'), settings.hotkeys.paste_last);
  $('auto-paste').checked = settings.auto_paste;
  $('terms').replaceChildren();
  settings.vocabulary.forEach(termRow);
  if (!settings.vocabulary.length) termRow();
}

function collect() {
  const vocabulary = [...document.querySelectorAll('.term')]
    .map((r) => ({
      term: r.querySelector('.t').value.trim(),
      sounds_like: r.querySelector('.s').value.split(',').map((s) => s.trim()).filter(Boolean),
    }))
    .filter((t) => t.term);
  return {
    backend: $('backend').value,
    local_model: $('local-model').value || 'base',
    api_key: $('api-key').value.trim(),
    model: $('model').value,
    language: $('language').value.trim(),
    vocabulary,
    hotkeys: { hold: $('hk-hold').dataset.value, toggle: $('hk-toggle').dataset.value, paste_last: $('hk-paste').dataset.value },
    auto_paste: $('auto-paste').checked,
  };
}

// Every change is saved automatically, so what you see is what the app uses.
let saveTimer;
function scheduleSave() {
  if (!settings) return;
  clearTimeout(saveTimer);
  saveTimer = setTimeout(save, 400);
}
async function save() {
  const next = collect();
  try {
    await invoke('save_settings', { settings: next });
    settings = next;
    $('error').hidden = true;
    $('saved').hidden = false;
    clearTimeout(save.hide);
    save.hide = setTimeout(() => ($('saved').hidden = true), 1500);
  } catch (err) {
    $('error').textContent = String(err);
    $('error').hidden = false;
  }
}
document.querySelector('main').addEventListener('input', scheduleSave);
document.querySelector('main').addEventListener('change', scheduleSave);

(async () => {
  settings = await invoke('get_settings');
  render();
  await refreshModels();
  $('last').value = await invoke('get_last');
  setStatus('idle');
})();
