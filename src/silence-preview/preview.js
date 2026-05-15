const { invoke } = window.__TAURI__.core;

const dropZone   = document.getElementById('drop-zone');
const dropHint   = document.getElementById('drop-hint');
const fileInfo   = document.getElementById('file-info');
const fileNameEl = document.getElementById('file-name');
const fileDurEl  = document.getElementById('file-duration');
const canvas     = document.getElementById('waveform');
const emptyEl    = document.getElementById('waveform-empty');
const dbInput    = document.getElementById('previewDb');
const durInput   = document.getElementById('previewDurationMs');
const analyzeBtn = document.getElementById('analyze-btn');
const statusEl   = document.getElementById('analyze-status');

let currentPath   = null;
let waveformPeaks = null;   // Vec<(f32, f32)>
let totalDuration = 0;
let analyzeTimer  = null;

// --- Load settings defaults ---
async function loadDefaults() {
  try {
    const s = await invoke('get_settings');
    dbInput.value  = String(s.silenceTrimDb ?? -80);
    durInput.value = String(s.silenceTrimDurationMs ?? 50);
  } catch (_) {}
}

// --- Drag & drop ---
dropZone.addEventListener('dragover', (e) => {
  e.preventDefault();
  dropZone.classList.add('drag-over');
});

dropZone.addEventListener('dragleave', () => {
  dropZone.classList.remove('drag-over');
});

dropZone.addEventListener('drop', async (e) => {
  e.preventDefault();
  dropZone.classList.remove('drag-over');
  const file = e.dataTransfer?.files?.[0];
  if (!file) return;
  await loadFile(file.path || file.name, file.name);
});

async function loadFile(path, name) {
  currentPath = path;
  waveformPeaks = null;
  fileNameEl.textContent = name;
  fileDurEl.textContent  = '';
  dropHint.classList.add('hidden');
  fileInfo.classList.remove('hidden');
  emptyEl.classList.add('hidden');
  clearCanvas();
  statusEl.textContent = '波形を読み込み中…';
  analyzeBtn.disabled = true;

  try {
    const data = await invoke('get_waveform_data', { path });
    waveformPeaks = data.peaksCh0;
    totalDuration = data.durationSecs;
    fileDurEl.textContent = formatDuration(totalDuration);
    drawWaveform(waveformPeaks, []);
    statusEl.textContent = '';
    analyzeBtn.disabled = false;
    scheduleAnalyze();
  } catch (err) {
    statusEl.textContent = 'エラー: ' + err;
    analyzeBtn.disabled = false;
  }
}

// --- Waveform rendering ---
function clearCanvas() {
  const ctx = canvas.getContext('2d');
  canvas.width  = canvas.offsetWidth  || 750;
  canvas.height = canvas.offsetHeight || 300;
  ctx.clearRect(0, 0, canvas.width, canvas.height);
}

function drawWaveform(peaks, silenceRegions) {
  const W = canvas.offsetWidth  || 750;
  const H = canvas.offsetHeight || 300;
  canvas.width  = W;
  canvas.height = H;

  const ctx = canvas.getContext('2d');
  ctx.clearRect(0, 0, W, H);

  if (!peaks || peaks.length === 0) return;

  const midY = H / 2;
  const scaleY = midY * 0.92;
  const barW = W / peaks.length;

  // Draw silence overlay regions first (behind waveform)
  if (silenceRegions.length > 0 && totalDuration > 0) {
    ctx.fillStyle = 'rgba(239, 68, 68, 0.22)';
    for (const [start, end] of silenceRegions) {
      const x1 = (start / totalDuration) * W;
      const x2 = (end   / totalDuration) * W;
      ctx.fillRect(x1, 0, x2 - x1, H);
    }
  }

  // Draw waveform bars
  for (let i = 0; i < peaks.length; i++) {
    const [mn, mx] = peaks[i];
    const x  = i * barW;
    const y1 = midY - mx * scaleY;
    const y2 = midY - mn * scaleY;

    // Check if this bar is in a silence region
    const tPos = (i / peaks.length) * totalDuration;
    const inSilence = silenceRegions.some(([s, e]) => tPos >= s && tPos <= e);

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
  const db  = parseFloat(dbInput.value)  || -80;
  const dur = parseInt(durInput.value, 10) || 50;

  statusEl.textContent = '解析中…';
  analyzeBtn.disabled = true;
  try {
    const regions = await invoke('get_silence_regions', { path: currentPath, db, durationMs: dur });
    drawWaveform(waveformPeaks, regions);
    const count = regions.length;
    const trimmed = regions.reduce((s, [a, b]) => s + (b - a), 0);
    statusEl.textContent = count > 0
      ? `${count}箇所 / 合計 ${trimmed.toFixed(2)}秒 の無音を検出`
      : '無音なし';
  } catch (err) {
    statusEl.textContent = 'エラー: ' + err;
  }
  analyzeBtn.disabled = false;
}

// --- Events ---
analyzeBtn.addEventListener('click', runAnalyze);

[dbInput, durInput].forEach((el) => {
  el.addEventListener('input', () => {
    if (waveformPeaks) scheduleAnalyze();
  });
});

window.addEventListener('resize', () => {
  if (waveformPeaks) drawWaveform(waveformPeaks, []);
});

// --- Helpers ---
function formatDuration(secs) {
  const m = Math.floor(secs / 60);
  const s = (secs % 60).toFixed(2).padStart(5, '0');
  return `${m}:${s}`;
}

// --- Init ---
loadDefaults();
