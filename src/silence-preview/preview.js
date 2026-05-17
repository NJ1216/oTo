import { initI18n, t } from '../i18n/index.js';

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';

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

// Playback elements
const btnPlayFromStart = document.getElementById('btn-play-from-start');
const btnPlayLastTrim  = document.getElementById('btn-play-last-trim');
const btnStop          = document.getElementById('btn-stop');
const playbackBtns     = document.getElementById('playback-btns');
const volumeSlider     = document.getElementById('volume-slider');
const volumeValueEl    = document.getElementById('volume-value');
const preplayInput     = document.getElementById('preplay-seconds');

let currentPath    = null;
let waveformLevels = [];
let totalDuration  = 0;
let analyzeTimer   = null;
let silenceRegions = [];

// --- Zoom state ---
let viewStart = 0;
let viewEnd   = 1;
let vertScale = 1;

// --- Selection drag state ---
let isDragSelecting = false;
let selDragStartX   = null;

// --- Middle-click scroll state ---
let isMiddleScrolling = false;
let midScrollStartX   = null;
let midScrollStartViewStart = 0;
let midScrollStartViewEnd   = 0;

// --- Playback state ---
let audioElement = null;
let decodedWavPath = null;
let isPlaying = false;
let playbackMode = null;
let volume = 1.0;
let playbackProgress = 0;
let playbackAnimFrame = null;
let playbackStopTime = 0;

// --- Trim points ---
let firstSilenceEnd = 0;
let lastSilenceStart = 0;

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
  const win = getCurrentWebviewWindow();
  win.setTitle(t('settings.silencePreview.title'));
  document.title = t('settings.silencePreview.title');
  dropHint.textContent = t('silencePreview.drop');
  document.getElementById('legend-silence').textContent = t('silencePreview.silenceLabel');
  document.getElementById('legend-keep').textContent    = t('silencePreview.keepLabel');
}

// --- Help overlay ---
const helpTrigger = document.getElementById('help-trigger');
const helpOverlay = document.getElementById('help-overlay');

helpTrigger.addEventListener('mouseenter', () => {
  helpOverlay.classList.add('visible');
});

helpTrigger.addEventListener('mouseleave', (e) => {
  setTimeout(() => {
    if (!helpOverlay.matches(':hover') && !helpTrigger.matches(':hover')) {
      helpOverlay.classList.remove('visible');
    }
  }, 100);
});

helpOverlay.addEventListener('mouseenter', () => {
  helpOverlay.classList.add('visible');
});

helpOverlay.addEventListener('mouseleave', () => {
  helpOverlay.classList.remove('visible');
});

// --- Drag & Drop ---
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
  waveformLevels = [];
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
  statusEl.textContent = t('silencePreview.loadingWaveform');

  stopPlayback();
  if (decodedWavPath) {
    URL.revokeObjectURL(decodedWavPath);
    decodedWavPath = null;
  }
  firstSilenceEnd = 0;
  lastSilenceStart = 0;
  playbackProgress = 0;

  try {
    const data = await invoke('get_waveform_data', { path });
    waveformLevels = data.levels;
    totalDuration = data.durationSecs;
    fileDurEl.textContent = formatDuration(totalDuration);
    redraw();
    statusEl.textContent = '';
    scheduleAnalyze();

    try {
      const wavBase64 = await invoke('decode_to_wav', { path });
      const binaryString = atob(wavBase64);
      const bytes = new Uint8Array(binaryString.length);
      for (let i = 0; i < binaryString.length; i++) {
        bytes[i] = binaryString.charCodeAt(i);
      }
      const blob = new Blob([bytes], { type: 'audio/wav' });
      decodedWavPath = URL.createObjectURL(blob);
    } catch (e) {
      console.error('WAV decode failed:', e);
    }
  } catch (err) {
    statusEl.textContent = t('silencePreview.error', { msg: err });
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
  if (waveformLevels.length === 0) return;
  const levelIdx = waveformLevels.length - 1;
  drawWaveform(waveformLevels[levelIdx].peaks, silenceRegions);
}

