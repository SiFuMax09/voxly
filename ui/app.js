const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

if (/android|iphone|ipad/i.test(navigator.userAgent)) document.body.classList.add('mobile');

let settings = null;

// ---- tabs ---------------------------------------------------------------
function showTab(name) {
  document.querySelectorAll('#tabs button').forEach((b) => b.setAttribute('aria-selected', String(b.dataset.tab === name)));
  document.querySelectorAll('.tab').forEach((t) => (t.hidden = t.id !== `tab-${name}`));
  try { localStorage.setItem('tab', name); } catch {}
  if (name === 'usage') renderUsage();
  if (name === 'history') renderHistory();
}
document.querySelectorAll('#tabs button').forEach((b) => (b.onclick = () => showTab(b.dataset.tab)));

// ---- history ------------------------------------------------------------
let history = { entries: [] };
function renderHistory() {
  const q = $('history-search').value.trim().toLowerCase();
  const list = $('history');
  const shown = history.entries.filter((e) => !q || e.text.toLowerCase().includes(q)).reverse();
  list.replaceChildren(...shown.map((e) => {
    const row = document.createElement('div');
    row.className = 'entry';
    const meta = document.createElement('div');
    meta.className = 'entry-meta';
    const when = document.createElement('span');
    when.className = 'grow';
    const words = e.text.trim().split(/\s+/).length;
    when.textContent = `${new Date(e.id).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' })} · ${words} word${words === 1 ? '' : 's'}`;
    const copy = document.createElement('button');
    copy.type = 'button';
    copy.textContent = 'Copy';
    copy.onclick = async () => {
      try { await navigator.clipboard.writeText(e.text); copy.textContent = 'Copied'; } catch { copy.textContent = 'Failed'; }
      setTimeout(() => (copy.textContent = 'Copy'), 1200);
    };
    const del = document.createElement('button');
    del.type = 'button';
    del.className = 'del';
    del.textContent = 'Delete';
    del.onclick = async () => { history = await invoke('delete_history_entry', { id: e.id }); renderHistory(); };
    meta.append(when, copy, del);
    const text = document.createElement('p');
    text.className = 'entry-text';
    text.textContent = e.text;
    row.append(meta, text);
    return row;
  }));
  $('history-empty').hidden = shown.length > 0;
  $('history-empty').textContent = q ? 'No matches.' : 'Nothing here yet. Your dictations show up here.';
  $('history-clear').hidden = history.entries.length === 0;
}
$('history-search').addEventListener('input', renderHistory);
let clearArmed;
$('history-clear').onclick = async () => {
  if (!clearArmed) {
    $('history-clear').textContent = 'Click again to delete everything';
    clearArmed = setTimeout(() => { clearArmed = null; $('history-clear').textContent = 'Clear history'; }, 3000);
    return;
  }
  clearTimeout(clearArmed);
  clearArmed = null;
  $('history-clear').textContent = 'Clear history';
  history = await invoke('clear_history');
  renderHistory();
};
listen('history', (e) => { history = e.payload; renderHistory(); });

// ---- usage --------------------------------------------------------------
let usage = { days: {} };
const dayKey = (d) => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
const fmt = (n) => n.toLocaleString();
const WEEKS = 26;

