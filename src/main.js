import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
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
let activeOverwriteDialogId = null;
let sessionQualityProfile = null;
let overwriteDialogSuspendedForQuit = false;

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
    const { percent, jobId } = event.payload;
    // 旧ジョブの遅延した progress を取りこぼさないようにジョブ ID で照合
    if (isProcessing && (!jobId || jobId === activeJobId)) {
      setProgress(percent);
    }
  });

  await listen('conversion_complete', (event) => {
    if (jobCancelled) {
      jobCancelled = false;
      return;
    }
    const { successCount, errorCount, skippedCount, mixedProfiles, results, jobId } = event.payload;
    if (jobId && activeJobId && jobId !== activeJobId) return;
    isProcessing = false;
    activeJobId = null;
    setState('standby');
    showCompletionToast(
      successCount,
      errorCount,
      skippedCount,
      mixedProfiles,
      results,
      sessionQualityProfile,
    );
    sessionQualityProfile = null;
    saveLastSettings();
  });

  await listen('settings_preview_updated', async (event) => {
    appSettings = event.payload;
    await initI18n(appSettings.language || '');
    applyModeToUI();
  });

  await listen('settings_updated', async (event) => {
    appSettings = event.payload || await invoke('get_settings');
    await initI18n(appSettings.language || '');
    applyModeToUI();
  });

  await listen('silence_preview_opened', () => { silencePreviewVisible = true; });
  await listen('silence_preview_closed', () => { silencePreviewVisible = false; });

  // HMR/リロード時に silence-preview が既に開いている可能性があるため初期同期する
  try { silencePreviewVisible = await invoke('is_silence_preview_visible'); } catch (_) {}
  await listen('overwrite_confirm', async (event) => {
    if (jobCancelled) {
      await respondOverwrite('cancel_all');
      return;
    }
    showOverwriteDialog(event.payload);
  });

  // Cmd+Q (macOS) — ExitRequested はフロントエンドの onCloseRequested を経由しない
  await listen('quit_requested', async () => {
    if (isProcessing) {
      // 上書き確認は保留したまま隠し、セッション全体を一時停止する。
      if (isOverwriteDialogVisible()) {
        hideOverwriteDialog();
        overwriteDialogSuspendedForQuit = true;
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
      if (isOverwriteDialogVisible()) {
        hideOverwriteDialog();
        overwriteDialogSuspendedForQuit = true;
      }
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
  isDragging = hovered;
  setState(hovered ? 'hover' : (isProcessing ? 'processing' : 'standby'));
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
    const paths = event.payload.paths;
    if (!paths || paths.length === 0) {
      setState(isProcessing ? 'processing' : 'standby');
      return;
    }

    startConversion(paths);
  });
}

// --- Conversion ---
async function startConversion(paths) {
  const wasProcessing = isProcessing;
  if (!isProcessing) {
    isProcessing = true;
    activeJobId = crypto.randomUUID();
    setProgress(0);
  }
  jobCancelled = false;
  setState('processing');
  const submittedProfile = {
    mode: currentMode,
    format: currentMode === 'decode' ? currentDecodeFormat : currentFormat,
    settings: { ...appSettings },
  };
  const previousQualityProfile = sessionQualityProfile;
  sessionQualityProfile ??= submittedProfile;

  try {
    if (appSettings?.autoOpenActivity) {
      invoke('open_activity_window').catch(console.error);
    }
    await invoke('convert_files', {
      jobId: activeJobId,
      request: {
        paths,
        mode: submittedProfile.mode,
        format: submittedProfile.format,
      },
      settingsSnapshot: submittedProfile.settings,
    });
    if (wasProcessing) {
      showToast(t('toast.queued', { n: paths.length }), 'success', 2000);
    }
  } catch (e) {
    if (!wasProcessing) {
      sessionQualityProfile = previousQualityProfile;
      isProcessing = false;
      activeJobId = null;
      setState('standby');
    }
    showToast(t('toast.error', { msg: e }), 'error', 4000);
  }
}

// --- Toast ---
function getQualityInfo(profile) {
  const settings = profile?.settings;
  if (!settings || profile.mode !== 'encode') return null;
  switch (profile.format) {
    case 'mp3':
      if (settings.mp3Preset === 'custom') {
        if (settings.mp3Mode === 'vbr') return `VBR Quality ${settings.mp3VbrQuality}`;
        return `Bitrate ${settings.mp3Bitrate}kbps`;
      }
      return `Bitrate ${settings.mp3Preset}kbps`;
    case 'aac':
      if (settings.aacPreset === 'custom') {
        return `Bitrate ${settings.m4aBitrate}kbps`;
      }
      return `Bitrate ${settings.aacPreset}kbps`;
    case 'opus':
      if (settings.opusPreset === 'custom') return `Bitrate ${settings.opusBitrate}kbps`;
      return `Bitrate ${settings.opusPreset}kbps`;
    case 'flac': {
      const lvl = settings.flacPreset === 'custom' ? settings.flacCompression : settings.flacPreset;
      return `Compression ${lvl}`;
    }
    case 'alac': {
      const bits = settings.alacPreset === 'custom' ? settings.alacBitDepth : 16;
      return `Lossless ${bits}bit`;
    }
    default:
      return null;
  }
}

function showCompletionToast(
  successCount,
  errorCount,
  skippedCount = 0,
  mixedProfiles = false,
  results,
  qualityProfile,
) {
  // "__CANCELLED__" はユーザーが上書き確認ダイアログで中止を選んだ印で、UI 上はエラー扱いしない
  const sanitized = (results ?? []).map((r) =>
    r.error === '__CANCELLED__' ? { ...r, success: false, skipped: true, error: null } : r
  );
  const adjErrorCount = sanitized.filter((r) => !r.success && !r.skipped).length;
  errorCount = adjErrorCount;
  results = sanitized;

  if (successCount === 0 && errorCount === 0 && skippedCount === 0) {
    showToast(t('toast.noFiles'), 'warning', 4000);
    return;
  }

  const failures = results?.filter((r) => !r.success && !r.skipped) ?? [];
  failures.forEach((r) => console.error(`[oTo] 変換失敗: ${r.inputPath}\n${r.error}`));

  const lines = [];

  if (successCount > 0) {
    lines.push(successCount === 1 ? t('toast.success.one') : t('toast.success.many', { n: successCount }));
    const qi = mixedProfiles ? null : getQualityInfo(qualityProfile);
    if (qi) lines.push(qi);
  }

  if (errorCount > 0) {
    lines.push(errorCount === 1 ? t('toast.fail.one') : t('toast.fail.many', { n: errorCount }));
  }
  if (skippedCount > 0) lines.push(t('toast.skipped', { n: skippedCount }));

  const type = errorCount > 0
    ? (successCount > 0 ? 'warning' : 'error')
    : (skippedCount > 0 ? 'warning' : 'success');
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

  document.getElementById('menu-silence-detail').addEventListener('click', (e) => {
    if (e.target.id === 'menu-silence-toggle') return;
    hideContextMenu();
    invoke('open_silence_preview').catch(console.error);
  });

  document.getElementById('menu-silence-toggle').addEventListener('click', async (e) => {
    e.stopPropagation();
    if (!appSettings) return;
    appSettings.silenceTrimEnabled = !appSettings.silenceTrimEnabled;
    e.currentTarget.setAttribute('aria-pressed', String(appSettings.silenceTrimEnabled));
    try {
      const latest = await invoke('get_settings');
      await invoke('save_settings', { s: { ...latest, silenceTrimEnabled: appSettings.silenceTrimEnabled } });
    } catch (_) {}
  });

  document.getElementById('menu-activity').addEventListener('click', (e) => {
    if (e.target.id === 'menu-activity-toggle') return;
    hideContextMenu();
    invoke('open_activity_window').catch(console.error);
  });

  document.getElementById('menu-activity-toggle').addEventListener('click', async (e) => {
    e.stopPropagation();
    if (!appSettings) return;
    appSettings.autoOpenActivity = !appSettings.autoOpenActivity;
    e.currentTarget.setAttribute('aria-pressed', String(appSettings.autoOpenActivity));
    try {
      const latest = await invoke('get_settings');
      await invoke('save_settings', { s: { ...latest, autoOpenActivity: appSettings.autoOpenActivity } });
    } catch (_) {}
  });

  document.getElementById('menu-about').addEventListener('click', () => {
    hideContextMenu();
    invoke('open_about_window').catch(console.error);
  });
}

function showContextMenu(x, y) {
  // トグルを現在の設定と同期
  const silenceToggle = document.getElementById('menu-silence-toggle');
  if (silenceToggle) {
    silenceToggle.setAttribute('aria-pressed', String(appSettings?.silenceTrimEnabled ?? false));
  }
  const activityToggle = document.getElementById('menu-activity-toggle');
  if (activityToggle) {
    activityToggle.setAttribute('aria-pressed', String(appSettings?.autoOpenActivity ?? false));
  }
  contextMenu.style.display = 'block';
  const mw = contextMenu.offsetWidth;
  const mh = contextMenu.offsetHeight;
  const ww = window.innerWidth;
  const wh = window.innerHeight;
  // 右・下端ではカーソルの反対側に開き、左・上端も含めて必ず画面内へ収める。
  // メニュー自体が画面より大きい場合は左上を 0 に固定する。
  const left = x + mw > ww ? x - mw : x;
  const top = y + mh > wh ? y - mh : y;
  contextMenu.style.left = Math.max(0, Math.min(left, Math.max(0, ww - mw))) + 'px';
  contextMenu.style.top = Math.max(0, Math.min(top, Math.max(0, wh - mh))) + 'px';
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
function showOverwriteDialog(payload) {
  const filename = typeof payload === 'string' ? payload : payload.filename;
  activeOverwriteDialogId = typeof payload === 'string' ? null : payload.dialogId;
  document.getElementById('overwrite-filename').textContent = filename;
  document.getElementById('overwrite-dialog').classList.remove('hidden');
}

function hideOverwriteDialog() {
  document.getElementById('overwrite-dialog').classList.add('hidden');
}

async function respondOverwrite(choice) {
  const dialogId = activeOverwriteDialogId;
  activeOverwriteDialogId = null;
  overwriteDialogSuspendedForQuit = false;
  await invoke('respond_overwrite', { choice, dialogId }).catch(console.error);
}

function isOverwriteDialogVisible() {
  return !document.getElementById('overwrite-dialog').classList.contains('hidden');
}

async function cancelAllViaOverwrite() {
  jobCancelled = true; // 先にフラグを立てて後続のoverwrite_confirmを自動却下
  hideOverwriteDialog();
  await respondOverwrite('cancel_all');
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
    await respondOverwrite('overwrite');
  });

  document.getElementById('btn-overwrite-rename').addEventListener('click', async () => {
    hideOverwriteDialog();
    await respondOverwrite('rename');
  });

  document.getElementById('btn-overwrite-skip').addEventListener('click', async () => {
    hideOverwriteDialog();
    await respondOverwrite('skip');
  });

  document.getElementById('btn-overwrite-cancel').addEventListener('click', async () => {
    await cancelAllViaOverwrite();
  });

  document.getElementById('btn-quit-resume').addEventListener('click', async () => {
    hideQuitDialog();
    if (activeJobId) {
      await invoke('resume_job', { jobId: activeJobId }).catch(console.error);
    }
    if (overwriteDialogSuspendedForQuit && activeOverwriteDialogId) {
      overwriteDialogSuspendedForQuit = false;
      document.getElementById('overwrite-dialog').classList.remove('hidden');
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