function drawWaveform(peaks, regions) {
  const W = canvas.offsetWidth  || 800;
  const H = canvas.offsetHeight || 300;
  canvas.width  = W;
  canvas.height = H;

  const ctx = canvas.getContext('2d');
  ctx.clearRect(0, 0, W, H);

  if (!peaks || peaks.length === 0) return;

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

  const startIdx = Math.floor(viewStart * peaks.length);
  const endIdx   = Math.ceil(viewEnd   * peaks.length);
  drawChannel(ctx, peaks, startIdx, endIdx, regions, W, H, vertScale);

  // Playback position indicator
  if (isPlaying && playbackProgress > 0 && totalDuration > 0) {
    const tStart = viewStart * totalDuration;
    const tRange = (viewEnd - viewStart) * totalDuration;
    const px = ((playbackProgress - tStart) / tRange) * W;
    if (px >= 0 && px <= W) {
      ctx.strokeStyle = '#fbbf24';
      ctx.lineWidth = 2;
      ctx.beginPath();
      ctx.moveTo(px, 0);
      ctx.lineTo(px, H);
      ctx.stroke();
    }
  }
}

function drawChannel(ctx, peaks, startIdx, endIdx, regions, canvasW, canvasH, vScale) {
  const midY = canvasH / 2;
  const scaleY = (canvasH / 2) * 0.92 * Math.min(vScale, 50);
  const visibleCount = endIdx - startIdx;
  if (visibleCount <= 0) return;

  const barW = 1;
  const bucketSize = visibleCount / canvasW;

  for (let px = 0; px < canvasW; px++) {
    const bStart = startIdx + Math.floor(px * bucketSize);
    const bEnd   = startIdx + Math.floor((px + 1) * bucketSize);
    let mn = 0, mx = 0;
    for (let i = bStart; i < bEnd; i++) {
      const [vMn, vMx] = peaks[i];
      if (vMn < mn) mn = vMn;
      if (vMx > mx) mx = vMx;
    }

    const x = px;
    const y1 = midY - Math.min(mx * scaleY, canvasH / 2);
    const y2 = midY - Math.max(mn * scaleY, -canvasH / 2);

    const tPos = (viewStart + (px / canvasW) * (viewEnd - viewStart)) * totalDuration;
    const inSilence = regions.some(([s, e]) => tPos >= s && tPos <= e);

    ctx.fillStyle = inSilence
      ? 'rgba(239, 68, 68, 0.6)'
      : 'rgba(99, 102, 241, 0.7)';
    ctx.fillRect(x, y1, barW, Math.max(y2 - y1, 1));
  }

  ctx.strokeStyle = 'rgba(255, 255, 255, 0.07)';
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(0, midY);
  ctx.lineTo(canvasW, midY);
  ctx.stroke();
}

// --- Analysis ---
function scheduleAnalyze() {
  clearTimeout(analyzeTimer);
  analyzeTimer = setTimeout(runAnalyze, 400);
}

async function runAnalyze() {
  if (!currentPath || waveformLevels.length === 0) return;
  const db  = parseFloat(dbInput.value)   || -80;
  const dur = parseInt(durInput.value, 10) || 50;

  statusEl.textContent = t('silencePreview.analyzing');
  try {
    const levelIdx = waveformLevels.length - 1;
    const level = waveformLevels[levelIdx];
    silenceRegions = detectSilence(level.rms, db, dur / 1000.0);
    redraw();
    const count   = silenceRegions.length;
    const trimmed = silenceRegions.reduce((s, [a, b]) => s + (b - a), 0);
    statusEl.textContent = count > 0
      ? t('silencePreview.silenceDetected', { count, total: trimmed.toFixed(2) })
      : t('silencePreview.noSilence');

    if (silenceRegions.length > 0) {
      firstSilenceEnd = silenceRegions[0][1];
      if (silenceRegions.length > 1) {
        lastSilenceStart = silenceRegions[silenceRegions.length - 1][0];
      } else {
        lastSilenceStart = 0;
      }
    } else {
      firstSilenceEnd = 0;
      lastSilenceStart = 0;
    }
  } catch (err) {
    statusEl.textContent = t('silencePreview.error', { msg: err });
  }
}

