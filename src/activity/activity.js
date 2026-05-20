import { invoke } from '@tauri-apps/api/core';
import { initI18n, t } from '../i18n/index.js';

const MAX_LOG_ENTRIES = 300;
let pollTimer = null;
let windowOpenTs = Date.now();

// ログ行管理: file_name → { el: HTMLElement, completed: boolean }
const logEntryMap = new Map();
// 適用済みログエントリ数（visible 配列内の index）
let appliedLogCount = 0;
// 設定
let settings = null;
// 前回の is_converting フラグ
let prevIsConverting = false;

// --- Init ---
async function init() {
  await initI18n();
  applyI18n();

  // 設定を読み込む
  try {
    settings = await invoke('load_settings');
  } catch (_) {
    settings = { clearLogOnConvert: true };
  }

  // クリアボタンの表示制御
  const clearBtn = document.getElementById('clear-btn');
  if (settings.clearLogOnConvert) {
    clearBtn.style.display = 'none';
  } else {
    clearBtn.style.display = '';
  }

  clearBtn.addEventListener('click', () => {
    document.getElementById('log-list').innerHTML =
      `<div id="log-empty" class="log-empty">${t('activity.logEmpty')}</div>`;
    windowOpenTs = Date.now();
    appliedLogCount = 0;
    logEntryMap.clear();
  });

  startPolling();
}

function applyI18n() {
  document.querySelectorAll('[data-i18n]').forEach(el => {
    const key = el.dataset.i18n;
    const text = t(key);
    if (text) el.textContent = text;
  });
  document.title = t('activity.title') || 'Activity';
}

// --- Polling ---
function startPolling() {
  poll();
  pollTimer = setInterval(poll, 200);
}

async function poll() {
  try {
    const data = await invoke('get_activity_data');

    // is_converting フラグの遷移を検知して自動クリア
    const isConverting = data.is_converting ?? false;
    if (!prevIsConverting && isConverting && settings?.clearLogOnConvert) {
      document.getElementById('log-list').innerHTML =
        `<div id="log-empty" class="log-empty">${t('activity.logEmpty')}</div>`;
      windowOpenTs = Date.now();
      appliedLogCount = 0;
      logEntryMap.clear();
    }
    prevIsConverting = isConverting;

    updateMetrics(data);
    updateLog(data.log ?? [], data.active_files ?? {});
  } catch (_) {}
}

// --- Metrics ---
function updateMetrics({ cpu_percent, memory_used_mb, memory_peak_mb, memory_budget_mb, is_network, is_converting }) {
  // CPU
  const cpuEl  = document.getElementById('cpu-value');
  const cpuBar = document.getElementById('cpu-bar');
  const cpuPct = Math.min(cpu_percent ?? 0, 999.9);
  cpuEl.textContent = cpuPct.toFixed(1) + '%';
  cpuBar.style.width = Math.min(cpuPct, 100) + '%';
  cpuBar.className = 'metric-bar' +
    (cpuPct >= 80 ? ' danger' : cpuPct >= 50 ? ' warn' : '');

  // Memory
  const memEl   = document.getElementById('mem-value');
  const memBar  = document.getElementById('mem-bar');
  const memHint = document.getElementById('mem-hint');
  const memPeak = document.getElementById('mem-peak');

  const budget = memory_budget_mb ?? 0;
  const used   = memory_used_mb   ?? 0;
  const peak   = memory_peak_mb   ?? 0;

  if (!is_converting || !is_network || budget === 0) {
    memEl.textContent = '—';
    memBar.style.width = '0%';
    memBar.className = 'metric-bar';
    if (!is_converting) {
      memHint.textContent = t('activity.memoryInactive');
    } else if (!is_network) {
      memHint.textContent = t('activity.memoryLocal');
    } else {
      memHint.textContent = '';
    }
    memPeak.textContent = '';
  } else {
    const pct = Math.min(used / budget * 100, 100);
    memEl.textContent = used.toFixed(1) + ' MB';
    memBar.style.width = pct.toFixed(1) + '%';
    memBar.className = 'metric-bar' +
      (pct >= 90 ? ' danger' : pct >= 70 ? ' warn' : '');
    memHint.textContent = `/ ${budget} MB`;
    memPeak.textContent = peak > 0 ? `peak: ${peak.toFixed(1)} MB` : '';
  }
}

// --- Log ---
function updateLog(log, active_files) {
  const visible = log.filter(e => e.ts_ms >= windowOpenTs);

  // 新着ログエントリを適用
  if (visible.length > appliedLogCount) {
    const newEntries = visible.slice(appliedLogCount);
    newEntries.forEach(entry => applyLogEntry(entry));
    appliedLogCount = visible.length;
  }

  // アクティブファイルのプログレスバーを更新
  for (const [name, ratio] of Object.entries(active_files)) {
    const item = logEntryMap.get(name);
    if (item && !item.completed) {
      updateProgressBar(item.el, ratio);
    }
  }
}

