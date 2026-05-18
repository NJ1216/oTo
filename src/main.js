import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { homeDir } from '@tauri-apps/api/path';
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';
import { initSVGController, setState, setFormat, setMode, setProgress } from './svg-controller.js';
import { initI18n, t } from './i18n/index.js';

const appWindow = getCurrentWebviewWindow();

// --- State ---
let appSettings = null;
let currentMode = 'encode';
let currentFormat = 'mp3';
let isProcessing = false;
let isDragging = false;
let toastTimeout = null;
let activeJobId = null;
let jobCancelled = false;
let currentDecodeFormat = 'wav';
let homeDirPath = null;

// --- DOM refs ---
const bgContainer = document.getElementById('bg-container');
const toggle = document.getElementById('mode-toggle');
const formatSelector = document.getElementById('format-selector');
const contextMenu = document.getElementById('context-menu');
const toast = document.getElementById('toast');

// --- Init ---
async function init() {
  try {
    appSettings = await invoke('get_settings');
  } catch (_) {
    appSettings = {
      lastMode: 'encode',
      lastFormat: 'mp3',
      language: '',
    };
  }

  try { homeDirPath = await homeDir(); } catch (_) {}

  await initI18n(appSettings.language || '');
  const title = t('window.main');
  document.title = title;
  getCurrentWebviewWindow().setTitle(title);

  currentMode = appSettings.lastMode || 'encode';
  currentFormat = appSettings.lastFormat || 'mp3';
  currentDecodeFormat = appSettings.lastDecodeFormat || 'wav';

  await initSVGController(bgContainer, currentFormat, currentMode);

  applyModeToUI();

  registerDragDrop();
  registerContextMenu();
  registerEventListeners();

  await listen('progress', (event) => {
    const { percent } = event.payload;
    if (isProcessing) {
      setProgress(percent);
    }
  });

  await listen('conversion_complete', (event) => {
    if (jobCancelled) {
      jobCancelled = false;
      return;
    }
    const { successCount, errorCount, results } = event.payload;
    isProcessing = false;
    activeJobId = null;
    setState('standby');
    showCompletionToast(successCount, errorCount, results);
    saveLastSettings();
  });

  await listen('settings_updated', async () => {
    appSettings = await invoke('get_settings');
    await initI18n(appSettings.language || '');
    applyModeToUI();
  });

  await listen('silence_preview_opened', () => { silencePreviewVisible = true; });
  await listen('silence_preview_closed', () => { silencePreviewVisible = false; });
  await listen('overwrite_confirm', async (event) => {
    if (jobCancelled) {
      await invoke('respond_overwrite', { choice: 'cancel' }).catch(console.error);
      return;
    }
    showOverwriteDialog(event.payload);
  });

  // Cmd+Q (macOS) — ExitRequested はフロントエンドの onCloseRequested を経由しない
  await listen('quit_requested', async () => {
    if (isProcessing) {
      // 上書き確認ダイアログが出ていたらキャンセル応答を送り、ブロック解除してからpause
      if (isOverwriteDialogVisible()) {
        hideOverwriteDialog();
        await invoke('respond_overwrite', { choice: 'cancel' }).catch(console.error);
      }
      hidePauseDialog();
      if (activeJobId) {
        await invoke('pause_job', { jobId: activeJobId }).catch(console.error);
      }
      showQuitDialog();
    } else {
      await invoke('exit_app');
    }
  });

  // ウィンドウの閉じるボタン（macOS 赤ボタン）
  appWindow.onCloseRequested(async (event) => {
    if (isProcessing) {
      event.preventDefault();
      hidePauseDialog();
      if (activeJobId) {
        await invoke('pause_job', { jobId: activeJobId }).catch(console.error);
      }
      showQuitDialog();
    }
  });
}

// --- Mode / Format ---
function applyModeToUI() {
  toggle.checked = currentMode === 'decode';
  setMode(currentMode);
  formatSelector.style.display = 'flex';

  if (currentMode === 'decode') {
    document.querySelectorAll('.fmt-btn').forEach((b) => (b.style.display = 'none'));
    document.querySelectorAll('.decode-fmt-btn').forEach((b) => (b.style.display = ''));
    applyDecodeFormatToUI();
  } else {
    document.querySelectorAll('.decode-fmt-btn').forEach((b) => (b.style.display = 'none'));
    applyFormatToUI();
  }
}

