import { invoke } from '@tauri-apps/api/core';
import { initI18n, t } from '../i18n/index.js';
import {
  compensateTrimmedScrollTop,
  isLogAtBottom,
  logEntryAction,
  progressPercent,
  shouldTrimLog,
} from './log-follow.js';

const MAX_LOG_ENTRIES = 10_000;
let pollTimer = null;
let interpolationTimer = null;
let logCursor = 0;
let logStateRevision = null;
let selectedErrorCopyText = '';
let latestActivityData = null;
let etaSamples = [];
let etaJobStart = 0;
let lastEtaProgress = null;
let lastProgressChangeTs = 0;
let autoFollowLog = true;
let mutatingLog = false;

// ログ行管理: file_name → { el: HTMLElement, completed: boolean }
const logEntryMap = new Map();
const logElementKeys = new WeakMap();
// 設定
let settings = null;
// 前回の is_converting フラグ
let prevIsConverting = false;

// --- Init ---
async function init() {
  // 設定を先に読み込む（言語設定をi18n初期化に渡すため）
  try {
    settings = await invoke('get_settings');
  } catch (_) {
    settings = { clearLogOnConvert: true };
  }

  await initI18n(settings?.language || '');

  // クリアボタンは常に表示
  const clearBtn = document.getElementById('clear-btn');
  const logList = document.getElementById('log-list');

  // wheel / trackpad / scrollbar / keyboard are all reflected by this event.
  logList.addEventListener('scroll', () => {
    if (!mutatingLog) autoFollowLog = isLogAtBottom(logList);
  });

  document.getElementById('btn-copy-log-error').addEventListener('click', copySelectedErrorDetails);
  document.addEventListener('click', (event) => {
    if (!document.getElementById('log-copy-menu').contains(event.target)) hideLogCopyMenu();
  });

  clearBtn.addEventListener('click', () => {
    document.getElementById('log-list').innerHTML =
      `<div id="log-empty" class="log-empty">${t('activity.logEmpty')}</div>`;
    logCursor = 0;
    logStateRevision = null;
    logEntryMap.clear();
    resetLogAutoFollow();
    invoke('clear_activity_log').catch(console.error);
  });

  startPolling();
  interpolationTimer = setInterval(renderInterpolatedMetrics, 100);
}

// --- Polling ---
function startPolling() {
  poll();
  pollTimer = setInterval(poll, 200);
}

async function poll() {
  try {
    const data = await invoke('get_activity_data', {
      afterSeq: logCursor,
      knownLogStateRevision: logStateRevision,
    });

    // is_converting フラグの遷移を検知して自動クリア
    const isConverting = data.is_converting ?? false;
    if (!prevIsConverting && isConverting) {
      resetLogAutoFollow();
    }
    if (!prevIsConverting && isConverting && settings?.clearLogOnConvert) {
      document.getElementById('log-list').innerHTML =
        `<div id="log-empty" class="log-empty">${t('activity.logEmpty')}</div>`;
      logCursor = 0;
      logStateRevision = null;
      logEntryMap.clear();
      invoke('clear_activity_log').catch(console.error);
    }
    prevIsConverting = isConverting;

    updateMetrics(data);
    updateLog(data.log ?? [], data.active_files ?? {}, data.log_reset ?? false);
    logCursor = data.log_cursor ?? logCursor;
    logStateRevision = data.log_state_revision ?? logStateRevision;
  } catch (_) {}
}