function applyLogEntry({ ts_ms, file_name, status, error }) {
  const list = document.getElementById('log-list');
  const empty = document.getElementById('log-empty');
  if (empty) empty.remove();

  if (status === 'processing') {
    if (logEntryMap.has(file_name)) return; // 重複は無視

    trimLog(list);
    const el = buildProcessingRow(ts_ms, file_name);
    list.appendChild(el);
    logEntryMap.set(file_name, { el, completed: false });
    list.scrollTop = list.scrollHeight;
  } else {
    // done / error / skipped
    const item = logEntryMap.get(file_name);
    if (item && !item.completed) {
      // インプレース更新
      promoteRow(item.el, ts_ms, status, error);
      item.completed = true;
    } else if (!item) {
      // processing なしで完了が来た場合（稀）
      trimLog(list);
      const el = buildCompletedRow(ts_ms, file_name, status, error);
      list.appendChild(el);
      logEntryMap.set(file_name, { el, completed: true });
      list.scrollTop = list.scrollHeight;
    }
  }
}

// 上限超えたら最古エントリを削除
function trimLog(list) {
  const existing = list.querySelectorAll('.log-entry');
  if (existing.length >= MAX_LOG_ENTRIES) {
    const oldest = existing[0];
    // map からも除去
    for (const [key, val] of logEntryMap) {
      if (val.el === oldest) { logEntryMap.delete(key); break; }
    }
    oldest.remove();
  }
}

// 進捗バーを更新（0.0–1.0）
function updateProgressBar(el, ratio) {
  const fill = el.querySelector('.log-progress-fill');
  const pct  = el.querySelector('.log-pct');
  if (!fill || !pct) return;
  const p = Math.round(ratio * 100);
  fill.style.width = p + '%';
  pct.textContent  = p + '%';
}

// processing → 完了バッジへ昇格
function promoteRow(el, ts_ms, status, error) {
  // タイムスタンプを完了時刻に更新
  const timeEl = el.querySelector('.log-time');
  if (timeEl) timeEl.textContent = formatTime(ts_ms);

  // バッジを置き換え
  const badge = el.querySelector('.log-badge');
  if (badge) {
    badge.className = `log-badge ${status}`;
    badge.textContent = badgeLabel(status);
  }

  // プログレスバーを削除
  const wrap = el.querySelector('.log-progress-wrap');
  if (wrap) wrap.remove();

  // エラーメッセージを追加
  if (error) {
    const errEl = document.createElement('span');
    errEl.className = 'log-error';
    errEl.title = escHtml(error);
    errEl.textContent = error;
    el.appendChild(errEl);
  }
}

// --- Row builders ---
function buildProcessingRow(ts_ms, file_name) {
  const el = document.createElement('div');
  el.className = 'log-entry';
  el.innerHTML = `
    <span class="log-time">${formatTime(ts_ms)}</span>
    <span class="log-badge processing">${escHtml(badgeLabel('processing'))}</span>
    <span class="log-filename" title="${escHtml(file_name)}">${escHtml(file_name)}</span>
    <div class="log-progress-wrap">
      <div class="log-progress-track"><div class="log-progress-fill"></div></div>
      <span class="log-pct">0%</span>
    </div>
  `;
  return el;
}

function buildCompletedRow(ts_ms, file_name, status, error) {
  const el = document.createElement('div');
  el.className = 'log-entry';
  el.innerHTML = `
    <span class="log-time">${formatTime(ts_ms)}</span>
    <span class="log-badge ${escHtml(status)}">${escHtml(badgeLabel(status))}</span>
    <span class="log-filename" title="${escHtml(file_name)}">${escHtml(file_name)}</span>
    ${error ? `<span class="log-error" title="${escHtml(error)}">${escHtml(error)}</span>` : ''}
  `;
  return el;
}

// --- Helpers ---
function badgeLabel(status) {
  return {
    processing: t('activity.statusProcessing'),
    done:       t('activity.statusDone'),
    error:      t('activity.statusError'),
    skipped:    t('activity.statusSkipped'),
  }[status] ?? status;
}

function formatTime(ts_ms) {
  const d = new Date(ts_ms);
  const hh = String(d.getHours()).padStart(2, '0');
  const mm = String(d.getMinutes()).padStart(2, '0');
  const ss = String(d.getSeconds()).padStart(2, '0');
  return `${hh}:${mm}:${ss}`;
}

function escHtml(str) {
  return String(str)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

// --- Cleanup ---
window.addEventListener('unload', () => clearInterval(pollTimer));

init();