function detectSilence(rmsValues, db, minDurationSecs) {
  const dbLinear = Math.pow(10, db / 20);
  const n = rmsValues.length;
  if (n === 0) return [];

  const sampleDur = totalDuration / n;
  const allRegions = [];
  let inSilence = false;
  let silenceStart = 0;

  for (let i = 0; i < n; i++) {
    const isQuiet = rmsValues[i] < dbLinear;

    if (isQuiet && !inSilence) {
      inSilence = true;
      silenceStart = i * sampleDur;
    } else if (!isQuiet && inSilence) {
      const silenceEnd = i * sampleDur;
      if (silenceEnd - silenceStart >= minDurationSecs) {
        allRegions.push([silenceStart, silenceEnd]);
      }
      inSilence = false;
    }
  }

  if (inSilence) {
    const silenceEnd = totalDuration;
    if (silenceEnd - silenceStart >= minDurationSecs) {
      allRegions.push([silenceStart, silenceEnd]);
    }
  }

  if (allRegions.length === 0) return [];

  const tolerance = 0.05; // 50ms tolerance — match Rust logic
  const result = [];

  // Only treat as "start silence" if it begins near the very start
  if (allRegions[0][0] <= tolerance) {
    result.push(allRegions[0]);
  }

  // Only treat as "end silence" if it ends near the very end
  if (allRegions.length > 1) {
    const last = allRegions[allRegions.length - 1];
    if (Math.abs(totalDuration - last[1]) <= tolerance) {
      // Avoid duplicating the same region
      if (result.length === 0 || last !== result[0]) {
        result.push(last);
      }
    }
  }

  return result;
}

[dbInput, durInput].forEach((el) => {
  el.addEventListener('input', () => {
    if (waveformLevels.length > 0) scheduleAnalyze();
  });
});

// --- Playback ---
function updatePlaybackButtons() {
  if (isPlaying) {
    btnPlayFromStart.classList.add('hidden');
    btnPlayLastTrim.classList.add('hidden');
    btnStop.classList.remove('hidden');
  } else {
    btnPlayFromStart.classList.remove('hidden');
    btnPlayLastTrim.classList.remove('hidden');
    btnStop.classList.add('hidden');
  }
}

function stopPlayback() {
  if (playbackAnimFrame) {
    cancelAnimationFrame(playbackAnimFrame);
    playbackAnimFrame = null;
  }
  if (audioElement) {
    audioElement.pause();
    audioElement.onended = null;
    audioElement.ontimeupdate = null;
    audioElement = null;
  }
  isPlaying = false;
  playbackMode = null;
  playbackStopTime = 0;
  updatePlaybackButtons();
}

function createAudio() {
  if (!decodedWavPath) return null;
  const audio = new Audio(decodedWavPath);
  audio.volume = volume;
  audio.preload = 'auto';
  return audio;
}

function updatePlaybackPosition(ratio) {
  playbackProgress = ratio * totalDuration;
  redraw();
}

function startProgressTracking() {
  if (!audioElement) return;
  function tick() {
    if (!audioElement || audioElement.paused) return;
    if (playbackStopTime > 0 && audioElement.currentTime >= playbackStopTime) {
      stopPlayback();
      return;
    }
    const ratio = audioElement.duration > 0 ? audioElement.currentTime / audioElement.duration : 0;
    updatePlaybackPosition(ratio);
    playbackAnimFrame = requestAnimationFrame(tick);
  }
  playbackAnimFrame = requestAnimationFrame(tick);
}

function playFromTrimStart() {
  if (!decodedWavPath || totalDuration === 0) return;
  const startTime = firstSilenceEnd > 0 ? firstSilenceEnd : 0;
  if (startTime >= totalDuration) return;

  stopPlayback();
  const audio = createAudio();
  if (!audio) return;

  audioElement = audio;
  audio.currentTime = startTime;
  playbackMode = 'from-trim-start';
  playbackStopTime = 0;
  isPlaying = true;
  updatePlaybackButtons();

  audio.onended = () => {
    stopPlayback();
  };

  audio.play().catch(() => {
    stopPlayback();
  });

  startProgressTracking();
}