// --- Metrics ---
function updateMetrics(data) {
  latestActivityData = data;

  const {
    cpu_percent,
    system_cpu_percent,
    input_spool_used_mb,
    input_spool_target_mb,
    output_spool_used_mb,
    input_spool_waiting,
    output_spool_waiting,
    is_converting,
    overall_progress_percent,
    completed_count,
    target_total,
    enumerated_input_count,
    completed_input_count,
    progress_phase,
    scan_complete,
    scanning_batch_count,
    queued_batch_count,
    waiting_count,
    processing_count,
    successful_count,
    failed_count,
    skipped_count,
  } = data;

  // Mac全体の全コア平均CPU使用率
  const cpuEl  = document.getElementById('cpu-value');
  const cpuBar = document.getElementById('cpu-bar');
  const cpuPct = Math.max(0, Math.min(system_cpu_percent ?? cpu_percent ?? 0, 100));
  cpuEl.textContent = cpuPct.toFixed(1) + '%';
  cpuBar.style.width = cpuPct + '%';
  cpuBar.className = 'metric-bar' +
    (cpuPct >= 80 ? ' danger' : cpuPct >= 50 ? ' warn' : '');

  // 入力スプール。1GiBは表示上の目標であり、超過時も実量と割合を維持する。
  const used = input_spool_used_mb ?? 0;
  const target = input_spool_target_mb || 256;
  const inputPct = target > 0 ? used / target * 100 : 0;
  const inputEl = document.getElementById('input-spool-value');
  const inputBar = document.getElementById('input-spool-bar');
  const inputHint = document.getElementById('input-spool-hint');
  inputEl.textContent = `${formatStorage(used)} · ${inputPct.toFixed(1)}%`;
  inputEl.classList.toggle('danger', inputPct > 100);
  inputBar.style.width = Math.min(inputPct, 100) + '%';
  inputBar.className = 'metric-bar' +
    (inputPct > 100 ? ' danger' : inputPct >= 80 ? ' warn' : '');
  inputHint.textContent = input_spool_waiting
    ? t('activity.inputSpoolWaiting')
    : `${t('activity.target')} ${formatStorage(target)}`;

  // 出力スプールは量と待機状態のみ。
  const outputUsed = output_spool_used_mb ?? 0;
  const outputEl = document.getElementById('output-spool-value');
  const outputHint = document.getElementById('output-spool-hint');
  outputEl.textContent = formatStorage(outputUsed);
  outputHint.textContent = output_spool_waiting
    ? t('activity.outputSpoolWaiting')
    : (!is_converting ? t('activity.standby') : t('activity.noWaiting'));

  // 全工程進捗はバックエンドの正本をそのまま表示する。
  const progress = Math.max(0, Math.min(overall_progress_percent ?? 0, 100));
  document.getElementById('overall-percent').textContent = progress.toFixed(1) + '%';
  document.getElementById('overall-progress-bar').style.width = progress + '%';
  const progressTrack = document.querySelector('.overall-progress-track');
  progressTrack.setAttribute('aria-valuenow', progress.toFixed(1));
  const exactProgress = progress_phase === 'exact' || (progress_phase == null && scan_complete);
  const displayedCompleted = exactProgress ? completed_count : completed_input_count;
  const displayedTotal = exactProgress ? target_total : enumerated_input_count;
  const totalText = exactProgress
    ? String(displayedTotal ?? 0)
    : `${displayedTotal ?? 0}${t('activity.detectingSuffix')}`;
  document.getElementById('completed-value').textContent = `${displayedCompleted ?? 0} / ${totalText}`;
  document.getElementById('queue-batches-value').textContent =
    `${queued_batch_count ?? 0}${(scanning_batch_count ?? 0) > 0 ? ` (${scanning_batch_count}…)` : ''}`;
  document.getElementById('queue-waiting-value').textContent = String(waiting_count ?? 0);
  document.getElementById('queue-processing-value').textContent = String(processing_count ?? 0);
  document.getElementById('queue-done-value').textContent = String(successful_count ?? 0);
  document.getElementById('queue-skipped-value').textContent = String(skipped_count ?? 0);
  document.getElementById('queue-failed-value').textContent = String(failed_count ?? 0);

  updateEtaSamples(data);
  renderInterpolatedMetrics();
}

function updateEtaSamples({
  conv_start_ts,
  is_converting,
  progress_phase,
  scan_complete,
  overall_progress_percent,
}) {
  const jobStart = conv_start_ts ?? 0;
  if (jobStart !== etaJobStart) {
    etaJobStart = jobStart;
    etaSamples = [];
    lastEtaProgress = null;
    lastProgressChangeTs = 0;
  }
  const exactProgress = progress_phase === 'exact' || (progress_phase == null && scan_complete);
  if (!is_converting || !exactProgress) {
    etaSamples = [];
    lastEtaProgress = null;
    lastProgressChangeTs = 0;
    return;
  }

  const now = Date.now();
  const progress = Math.max(0, Math.min(overall_progress_percent ?? 0, 100));
  if (lastEtaProgress === null || progress > lastEtaProgress + 0.001) {
    lastProgressChangeTs = now;
  }
  lastEtaProgress = progress;
  etaSamples.push({ ts: now, progress });
  etaSamples = etaSamples.filter(sample => sample.ts >= now - 10_000);
}

