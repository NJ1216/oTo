import { initSVGController, setState, setFormat, setMode, setProgress } from './svg-controller.js';
import { initI18n, t } from './i18n/index.js';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

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
  } catch (e) {
    appSettings = {
      lastMode: 'encode',
      lastFormat: 'mp3',
      language: '',
    };
  }

  await initI18n(appSettings.language || '');
  const title = t('window.main');
  document.title = title;
  window.__TAURI__.webviewWindow.getCurrentWebviewWindow().setTitle(title);

  currentMode = appSettings.lastMode || 'encode';
  currentFormat = appSettings.lastFormat || 'mp3';
  currentDecodeFormat = appSettings.lastDecodeFormat || 'wav';

  await initSVGController(bgContainer, currentFormat, currentMode);

  applyModeToUI(); // 内部でapplyFormatToUI/applyDecodeFormatToUIを呼ぶ

  registerDragDrop();
  registerContextMenu();
  registerEventListeners();

  // Listen for Tauri conversion events
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

  // Listen for settings update from settings window
  await listen('settings_updated', async () => {
    appSettings = await invoke('get_settings');
    await initI18n(appSettings.language || '');
    applyFormatToUI();
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
  document.querySelectorAll('.decode-fmt-btn').forEach((btn) => {
    btn.classList.toggle('active', btn.dataset.fmt === currentDecodeFormat);
  });
  setFormat(currentDecodeFormat);
}

function applyFormatToUI() {
  const enabled = appSettings?.enabledFormats || ['mp3', 'm4a', 'flac'];

  document.querySelectorAll('.fmt-btn').forEach((btn) => {
    btn.style.display = enabled.includes(btn.dataset.fmt) ? '' : 'none';
    btn.classList.toggle('active', btn.dataset.fmt === currentFormat);
  });

  // currentFormat が非表示になった場合は最初の有効フォーマットに切り替え
  if (!enabled.includes(currentFormat)) {
    currentFormat = enabled[0] || 'mp3';
    document.querySelectorAll('.fmt-btn').forEach((btn) => {
      btn.classList.toggle('active', btn.dataset.fmt === currentFormat);
    });
  }

  setFormat(currentFormat);
}

function saveLastSettings() {
  if (!appSettings) return;
  appSettings.lastMode = currentMode;
  appSettings.lastFormat = currentFormat;
  appSettings.lastDecodeFormat = currentDecodeFormat;
  invoke('save_settings', { s: appSettings }).catch(() => {});
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
function setDragHover(hovered) {
  if (isProcessing) return;
  isDragging = hovered;
  setState(hovered ? 'hover' : 'standby');
}

function registerDragDrop() {
  listen('tauri://drag-enter', () => setDragHover(true));

  listen('tauri://drag-over', () => { if (!isDragging) setDragHover(true); });

  listen('tauri://drag-leave', () => setDragHover(false));

  listen('tauri://drag-drop', (event) => {
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
  setState('processing');
  setProgress(0);

  try {
    activeJobId = await invoke('convert_files', {
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
function showCompletionToast(successCount, errorCount, results) {
  if (successCount === 0 && errorCount === 0) {
    showToast(t('toast.noFiles'), 'warning', 4000);
    return;
  }

  if (errorCount === 0) {
    showToast(
      successCount === 1
        ? t('toast.done.one')
        : t('toast.done.many', { n: successCount }),
      'success'
    );
    return;
  }

  results?.filter((r) => !r.success && !r.skipped).forEach((r) => {
    console.error(`[oTo] 変換失敗: ${r.inputPath}\n${r.error}`);
  });

  if (successCount === 0) {
    showToast(t('toast.fail.all', { n: errorCount }), 'error', 4000);
  } else {
    showToast(t('toast.fail.partial', { ok: successCount, err: errorCount }), 'warning', 4000);
  }
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

// --- Misc event listeners ---
function registerEventListeners() {
  document.addEventListener('keydown', async (e) => {
    if (e.key === 'Escape') {
      if (activeJobId) {
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
}

init().catch(console.error);