function playLastTrim() {
  if (!decodedWavPath || totalDuration === 0) return;
  if (lastSilenceStart <= 0) return;

  const preplaySecs = parseFloat(preplayInput.value) || 2;
  const startTime = Math.max(0, lastSilenceStart - preplaySecs);

  stopPlayback();
  const audio = createAudio();
  if (!audio) return;

  audioElement = audio;
  audio.currentTime = startTime;
  playbackMode = 'last-trim';
  playbackStopTime = lastSilenceStart;
  isPlaying = true;
  updatePlaybackButtons();

  audio.onended = () => {
    stopPlayback();
  };

  audio.play().catch(() => {
    stopPlayback();
  });

  startProgressTracking();
}

function togglePlayStop() {
  if (isPlaying) {
    stopPlayback();
    playbackProgress = 0;
    redraw();
  } else {
    playFromTrimStart();
  }
}

// --- Playback event handlers ---
btnPlayFromStart.addEventListener('click', () => {
  playFromTrimStart();
});

btnPlayLastTrim.addEventListener('click', () => {
  playLastTrim();
});

btnStop.addEventListener('click', () => {
  stopPlayback();
  playbackProgress = 0;
  redraw();
});

// --- Volume control ---
volumeSlider.addEventListener('input', () => {
  volume = parseInt(volumeSlider.value, 10) / 100;
  volumeValueEl.textContent = `${volumeSlider.value}%`;
  if (audioElement) {
    audioElement.volume = volume;
  }
});

// --- Cancel / Apply buttons ---
const btnCancel = document.getElementById('btn-cancel');
const btnApply  = document.getElementById('btn-apply');

btnCancel.addEventListener('click', async () => {
  const win = getCurrentWebviewWindow();
  await win.close();
});

btnApply.addEventListener('click', async () => {
  try {
    const s = await invoke('get_settings');
    s.silenceTrimDb = parseFloat(dbInput.value) || -80;
    s.silenceTrimDurationMs = parseInt(durInput.value, 10) || 50;
    await invoke('save_settings', { s });
  } catch (_) {}
  const win = getCurrentWebviewWindow();
  await win.close();
});

// --- Keyboard shortcuts ---
document.addEventListener('keydown', (e) => {
  if (e.code === 'Space' && currentPath && decodedWavPath) {
    e.preventDefault();
    if (e.shiftKey) {
      playLastTrim();
    } else {
      togglePlayStop();
    }
    return;
  }

  const key = e.key.toLowerCase();
  if (key === 'g' && !e.shiftKey) {
    zoomHorizontal(1.5, 0.5);
  } else if (key === 'h' && !e.shiftKey) {
    zoomHorizontal(1 / 1.5, 0.5);
  } else if (key === 'g' && e.shiftKey) {
    vertScale = Math.max(1, vertScale / 1.5);
    redraw();
  } else if (key === 'h' && e.shiftKey) {
    vertScale = Math.min(50, vertScale * 1.5);
    redraw();
  }
});

function zoomHorizontal(factor, pivotRatio) {
  let range = (viewEnd - viewStart) * factor;
  range = Math.max(0.005, Math.min(1, range));
  const pivot = pivotRatio !== undefined
    ? viewStart + pivotRatio * (viewEnd - viewStart)
    : (viewStart + viewEnd) / 2;
  viewStart = pivot - range * (pivot - viewStart) / (viewEnd - viewStart || 1);
  viewEnd   = viewStart + range;
  if (viewStart < 0) { viewEnd -= viewStart; viewStart = 0; }
  if (viewEnd > 1)   { viewStart -= viewEnd - 1; viewEnd = 1; }
  viewStart = Math.max(0, viewStart);
  viewEnd   = Math.min(1, viewEnd);
  redraw();
}