function renderUsage() {
  const days = usage.days || {};
  const today = new Date();
  today.setHours(12, 0, 0, 0);
  const at = (offset) => { const d = new Date(today); d.setDate(d.getDate() - offset); return d; };
  const words = (d) => days[dayKey(d)]?.words ?? 0;

  // Headline numbers.
  let week = 0;
  for (let i = 0; i < 7; i++) week += words(at(i));
  const all = Object.values(days);
  const total = all.reduce((a, d) => a + d.words, 0);
  const recs = all.reduce((a, d) => a + d.recordings, 0);
  const secs = all.reduce((a, d) => a + d.seconds, 0);
  const active = all.filter((d) => d.words > 0).length;
  // A streak survives until the end of today, so start from yesterday if today is still empty.
  let streak = 0;
  for (let i = words(at(0)) > 0 ? 0 : 1; words(at(i)) > 0; i++) streak++;
  $('st-today').textContent = fmt(words(at(0)));
  $('st-week').textContent = fmt(week);
  $('st-total').textContent = fmt(total);
  $('st-streak').textContent = fmt(streak);
  $('st-rec').textContent = fmt(recs);
  $('st-time').textContent = secs >= 3600 ? `${(secs / 3600).toFixed(1)} h` : `${Math.round(secs / 60)} min`;
  $('st-avg').textContent = fmt(active ? Math.round(total / active) : 0);
  $('st-wpm').textContent = fmt(secs >= 10 ? Math.round(total / (secs / 60)) : 0);

  // Time and money saved.
  const wpm = Math.max(5, Number($('typing-wpm').value) || 40);
  const typing = (total / wpm) * 60;
  const dur = (sec) => (sec >= 3600 ? `${(sec / 3600).toFixed(1)} h` : `${Math.round(sec / 60)} min`);
  $('st-typing').textContent = dur(typing);
  $('st-saved').textContent = dur(Math.max(0, typing - secs));
  const perMin = ($('model').value || '').includes('mini') ? 0.003 : 0.006;
  $('st-cloud').textContent = `$${((secs / 60) * perMin).toFixed(2)}`;

  // Heatmap: columns are weeks (Monday first), rows are weekdays.
  const dow = (today.getDay() + 6) % 7; // Monday = 0
  const start = at(dow + (WEEKS - 1) * 7);
  const max = Math.max(0, ...Object.values(days).map((d) => d.words));
  const level = (w) => (w <= 0 ? 0 : Math.min(4, Math.ceil((w / max) * 4)));
  const heat = $('heat');
  const months = $('heat-months');
  heat.replaceChildren();
  months.replaceChildren();
  let lastMonth = -1;
  for (let i = 0; i < WEEKS * 7; i++) {
    const d = new Date(start);
    d.setDate(start.getDate() + i);
    const cell = document.createElement('i');
    const w = words(d);
    if (d > today) cell.className = 'future';
    else {
      cell.className = `l${level(w)}`;
      const label = `${d.toLocaleDateString(undefined, { weekday: 'short', day: 'numeric', month: 'short', year: 'numeric' })}: ${fmt(w)} word${w === 1 ? '' : 's'}`;
      cell.title = label;
      cell.dataset.label = label;
    }
    heat.appendChild(cell);
    if (i % 7 === 0 && d.getMonth() !== lastMonth) {
      lastMonth = d.getMonth();
      const m = document.createElement('span');
      m.textContent = d.toLocaleDateString(undefined, { month: 'short' });
      m.style.left = `${(i / 7) * 16}px`;
      months.appendChild(m);
    }
  }
  months.style.width = `${WEEKS * 16}px`;
  $('heat-detail').textContent = 'Hover or tap a day.';
}
$('heat').addEventListener('click', (e) => {
  const cell = e.target.closest('i');
  if (!cell?.dataset.label) return;
  document.querySelectorAll('#heat i.sel').forEach((c) => c.classList.remove('sel'));
  cell.classList.add('sel');
  $('heat-detail').textContent = cell.dataset.label;
});
$('heat').addEventListener('mouseover', (e) => {
  const cell = e.target.closest('i');
  if (cell?.dataset.label) $('heat-detail').textContent = cell.dataset.label;
});
listen('usage', (e) => { usage = e.payload; renderUsage(); });

// ---- recording ----------------------------------------------------------
const labels = { idle: 'Record', recording: 'Stop', transcribing: '…' };
function setStatus(s) {
  $('rec').classList.toggle('recording', s === 'recording');
  $('rec').disabled = s === 'transcribing';
  $('rec-label').textContent = labels[s];
  $('status').textContent = { idle: 'Idle', recording: 'Recording…', transcribing: 'Transcribing…' }[s];
  if (s !== 'idle') $('error').hidden = true;
  if (s === 'recording') $('notice').hidden = true;
}
$('rec').onclick = () => invoke('toggle');
$('paste-last').onclick = () => invoke('paste_last_cmd');
listen('status', (e) => setStatus(e.payload));
listen('notice', (e) => { $('notice').textContent = e.payload; $('notice').hidden = false; });
listen('timing', (e) => {
  const { ms, audio_s, backend } = e.payload;
  $('timing').textContent = `Transcribed ${audio_s} s of audio in ${(ms / 1000).toFixed(2)} s (${backend})`;
  $('timing').hidden = false;
});
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

// ---- hotkey access (Wayland) ---------------------------------------------
async function checkHotkeyAccess() {
  try { $('hk-access').hidden = (await invoke('hotkey_access')) === 'ok'; } catch {}
}
$('hk-grant').onclick = async () => {
  $('hk-grant').disabled = true;
  $('error').hidden = true;
  try {
    await invoke('grant_hotkey_access');
  } catch (e) {
    $('error').textContent = String(e);
    $('error').hidden = false;
  }
  $('hk-grant').disabled = false;
  checkHotkeyAccess();
};