function renderInterpolatedMetrics() {
  if (!latestActivityData) return;
  const {
    is_converting,
    conv_start_ts,
    conversion_elapsed_ms,
    progress_phase,
    scan_complete,
    overall_progress_percent,
  } = latestActivityData;
  const elapsedMs = is_converting && conv_start_ts > 0
    ? Math.max(0, Date.now() - conv_start_ts)
    : (conversion_elapsed_ms ?? 0);
  document.getElementById('elapsed-value').textContent = elapsedMs > 0 ? formatElapsed(elapsedMs) : '—';

  const etaEl = document.getElementById('eta-value');
  if (!is_converting && elapsedMs > 0 && (overall_progress_percent ?? 0) >= 100) {
    etaEl.textContent = t('activity.allStagesComplete');
    return;
  }
  if (!is_converting) {
    etaEl.textContent = '—';
    return;
  }
  const exactProgress = progress_phase === 'exact' || (progress_phase == null && scan_complete);
  if (!exactProgress || etaSamples.length < 2) {
    etaEl.textContent = t('activity.calculating');
    return;
  }

  const now = Date.now();
  const first = etaSamples[0];
  const last = etaSamples[etaSamples.length - 1];
  const sampleDuration = last.ts - first.ts;
  const progressDelta = last.progress - first.progress;
  if (sampleDuration < 2_000 || progressDelta < 0.2 || now - lastProgressChangeTs > 3_000) {
    etaEl.textContent = t('activity.calculating');
    return;
  }
  const percentPerMs = progressDelta / sampleDuration;
  const remainingMs = (100 - last.progress) / percentPerMs;
  if (!Number.isFinite(remainingMs) || remainingMs <= 0) {
    etaEl.textContent = t('activity.calculating');
    return;
  }
  etaEl.textContent = t('activity.approximately', { time: formatEta(remainingMs) });
}

function formatStorage(mebibytes) {
  const value = Math.max(0, mebibytes ?? 0);
  return value >= 1024 ? `${(value / 1024).toFixed(2)} GiB` : `${value.toFixed(value < 10 ? 1 : 0)} MiB`;
}

function formatElapsed(milliseconds) {
  const totalTenths = Math.max(0, Math.round(milliseconds / 100));
  const hours = Math.floor(totalTenths / 36000);
  const minutes = Math.floor((totalTenths % 36000) / 600);
  const seconds = Math.floor((totalTenths % 600) / 10);
  const tenths = totalTenths % 10;
  return hours > 0
    ? `${hours}:${String(minutes).padStart(2, '0')}:${String(seconds).padStart(2, '0')}.${tenths}`
    : `${minutes}:${String(seconds).padStart(2, '0')}.${tenths}`;
}

function formatEta(milliseconds) {
  const totalSeconds = Math.max(1, Math.ceil(milliseconds / 1000));
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  return hours > 0
    ? `${hours}:${String(minutes).padStart(2, '0')}:${String(seconds).padStart(2, '0')}`
    : `${minutes}:${String(seconds).padStart(2, '0')}`;
}

// --- Log ---
function updateLog(log, active_files, reset) {
  const list = document.getElementById('log-list');
  const previousScrollTop = list.scrollTop;
  mutatingLog = true;
  if (reset) {
    list.innerHTML = '';
    logEntryMap.clear();
  }

  // Rust側からは前回カーソル以降の差分だけが届く。
  if (log.length > 0) {
    log.forEach(entry => applyLogEntry(entry));
  }

  // アクティブファイルのプログレスバーを更新
  for (const [name, ratio] of Object.entries(active_files)) {
    const item = logEntryMap.get(name);
    if (item && !item.completed) {
      updateProgressBar(item.el, ratio);
    }
  }

  // A poll may add and promote many rows. Follow once after the whole update so
  // layout work is coalesced and a reader who scrolled up is never disturbed.
  if (autoFollowLog) {
    scrollLogToBottom(list);
  } else if (reset) {
    // A backend state revision (for example cancellation status promotion)
    // rebuilds the rows, but is not a user-visible log clear.
    list.scrollTop = previousScrollTop;
  }
  mutatingLog = false;
}

function applyLogEntry({ ts_ms, file_name, status, error }) {
  const list = document.getElementById('log-list');
  const empty = document.getElementById('log-empty');
  if (empty) empty.remove();
  const item = logEntryMap.get(file_name);
  const action = logEntryAction(item, status);
  if (action === 'ignore') return;

  if (action === 'append') {
    trimLog(list);
    const completed = status !== 'processing';
    const el = completed
      ? buildCompletedRow(ts_ms, file_name, status, error)
      : buildProcessingRow(ts_ms, file_name);
    appendLogEntry(list, el, status);
    logElementKeys.set(el, file_name);
    logEntryMap.set(file_name, { el, completed });
  } else if (action === 'promote') {
    promoteRow(item.el, ts_ms, status, error);
    item.completed = true;
  }
}