function applyDecodeFormatToUI() {
  const enabledDecode = appSettings?.enabledDecodeFormats || ['wav', 'aiff'];
  document.querySelectorAll('.decode-fmt-btn').forEach((btn) => {
    btn.style.display = enabledDecode.includes(btn.dataset.fmt) ? '' : 'none';
    const active = btn.dataset.fmt === currentDecodeFormat;
    btn.classList.toggle('active', active);
    btn.setAttribute('aria-pressed', active ? 'true' : 'false');
  });
  if (!enabledDecode.includes(currentDecodeFormat)) {
    currentDecodeFormat = enabledDecode[0] || 'wav';
    document.querySelectorAll('.decode-fmt-btn').forEach((btn) => {
      const active = btn.dataset.fmt === currentDecodeFormat;
      btn.classList.toggle('active', active);
      btn.setAttribute('aria-pressed', active ? 'true' : 'false');
    });
  }
  setFormat(currentDecodeFormat);
}

function applyFormatToUI() {
  const enabled = appSettings?.enabledFormats || ['mp3', 'm4a', 'flac'];

  document.querySelectorAll('.fmt-btn').forEach((btn) => {
    btn.style.display = enabled.includes(btn.dataset.fmt) ? '' : 'none';
    const active = btn.dataset.fmt === currentFormat;
    btn.classList.toggle('active', active);
    btn.setAttribute('aria-pressed', active ? 'true' : 'false');
  });

  if (!enabled.includes(currentFormat)) {
    currentFormat = enabled[0] || 'mp3';
    document.querySelectorAll('.fmt-btn').forEach((btn) => {
      const active = btn.dataset.fmt === currentFormat;
      btn.classList.toggle('active', active);
      btn.setAttribute('aria-pressed', active ? 'true' : 'false');
    });
  }

  setFormat(currentFormat);
}

function saveLastSettings() {
  if (!appSettings) return;
  appSettings.lastMode = currentMode;
  appSettings.lastFormat = currentFormat;
  appSettings.lastDecodeFormat = currentDecodeFormat;
  invoke('save_settings', { s: appSettings }).catch((e) => console.error('save_settings failed:', e));
}

// --- Toggle switch ---
toggle.addEventListener('change', () => {
  currentMode = toggle.checked ? 'decode' : 'encode';
  applyModeToUI();
  saveLastSettings();
});

// --- Format selector ---
document.querySelectorAll('.fmt-btn').forEach((btn) => {
  btn.addEventListener('click', () => {
    currentFormat = btn.dataset.fmt;
    applyFormatToUI();
    saveLastSettings();
  });
});

document.querySelectorAll('.decode-fmt-btn').forEach((btn) => {
  btn.addEventListener('click', () => {
    currentDecodeFormat = btn.dataset.fmt;
    applyDecodeFormatToUI();
    saveLastSettings();
  });
});

// --- Drag & Drop via Tauri events ---
// drag-enter でキャッシュして drag-over/drag-leave の高頻度 IPC を削減。
// drag-drop は信頼性のため毎回 IPC で確認する。
let silencePreviewVisible = false;

function setDragHover(hovered) {
  if (isProcessing) return;
  isDragging = hovered;
  setState(hovered ? 'hover' : 'standby');
}

function registerDragDrop() {
  listen('tauri://drag-enter', () => {
    if (silencePreviewVisible) return;
    setDragHover(true);
  });

  listen('tauri://drag-over', () => {
    if (silencePreviewVisible) return;
    if (!isDragging) setDragHover(true);
  });

  listen('tauri://drag-leave', () => {
    if (silencePreviewVisible) return;
    setDragHover(false);
  });

  listen('tauri://drag-drop', (event) => {
    if (silencePreviewVisible) return;
    isDragging = false;
    if (isProcessing) return;

    const paths = event.payload.paths;
    if (!paths || paths.length === 0) {
      setState('standby');
      return;
    }

    startConversion(paths);
  });
}