// ---- compare mode -------------------------------------------------------
function cmpSide(d, fastest) {
  const box = document.createElement('div');
  box.className = 'cmp-side' + (fastest ? ' fastest' : '');
  const h = document.createElement('div');
  h.className = 'cmp-head';
  h.textContent = d.label;
  const t = document.createElement('div');
  t.className = 'cmp-time';
  t.textContent = d.error ? 'failed' : `${(d.ms / 1000).toFixed(2)} s`;
  const body = document.createElement('div');
  body.className = d.error ? 'cmp-text error-text' : 'cmp-text';
  body.textContent = d.error || d.text || '(empty)';
  box.append(h, t, body);
  return box;
}
listen('compare', (e) => {
  const { audio_s, similarity, local, cloud } = e.payload;
  $('cmp-section').hidden = false;
  const card = document.createElement('div');
  card.className = 'cmp-card';
  const ok = !local.error && !cloud.error;
  const row = document.createElement('div');
  row.className = 'cmp-row';
  row.append(cmpSide(local, ok && local.ms <= cloud.ms), cmpSide(cloud, ok && cloud.ms < local.ms));
  const foot = document.createElement('p');
  foot.className = 'hint';
  foot.textContent = `${audio_s} s of audio` +
    (similarity != null ? ` · texts match ${Math.round(similarity * 100)}%` : '');
  card.append(row, foot);
  $('cmp-list').prepend(card);
  while ($('cmp-list').children.length > 10) $('cmp-list').lastChild.remove();
});

// ---- microphone ---------------------------------------------------------
async function loadMics() {
  let devices = [];
  try { devices = await invoke('list_input_devices'); } catch {}
  const saved = settings?.input_device || '';
  // Keep a saved microphone selectable even while it is unplugged.
  if (saved && !devices.some((d) => d.id === saved)) devices.push({ id: saved, label: `${saved} (not connected)` });
  $('mic').replaceChildren(new Option('System default', ''), ...devices.map((d) => new Option(d.label, d.id)));
  $('mic').value = saved;
}

// ---- speed test ---------------------------------------------------------
$('tune').onclick = async () => {
  const log = $('tune-log');
  log.replaceChildren();
  log.hidden = false;
  $('tune').disabled = true;
  $('error').hidden = true;
  try {
    const r = await invoke('tune_speed');
    const li = document.createElement('li');
    li.textContent = `Fastest: ${r.best} (${(Math.min(...r.trials.filter((t) => t.similar).map((t) => t.ms)) / 1000).toFixed(2)} s). Applied.`;
    li.style.fontWeight = '600';
    log.append(li);
    settings = await invoke('get_settings');
    render();
  } catch (e) {
    $('error').textContent = String(e);
    $('error').hidden = false;
  }
  $('tune').disabled = false;
};
listen('tune', (e) => {
  const li = document.createElement('li');
  li.textContent = e.payload;
  $('tune-log').append(li);
});
listen('tune_trial', (e) => {
  const t = e.payload;
  const li = document.createElement('li');
  li.textContent = `${t.label}: ${(t.ms / 1000).toFixed(2)} s${t.similar ? '' : ' (text differs, skipped)'}`;
  $('tune-log').append(li);
});

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
  // Function keys work on their own, which is handy for hold-to-talk: no
  // modifier is held down while text is typed into the focused app.
  if (!mods.length && /^F([1-9]|1\d|2[0-4])$/.test(e.code)) return e.code;
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
  // The language is always forced; never fall back to Whisper's auto-detect.
  const sel = $('language');
  const wanted = settings.language || navigator.language.slice(0, 2);
  sel.value = [...sel.options].some((o) => o.value === wanted) ? wanted : 'en';
  if (sel.value !== settings.language) scheduleSave();
  setHotkey($('hk-hold'), settings.hotkeys.hold);
  setHotkey($('hk-toggle'), settings.hotkeys.toggle);
  setHotkey($('hk-paste'), settings.hotkeys.paste_last);
  $('auto-paste').checked = settings.auto_paste;
  $('compare-mode').checked = !!settings.compare_mode;
  $('trim-silence').checked = settings.trim_silence !== false;
  $('voice-commands').checked = settings.voice_commands !== false;
  $('streaming').checked = settings.streaming !== false;
  $('typing-wpm').value = settings.typing_wpm || 40;
  if (!$('tab-usage').hidden) renderUsage();
  $('mic').value = settings.input_device || '';
  $('use-gpu').checked = settings.use_gpu !== false;
  $('overlay-position').value = settings.overlay_position || 'top';
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
    compare_mode: $('compare-mode').checked,
    trim_silence: $('trim-silence').checked,
    voice_commands: $('voice-commands').checked,
    streaming: $('streaming').checked,
    typing_wpm: Math.max(5, Number($('typing-wpm').value) || 40),
    input_device: $('mic').value,
    use_gpu: $('use-gpu').checked,
    overlay_position: $('overlay-position').value,
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
$('typing-wpm').addEventListener('input', () => { if (usage) renderUsage(); });

(async () => {
  settings = await invoke('get_settings');
  await loadMics();
  render();
  checkHotkeyAccess();
  await refreshModels();
  $('last').value = await invoke('get_last');
  usage = await invoke('get_usage');
  history = await invoke('get_history');
  let tab = 'home';
  try { tab = localStorage.getItem('tab') || 'home'; } catch {}
  showTab(document.getElementById(`tab-${tab}`) ? tab : 'home');
  setStatus('idle');
})();