// 上限超えたら最古エントリを削除
function trimLog(list) {
  if (shouldTrimLog(logEntryMap.size, MAX_LOG_ENTRIES)) {
    const oldest = list.querySelector('.log-entry');
    if (!oldest) return;
    const previousTop = list.scrollTop;
    const previousHeight = list.scrollHeight;
    const oldestKey = logElementKeys.get(oldest);
    if (oldestKey !== undefined) logEntryMap.delete(oldestKey);
    oldest.remove();
    if (!autoFollowLog) {
      list.scrollTop = compensateTrimmedScrollTop(
        previousTop,
        previousHeight,
        list.scrollHeight,
      );
    }
  }
}

function scrollLogToBottom(list) {
  list.scrollTop = list.scrollHeight;
}

function resetLogAutoFollow() {
  autoFollowLog = true;
  const list = document.getElementById('log-list');
  if (list) scrollLogToBottom(list);
}

// 進捗バーを更新（0.0–1.0）
function updateProgressBar(el, ratio) {
  const fill = el.querySelector('.log-progress-fill');
  const pct  = el.querySelector('.log-pct');
  if (!fill || !pct) return;
  const p = progressPercent(ratio);
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

  if (status === 'error') {
    // エラーはログ内の末尾に集約する。別枠に固定しないため、通常ログの確認を妨げない。
    el.classList.add('error');
    enableErrorCopy(el, el.querySelector('.log-filename')?.textContent ?? '', error);
    document.getElementById('log-list').appendChild(el);
  }
}

function appendLogEntry(list, el, status) {
  if (status === 'error') {
    list.appendChild(el);
    return;
  }

  // 新しい成功・処理中の行は、既にあるエラー群の直前へ挿入する。
  const firstError = list.querySelector('.log-entry.error');
  if (firstError) list.insertBefore(el, firstError);
  else list.appendChild(el);
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
  el.className = `log-entry${status === 'error' ? ' error' : ''}`;
  el.innerHTML = `
    <span class="log-time">${formatTime(ts_ms)}</span>
    <span class="log-badge ${escHtml(status)}">${escHtml(badgeLabel(status))}</span>
    <span class="log-filename" title="${escHtml(file_name)}">${escHtml(file_name)}</span>
    ${error ? `<span class="log-error" title="${escHtml(error)}">${escHtml(error)}</span>` : ''}
  `;
  if (status === 'error') enableErrorCopy(el, file_name, error);
  return el;
}

function enableErrorCopy(el, fileName, error) {
  if (el.dataset.copyEnabled === 'true') return;
  el.dataset.copyEnabled = 'true';
  el.addEventListener('contextmenu', (event) => {
    event.preventDefault();
    selectedErrorCopyText = `${fileName}\n\n${error || t('activity.copyErrorUnavailable')}`;
    showLogCopyMenu(event.clientX, event.clientY);
  });
}

function showLogCopyMenu(x, y) {
  const menu = document.getElementById('log-copy-menu');
  menu.classList.remove('hidden');
  const { width, height } = menu.getBoundingClientRect();
  menu.style.left = `${Math.max(8, Math.min(x, window.innerWidth - width - 8))}px`;
  menu.style.top = `${Math.max(8, Math.min(y, window.innerHeight - height - 8))}px`;
}

function hideLogCopyMenu() {
  document.getElementById('log-copy-menu').classList.add('hidden');
}

async function copySelectedErrorDetails() {
  if (!selectedErrorCopyText) return;
  try {
    await navigator.clipboard.writeText(selectedErrorCopyText);
  } catch (error) {
    console.error('Failed to copy conversion error details:', error);
  } finally {
    hideLogCopyMenu();
  }
}

// --- Helpers ---
function badgeLabel(status) {
  return {
    processing: t('activity.statusProcessing'),
    done:       t('activity.statusDone'),
    error:      t('activity.statusError'),
    skipped:    t('activity.statusSkipped'),
    cancelled:  t('activity.statusCancelled'),
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
window.addEventListener('unload', () => {
  clearInterval(pollTimer);
  clearInterval(interpolationTimer);
});

init();