canvas.addEventListener('wheel', (e) => {
  e.preventDefault();
  if (waveformLevels.length === 0) return;

  if (e.shiftKey) {
    const delta = e.deltaY !== 0 ? e.deltaY : e.deltaX;
    const factor = delta < 0 ? 1.15 : 1 / 1.15;
    vertScale = Math.max(1, Math.min(50, vertScale * factor));
    redraw();
  } else {
    const rect = canvas.getBoundingClientRect();
    const pivotRatio = (e.clientX - rect.left) / rect.width;
    const factor = e.deltaY > 0 ? 1.15 : 1 / 1.15;
    zoomHorizontal(factor, pivotRatio);
  }
}, { passive: false });

canvas.style.cursor = 'text';

canvas.addEventListener('mousedown', (e) => {
  if (waveformLevels.length === 0) return;
  if (e.button === 1) {
    e.preventDefault();
    isMiddleScrolling = true;
    midScrollStartX = e.clientX;
    midScrollStartViewStart = viewStart;
    midScrollStartViewEnd = viewEnd;
    canvas.style.cursor = 'grabbing';
    return;
  }
  if (e.button === 0 && e.shiftKey) {
    vertScale = 1;
    redraw();
    return;
  }
  if (e.button === 0) {
    isDragSelecting = true;
    selDragStartX   = e.offsetX;
    selOverlay.style.display = 'none';
  }
});

canvas.addEventListener('auxclick', (e) => {
  if (e.button === 1) {
    e.preventDefault();
  }
});

window.addEventListener('mousemove', (e) => {
  if (isMiddleScrolling) {
    const dx = e.clientX - midScrollStartX;
    const W = canvas.offsetWidth || 800;
    const range = midScrollStartViewEnd - midScrollStartViewStart;
    const shift = -(dx / W) * range;
    let newStart = midScrollStartViewStart + shift;
    let newEnd = midScrollStartViewEnd + shift;
    if (newStart < 0) { newEnd -= newStart; newStart = 0; }
    if (newEnd > 1) { newStart -= newEnd - 1; newEnd = 1; }
    viewStart = Math.max(0, newStart);
    viewEnd = Math.min(1, newEnd);
    redraw();
    return;
  }
  if (!isDragSelecting) return;
  const rect = canvas.getBoundingClientRect();
  const offsetX = e.clientX - rect.left;
  const x1 = Math.min(selDragStartX, offsetX);
  const x2 = Math.max(selDragStartX, offsetX);
  if (x2 - x1 > 2) {
    selOverlay.style.left    = x1 + 'px';
    selOverlay.style.width   = (x2 - x1) + 'px';
    selOverlay.style.display = 'block';
  }
});

window.addEventListener('mouseup', (e) => {
  if (isMiddleScrolling) {
    isMiddleScrolling = false;
    canvas.style.cursor = 'text';
    return;
  }
  if (!isDragSelecting) return;
  isDragSelecting = false;
  selOverlay.style.display = 'none';

  const rect = canvas.getBoundingClientRect();
  const offsetX = Math.max(0, Math.min(e.clientX - rect.left, canvas.offsetWidth));
  const x1 = Math.min(selDragStartX, offsetX);
  const x2 = Math.max(selDragStartX, offsetX);
  const W  = canvas.offsetWidth || 800;

  if (x2 - x1 < 4) {
    viewStart = 0;
    viewEnd   = 1;
    redraw();
    return;
  }

  const currentRange = viewEnd - viewStart;
  const newStart = viewStart + (x1 / W) * currentRange;
  const newEnd   = viewStart + (x2 / W) * currentRange;
  viewStart = newStart;
  viewEnd   = newEnd;
  redraw();
});

window.addEventListener('resize', () => {
  if (waveformLevels.length > 0) redraw();
});

function formatDuration(secs) {
  const m = Math.floor(secs / 60);
  const s = (secs % 60).toFixed(2).padStart(5, '0');
  return `${m}:${s}`;
}

init().catch(console.error);