// --- Conversion ---
async function startConversion(paths) {
  if (isProcessing) return;
  isProcessing = true;
  jobCancelled = false;
  setState('processing');
  setProgress(0);

  try {
    const jobId = crypto.randomUUID();
    activeJobId = jobId;
    await invoke('convert_files', {
      jobId,
      request: {
        paths,
        mode: currentMode,
        format: currentMode === 'decode' ? currentDecodeFormat : currentFormat,
      },
    });
  } catch (e) {
    isProcessing = false;
    activeJobId = null;
    setState('standby');
    showToast(t('toast.error', { msg: e }), 'error', 4000);
  }
}

// --- Toast ---
function getQualityInfo() {
  if (!appSettings || currentMode !== 'encode') return null;
  switch (currentFormat) {
    case 'mp3':
      if (appSettings.mp3Preset === 'custom') {
        if (appSettings.mp3Mode === 'vbr') return `VBR Quality ${appSettings.mp3VbrQuality}`;
        return `Bitrate ${appSettings.mp3Bitrate}kbps`;
      }
      return `Bitrate ${appSettings.mp3Preset}kbps`;
    case 'm4a':
      if (appSettings.aacPreset === 'custom') {
        if (appSettings.aacMode === 'vbr') return `VBR Quality ${appSettings.aacVbrQuality}`;
        return `Bitrate ${appSettings.m4aBitrate}kbps`;
      }
      return `Bitrate ${appSettings.aacPreset}kbps`;
    case 'opus':
      if (appSettings.opusPreset === 'custom') return `Bitrate ${appSettings.opusBitrate}kbps`;
      return `Bitrate ${appSettings.opusPreset}kbps`;
    case 'flac': {
      const lvl = appSettings.flacPreset === 'custom' ? appSettings.flacCompression : appSettings.flacPreset;
      return `Compression ${lvl}`;
    }
    case 'alac': {
      const bits = appSettings.alacPreset === 'custom' ? appSettings.alacBitDepth : 16;
      return `Lossless ${bits}bit`;
    }
    default:
      return null;
  }
}

function shortenPath(p) {
  const normalized = p.replace(/\\/g, '/');
  if (homeDirPath) {
    const home = homeDirPath.replace(/\\/g, '/').replace(/\/$/, '');
    if (normalized.startsWith(home)) return '~' + normalized.slice(home.length);
  }
  const parts = normalized.split('/');
  return parts.length > 3 ? '…/' + parts.slice(-2).join('/') : p;
}

function showCompletionToast(successCount, errorCount, results) {
  if (successCount === 0 && errorCount === 0) {
    showToast(t('toast.noFiles'), 'warning', 4000);
    return;
  }

  const failures = results?.filter((r) => !r.success && !r.skipped) ?? [];
  failures.forEach((r) => console.error(`[oTo] 変換失敗: ${r.inputPath}\n${r.error}`));

  const lines = [];

  if (successCount > 0) {
    lines.push(successCount === 1 ? t('toast.success.one') : t('toast.success.many', { n: successCount }));
    const qi = getQualityInfo();
    if (qi) lines.push(qi);
  }

  if (errorCount > 0) {
    lines.push(errorCount === 1 ? t('toast.fail.one') : t('toast.fail.many', { n: errorCount }));
    if (failures.length > 0) {
      lines.push(shortenPath(failures[0].inputPath));
      if (failures[0].error) lines.push(failures[0].error);
    }
  }

  const type = errorCount > 0 ? (successCount > 0 ? 'warning' : 'error') : 'success';
  const duration = errorCount > 0 ? 4000 : 2000;
  showToast(lines.join('\n'), type, duration);
}

function showToast(message, type = 'success', duration = 2000) {
  clearTimeout(toastTimeout);
  toast.textContent = message;
  toast.className = `toast ${type} visible`;
  toastTimeout = setTimeout(() => {
    toast.classList.remove('visible');
  }, duration);
}

// --- Context menu ---
function registerContextMenu() {
  document.addEventListener('contextmenu', (e) => {
    e.preventDefault();
    showContextMenu(e.clientX, e.clientY);
  });

  document.addEventListener('click', (e) => {
    if (!contextMenu.contains(e.target)) {
      hideContextMenu();
    }
  });

  document.getElementById('menu-settings').addEventListener('click', () => {
    hideContextMenu();
    invoke('open_settings_window').catch(console.error);
  });

  document.getElementById('menu-about').addEventListener('click', () => {
    hideContextMenu();
    invoke('open_about_window').catch(console.error);
  });
}

