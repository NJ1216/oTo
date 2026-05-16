import { initI18n, t } from '../i18n/index.js';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const dropZone   = document.getElementById('drop-zone');
const dropHint   = document.getElementById('drop-hint');
const fileInfo   = document.getElementById('file-info');
const fileNameEl = document.getElementById('file-name');
const fileDurEl  = document.getElementById('file-duration');
const canvas     = document.getElementById('waveform');
const selOverlay = document.getElementById('selection-overlay');
const emptyEl    = document.getElementById('waveform-empty');
const dbInput    = document.getElementById('previewDb');
const durInput   = document.getElementById('previewDurationMs');
const statusEl   = document.getElementById('analyze-status');

let currentPath    = null;
let waveformPeaks  = null;
let totalDuration  = 0;
let analyzeTimer   = null;
let silenceRegions = [];

// --- Zoom state ---
let viewStart = 0;   // 0.0–1.0 割合
let viewEnd   = 1;
let vertScale = 1;

// --- Selection drag state ---
let isDragSelecting = false;
let selDragStartX   = null;

// --- Init ---
async function init() {
  try {
    const s = await invoke('get_settings');
    await initI18n(s.language || '');
    dbInput.value  = String(s.silenceTrimDb ?? -80);
    durInput.value = String(s.silenceTrimDurationMs ?? 50);
    applyI18n();
  } catch (_) {}
}

function applyI18n() {
  const win = window.__TAURI__.webviewWindow.getCurrentWebviewWindow();
  win.setTitle(t('silencePreview.title'));
  document.title = t('silencePreview.title');
  dropHint.textContent = t('silencePreview.drop');
  emptyEl.textContent  = t('silencePreview.empty');
  document.getElementById('zoom-hint').textContent      = t('silencePreview.zoomHint');
  document.getElementById('legend-silence').textContent = t('silencePreview.silenceLabel');
  document.getElementById('legend-keep').textContent    = t('silencePreview.keepLabel');
}

// --- Drag & Drop via Tauri native events (fixes main-window interference) ---
listen('tauri://drag-enter', () => {
  dropZone.classList.add('drag-over');
});

listen('tauri://drag-over', () => {
  dropZone.classList.add('drag-over');
});

listen('tauri://drag-leave', () => {
  dropZone.classList.remove('drag-over');
});

listen('tauri://drag-drop', async (event) => {
  dropZone.classList.remove('drag-over');
  const paths = event.payload?.paths;
  if (!paths || paths.length === 0) return;
  const path = paths[0];
  const name = path.replace(/\\/g, '/').split('/').pop();
  await loadFile(path, name);
});

async function loadFile(path, name) {
  currentPath    = path;
  waveformPeaks  = null;
  silenceRegions = [];
  viewStart = 0;
  viewEnd   = 1;
  vertScale = 1;
  fileNameEl.textContent = name;
  fileDurEl.textContent  = '';
  dropHint.classList.add('hidden');
  fileInfo.classList.remove('hidden');
  emptyEl.classList.add('hidden');
  clearCanvas();
  statusEl.textContent = '波形を読み込み中…';

  try {
    const data = await invoke('get_waveform_data', { path });
    waveformPeaks = data.peaksCh0;
    totalDuration = data.durationSecs;
    fileDurEl.textContent = formatDuration(totalDuration);
    redraw();
    statusEl.textContent = '';
    scheduleAnalyze();
  } catch (err) {
    statusEl.textContent = 'エラー: ' + err;
  }
}

// --- Waveform rendering ---
function clearCanvas() {
  const ctx = canvas.getContext('2d');
  canvas.width  = canvas.offsetWidth  || 800;
  canvas.height = canvas.offsetHeight || 300;
  ctx.clearRect(0, 0, canvas.width, canvas.height);
}

function redraw() {
  drawWaveform(waveformPeaks, silenceRegions);
}

function drawWaveform(peaks, regions) {
  const W = canvas.offsetWidth  || 800;
  const H = canvas.offsetHeight || 300;
  canvas.width  = W;
  canvas.height = H;

  const ctx = canvas.getContext('2d');
  ctx.clearRect(0, 0, W, H);

  if (!peaks || peaks.length === 0) return;

  const startIdx = Math.floor(viewStart * peaks.length);
  const endIdx   = Math.ceil(viewEnd   * peaks.length);
  const visible  = peaks.slice(startIdx, endIdx);
  if (visible.length === 0) return;

  const midY   = H / 2;
  const scaleY = midY * 0.92 * Math.min(vertScale, 50);
  const barW   = W / visible.length;

  // Silence overlay regions
  if (regions.length > 0 && totalDuration > 0) {
    ctx.fillStyle = 'rgba(239, 68, 68, 0.18)';
    const tStart = viewStart * totalDuration;
    const tRange = (viewEnd - viewStart) * totalDuration;
    for (const [start, end] of regions) {
      const x1 = Math.max(0, ((start - tStart) / tRange) * W);
      const x2 = Math.min(W, ((end   - tStart) / tRange) * W);
      if (x2 > x1) ctx.fillRect(x1, 0, x2 - x1, H);
    }
  }

  // Waveform bars
  for (let i = 0; i < visible.length; i++) {
    const [mn, mx] = visible[i];
    const x  = i * barW;
    const y1 = midY - Math.min(mx * scaleY,  midY);
    const y2 = midY - Math.max(mn * scaleY, -midY);

    const tPos = (viewStart + (i / visible.length) * (viewEnd - viewStart)) * totalDuration;
    const inSilence = regions.some(([s, e]) => tPos >= s && tPos <= e);

    ctx.fillStyle = inSilence
      ? 'rgba(239, 68, 68, 0.65)'
      : 'rgba(99, 102, 241, 0.75)';
    ctx.fillRect(x, y1, Math.max(barW - 0.5, 0.5), Math.max(y2 - y1, 1));
  }

  // Center line
  ctx.strokeStyle = 'rgba(255, 255, 255, 0.07)';
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(0, midY);
  ctx.lineTo(W, midY);
  ctx.stroke();
}

