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

  currentMode = appSettings.lastMode || 'encode';
  currentFormat = appSettings.lastFormat || 'mp3';

  await initSVGController(bgContainer, currentFormat, currentMode);

  applyModeToUI();
  applyFormatToUI();

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
  });
}

// --- Mode / Format ---
function applyModeToUI() {
  toggle.checked = currentMode === 'decode';
  setMode(currentMode);

  if (currentMode === 'decode') {
    formatSelector.style.display = 'none';
  } else {
    formatSelector.style.display = 'flex';
  }
}

function applyFormatToUI() {
  document.querySelectorAll('.fmt-btn').forEach((btn) => {
    btn.classList.toggle('active', btn.dataset.fmt === currentFormat);
  });
  setFormat(currentFormat);
}

function saveLastSettings() {
  if (!appSettings) return;
  appSettings.lastMode = currentMode;
  appSettings.lastFormat = currentFormat;
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

// --- Drag & Drop via Tauri events ---
function registerDragDrop() {
  listen('tauri://drag-enter', () => {
    if (!isProcessing) {
      isDragging = true;
      setState('hover');
    }
  });

  listen('tauri://drag-over', () => {
    if (!isProcessing && !isDragging) {
      isDragging = true;
      setState('hover');
    }
  });

  listen('tauri://drag-leave', () => {
    if (!isProcessing) {
      isDragging = false;
      setState('standby');
    }
  });

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
        format: currentFormat,
      },
    });
  } catch (e) {
    isProcessing = false;
    activeJobId = null;
    setState('standby');
    showToast(t('toast.error', { msg: e }), 'error');
  }
}

// --- Toast ---
function showCompletionToast(successCount, errorCount, results) {
  if (errorCount === 0) {
    showToast(
      successCount === 1
        ? t('toast.done.one')
        : t('toast.done.many', { n: successCount }),
      'success'
    );
    return;
  }

  results?.filter((r) => !r.success).forEach((r) => {
    console.error(`[oTo] 変換失敗: ${r.inputPath}\n${r.error}`);
  });

  if (successCount === 0) {
    showToast(t('toast.fail.all', { n: errorCount }), 'error');
  } else {
    showToast(t('toast.fail.partial', { ok: successCount, err: errorCount }), 'warning');
  }
}

function showToast(message, type = 'success') {
  clearTimeout(toastTimeout);
  toast.textContent = message;
  toast.className = `toast ${type} visible`;
  toastTimeout = setTimeout(() => {
    toast.classList.remove('visible');
  }, 2000);
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

// --- Misc event listeners ---
function registerEventListeners() {
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') hideContextMenu();
  });
}

init().catch(console.error);