function showContextMenu(x, y) {
  contextMenu.style.display = 'block';
  const mw = contextMenu.offsetWidth;
  const mh = contextMenu.offsetHeight;
  const ww = window.innerWidth;
  const wh = window.innerHeight;
  contextMenu.style.left = (x + mw > ww ? x - mw : x) + 'px';
  contextMenu.style.top = (y + mh > wh ? y - mh : y) + 'px';
}

function hideContextMenu() {
  contextMenu.style.display = 'none';
}

// --- Pause dialog ---
function showPauseDialog() {
  document.getElementById('pause-dialog').classList.remove('hidden');
}

function hidePauseDialog() {
  document.getElementById('pause-dialog').classList.add('hidden');
}

// --- Quit confirm dialog ---
function showQuitDialog() {
  document.getElementById('quit-confirm-dialog').classList.remove('hidden');
}

function hideQuitDialog() {
  document.getElementById('quit-confirm-dialog').classList.add('hidden');
}

// --- Overwrite confirm dialog ---
function showOverwriteDialog(filename) {
  document.getElementById('overwrite-filename').textContent = filename;
  document.getElementById('overwrite-dialog').classList.remove('hidden');
}

function hideOverwriteDialog() {
  document.getElementById('overwrite-dialog').classList.add('hidden');
}

function isOverwriteDialogVisible() {
  return !document.getElementById('overwrite-dialog').classList.contains('hidden');
}

async function cancelAllViaOverwrite() {
  jobCancelled = true; // 先にフラグを立てて後続のoverwrite_confirmを自動却下
  hideOverwriteDialog();
  await invoke('respond_overwrite', { choice: 'cancel' }).catch(console.error);
  if (activeJobId) {
    const jobId = activeJobId;
    activeJobId = null;
    isProcessing = false;
    setState('standby');
    await invoke('cancel_job', { jobId }).catch(console.error);
  }
}

// --- Misc event listeners ---
function registerEventListeners() {
  document.addEventListener('keydown', async (e) => {
    if (e.key === 'Escape') {
      if (isOverwriteDialogVisible()) {
        await cancelAllViaOverwrite();
      } else if (activeJobId) {
        await invoke('pause_job', { jobId: activeJobId }).catch(console.error);
        showPauseDialog();
      } else {
        hideContextMenu();
      }
    }
  });

  document.getElementById('btn-resume').addEventListener('click', async () => {
    hidePauseDialog();
    if (activeJobId) {
      await invoke('resume_job', { jobId: activeJobId }).catch(console.error);
    }
  });

  document.getElementById('btn-cancel-job').addEventListener('click', async () => {
    hidePauseDialog();
    if (activeJobId) {
      const jobId = activeJobId;
      jobCancelled = true;
      activeJobId = null;
      isProcessing = false;
      setState('standby');
      await invoke('cancel_job', { jobId }).catch(console.error);
    }
  });

  document.getElementById('btn-overwrite-ok').addEventListener('click', async () => {
    hideOverwriteDialog();
    await invoke('respond_overwrite', { choice: 'overwrite' }).catch(console.error);
  });

  document.getElementById('btn-overwrite-rename').addEventListener('click', async () => {
    hideOverwriteDialog();
    await invoke('respond_overwrite', { choice: 'rename' }).catch(console.error);
  });

  document.getElementById('btn-overwrite-cancel').addEventListener('click', async () => {
    await cancelAllViaOverwrite();
  });

  document.getElementById('btn-quit-resume').addEventListener('click', async () => {
    hideQuitDialog();
    if (activeJobId) {
      await invoke('resume_job', { jobId: activeJobId }).catch(console.error);
    }
  });

  document.getElementById('btn-quit-close').addEventListener('click', async () => {
    hideQuitDialog();
    if (activeJobId) {
      const jobId = activeJobId;
      jobCancelled = true;
      activeJobId = null;
      isProcessing = false;
      setState('standby');
      await invoke('cancel_job', { jobId }).catch(console.error);
    }
    await invoke('exit_app');
  });
}

init().catch(console.error);