// --- Analysis ---
function scheduleAnalyze() {
  clearTimeout(analyzeTimer);
  analyzeTimer = setTimeout(runAnalyze, 400);
}

async function runAnalyze() {
  if (!currentPath || !waveformPeaks) return;
  const db  = parseFloat(dbInput.value)   || -80;
  const dur = parseInt(durInput.value, 10) || 50;

  statusEl.textContent = '解析中…';
  try {
    silenceRegions = await invoke('get_silence_regions', { path: currentPath, db, durationMs: dur });
    redraw();
    const count   = silenceRegions.length;
    const trimmed = silenceRegions.reduce((s, [a, b]) => s + (b - a), 0);
    statusEl.textContent = count > 0
      ? `${count}箇所 / 合計 ${trimmed.toFixed(2)}秒 の無音を検出`
      : '無音なし';
  } catch (err) {
    statusEl.textContent = 'エラー: ' + err;
  }
}

// Input changes trigger auto-analyze
[dbInput, durInput].forEach((el) => {
  el.addEventListener('input', () => {
    if (waveformPeaks) scheduleAnalyze();
  });
});

// --- Zoom: keyboard shortcuts ---
document.addEventListener('keydown', (e) => {
  const key = e.key.toLowerCase();
  if (key === 'g' && !e.shiftKey) {
    zoomHorizontal(1 / 1.5);
  } else if (key === 'h' && !e.shiftKey) {
    zoomHorizontal(1.5);
  } else if (key === 'g' && e.shiftKey) {
    vertScale = Math.max(0.1, vertScale / 1.5);
    redraw();
  } else if (key === 'h' && e.shiftKey) {
    vertScale = Math.min(50, vertScale * 1.5);
    redraw();
  }
});

function zoomHorizontal(factor) {
  const center  = (viewStart + viewEnd) / 2;
  let   range   = (viewEnd - viewStart) * factor;
  range = Math.max(0.005, Math.min(1, range));
  viewStart = Math.max(0, center - range / 2);
  viewEnd   = Math.min(1, viewStart + range);
  if (viewEnd === 1) viewStart = Math.max(0, 1 - range);
  redraw();
}

// --- Zoom: canvas mouse (range selection + click reset) ---
canvas.style.cursor = 'text';

canvas.addEventListener('mousedown', (e) => {
  if (!waveformPeaks) return;
  if (e.shiftKey) {
    // Shift+click: reset vertical zoom
    vertScale = 1;
    redraw();
    return;
  }
  isDragSelecting = true;
  selDragStartX   = e.offsetX;
  selOverlay.style.display = 'none';
});

canvas.addEventListener('mousemove', (e) => {
  if (!isDragSelecting) return;
  const x1 = Math.min(selDragStartX, e.offsetX);
  const x2 = Math.max(selDragStartX, e.offsetX);
  if (x2 - x1 > 2) {
    selOverlay.style.left    = x1 + 'px';
    selOverlay.style.width   = (x2 - x1) + 'px';
    selOverlay.style.display = 'block';
  }
});

canvas.addEventListener('mouseup', (e) => {
  if (!isDragSelecting) return;
  isDragSelecting = false;
  selOverlay.style.display = 'none';

  const x1 = Math.min(selDragStartX, e.offsetX);
  const x2 = Math.max(selDragStartX, e.offsetX);
  const W  = canvas.offsetWidth || 800;

  if (x2 - x1 < 4) {
    // Click: reset horizontal zoom
    viewStart = 0;
    viewEnd   = 1;
    redraw();
    return;
  }

  // Zoom into selection
  const currentRange = viewEnd - viewStart;
  const newStart = viewStart + (x1 / W) * currentRange;
  const newEnd   = viewStart + (x2 / W) * currentRange;
  viewStart = newStart;
  viewEnd   = newEnd;
  redraw();
});

canvas.addEventListener('mouseleave', () => {
  if (isDragSelecting) {
    isDragSelecting = false;
    selOverlay.style.display = 'none';
  }
});

// --- Resize ---
window.addEventListener('resize', () => {
  if (waveformPeaks) redraw();
});

// --- Helpers ---
function formatDuration(secs) {
  const m = Math.floor(secs / 60);
  const s = (secs % 60).toFixed(2).padStart(5, '0');
  return `${m}:${s}`;
}

init().catch(console.error);
