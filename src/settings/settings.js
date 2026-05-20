import { invoke } from '@tauri-apps/api/core';
import { emit } from '@tauri-apps/api/event';
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';
import { initI18n, t } from '../i18n/index.js';

let settings = null;
let customPath = null;

async function init() {
  settings = await invoke('get_settings');
  customPath = settings.customOutputPath || null;
  await initI18n(settings.language || '');
  const title = t('window.settings');
  document.title = title;
  getCurrentWebviewWindow().setTitle(title);
  populateForm(settings);
}

function updateCustomPathRow() {
  const dest = document.getElementById('outputDest').value;
  document.getElementById('custom-path-row')?.classList.toggle('hidden', dest !== 'custom');
  const showPreserve = dest !== 'source_folder';
  document.getElementById('preserve-structure-label')?.classList.toggle('hidden', !showPreserve);
  document.getElementById('preserve-structure-check')?.classList.toggle('hidden', !showPreserve);
}

function updateCustomPathDisplay() {
  const el = document.getElementById('custom-path-display');
  el.textContent = customPath || '';
}

function populateForm(s) {
  document.getElementById('outputDest').value = snakeCase(s.outputDest) || 'source_folder';
  updateCustomPathDisplay();
  updateCustomPathRow();
  document.getElementById('preserveFolderStructure').checked = !!s.preserveFolderStructure;

  const enabledDecode = s.enabledDecodeFormats || ['wav', 'aiff'];
  document.querySelectorAll('.decode-toggle-btn').forEach((btn) => {
    const active = enabledDecode.includes(btn.dataset.fmt);
    btn.classList.toggle('active', active);
    btn.setAttribute('aria-pressed', active ? 'true' : 'false');
  });

  document.getElementById('sourceAction').value = snakeCase(s.sourceFileAction) || 'keep';
  document.getElementById('nameConflict').value = snakeCase(s.nameConflict) || 'auto_rename';

  setPreset('mp3Preset', s.mp3Preset || '192');
  const mp3Mode = s.mp3Mode || 'cbr';
  document.querySelectorAll('input[name="mp3Mode"]').forEach((r) => { r.checked = r.value === mp3Mode; });
  toggleCbrVbr('mp3', mp3Mode);
  document.getElementById('mp3Bitrate').value = String(s.mp3Bitrate || 192);
  document.getElementById('mp3VbrQuality').value = String(s.mp3VbrQuality ?? 4);
  document.getElementById('mp3SampleRate').value = String(s.mp3SampleRate ?? 0);
  document.getElementById('mp3ChannelMode').value = s.mp3ChannelMode || 'auto';

  setPreset('aacPreset', s.aacPreset || '128');
  // FFmpeg ビルトイン aac は VBR 非対応のため UI は CBR 固定
  document.getElementById('m4aBitrate').value = String(s.m4aBitrate || 128);
  document.getElementById('aacSampleRate').value = String(s.aacSampleRate ?? 0);
  document.getElementById('aacChannels').value = String(s.aacChannels ?? 0);

  setPreset('opusPreset', s.opusPreset || '128');
  const opusMode = s.opusMode || 'vbr';
  document.querySelectorAll('input[name="opusMode"]').forEach((r) => { r.checked = r.value === opusMode; });
  document.getElementById('opusBitrate').value = String(s.opusBitrate || 128);
  document.getElementById('opusComplexity').value = String(s.opusComplexity ?? 5);

  setPreset('flacPreset', s.flacPreset || '5');
  document.getElementById('flacCompression').value = String(s.flacCompression ?? 5);

  setPreset('alacPreset', s.alacPreset || '');
  document.getElementById('alacBitDepth').value = String(s.alacBitDepth || 16);

  document.getElementById('silenceTrimEnabled').checked = !!s.silenceTrimEnabled;

  const enabled = s.enabledFormats || ['mp3', 'aac', 'flac'];
  document.querySelectorAll('#encode-fmt-group .fmt-toggle-btn').forEach((btn) => {
    const active = enabled.includes(btn.dataset.fmt);
    btn.classList.toggle('active', active);
    btn.setAttribute('aria-pressed', active ? 'true' : 'false');
  });

  document.getElementById('openInFinder').checked = s.openInFinder;
  document.getElementById('language').value = s.language || '';
  document.getElementById('maxMemoryMb').value = String(s.maxMemoryMb ?? 512);
}

function toggleCbrVbr(fmt, mode) {
  const cbrEl = document.getElementById(`${fmt}CbrRows`);
  const vbrEl = document.getElementById(`${fmt}VbrRows`);
  if (cbrEl) cbrEl.style.display = mode === 'cbr' ? '' : 'none';
  if (vbrEl) vbrEl.style.display = mode === 'vbr' ? '' : 'none';
}

function setPreset(selectId, value) {
  const el = document.getElementById(selectId);
  if (!el) return;
  el.value = value;
  toggleCustomDetail(selectId, value === 'custom');
}

function toggleCustomDetail(selectId, show) {
  const detailId = selectId.replace('Preset', 'Custom');
  const detail = document.getElementById(detailId);
  if (detail) detail.classList.toggle('open', show);
}

['mp3Preset', 'aacPreset', 'opusPreset', 'flacPreset', 'alacPreset'].forEach((id) => {
  document.getElementById(id)?.addEventListener('change', (e) => {
    toggleCustomDetail(id, e.target.value === 'custom');
  });
});

document.getElementById('outputDest').addEventListener('change', updateCustomPathRow);

// aac は VBR 非対応のため mp3 のみ CBR/VBR トグルを扱う
document.querySelectorAll('input[name="mp3Mode"]').forEach((r) => {
  r.addEventListener('change', (e) => toggleCbrVbr('mp3', e.target.value));
});

document.querySelectorAll('#encode-fmt-group .fmt-toggle-btn').forEach((btn) => {
  btn.addEventListener('click', () => {
    const activeCount = document.querySelectorAll('#encode-fmt-group .fmt-toggle-btn.active').length;
    if (btn.classList.contains('active') && activeCount <= 1) return;
    btn.classList.toggle('active');
    btn.setAttribute('aria-pressed', btn.classList.contains('active') ? 'true' : 'false');
  });
});

document.querySelectorAll('.decode-toggle-btn').forEach((btn) => {
  btn.addEventListener('click', () => {
    const activeCount = document.querySelectorAll('.decode-toggle-btn.active').length;
    if (btn.classList.contains('active') && activeCount <= 1) return;
    btn.classList.toggle('active');
    btn.setAttribute('aria-pressed', btn.classList.contains('active') ? 'true' : 'false');
  });
});

function snakeCase(val) {
  if (!val) return val;
  return val.replace(/([A-Z])/g, (m) => '_' + m.toLowerCase());
}

document.getElementById('pick-folder-btn').addEventListener('click', async () => {
  const path = await invoke('pick_folder');
  if (path) {
    customPath = path;
    updateCustomPathDisplay();
    document.getElementById('outputDest').value = 'custom';
    updateCustomPathRow();
  }
});

document.getElementById('open-preview-btn').addEventListener('click', async () => {
  await invoke('open_silence_preview');
});

document.getElementById('language').addEventListener('change', (e) => {
  initI18n(e.target.value);
});

function collectFormValues() {
  const int = (id, fallback = 0) => parseInt(document.getElementById(id)?.value, 10) || fallback;
  const str = (id, fallback = '') => document.getElementById(id)?.value || fallback;
  const chk = (id) => !!document.getElementById(id)?.checked;
  const radio = (name, fallback) => document.querySelector(`input[name="${name}"]:checked`)?.value || fallback;

  return {
    outputDest: str('outputDest', 'source_folder'),
    sourceFileAction: str('sourceAction', 'keep'),
    nameConflict: str('nameConflict', 'auto_rename'),
    mp3Preset: str('mp3Preset'),
    mp3Mode: radio('mp3Mode', 'cbr'),
    mp3Bitrate: int('mp3Bitrate', 192),
    mp3VbrQuality: int('mp3VbrQuality', 4),
    mp3SampleRate: int('mp3SampleRate'),
    mp3ChannelMode: str('mp3ChannelMode'),
    aacPreset: str('aacPreset'),
    // aacMode/aacVbrQuality はビルトイン aac エンコーダで効かないため固定値を渡す
    aacMode: 'cbr',
    m4aBitrate: int('m4aBitrate', 128),
    aacVbrQuality: 4,
    aacSampleRate: int('aacSampleRate'),
    aacChannels: int('aacChannels'),
    opusPreset: str('opusPreset'),
    opusMode: radio('opusMode', 'vbr'),
    opusBitrate: int('opusBitrate', 128),
    opusComplexity: int('opusComplexity'),
    flacPreset: str('flacPreset'),
    flacCompression: int('flacCompression'),
    alacPreset: str('alacPreset'),
    alacBitDepth: int('alacBitDepth', 16),
    silenceTrimEnabled: chk('silenceTrimEnabled'),
    openInFinder: chk('openInFinder'),
    preserveFolderStructure: chk('preserveFolderStructure'),
    customOutputPath: customPath,
    language: str('language'),
    maxMemoryMb: int('maxMemoryMb', 512),
    enabledFormats: (() => {
      const checked = [...document.querySelectorAll('#encode-fmt-group .fmt-toggle-btn.active')].map((b) => b.dataset.fmt);
      return checked.length > 0 ? checked : ['mp3'];
    })(),
    enabledDecodeFormats: (() => {
      const checked = [...document.querySelectorAll('.decode-toggle-btn.active')].map((b) => b.dataset.fmt);
      return checked.length > 0 ? checked : ['wav'];
    })(),
  };
}

document.getElementById('save-btn').addEventListener('click', async () => {
  // 設定ウィンドウ起動後にプレビュー側で更新された値（silenceTrimDb 等）を巻き戻さない
  // ようにするため、保存直前に最新を再取得してからフォーム値をマージする。
  let latest = settings;
  try { latest = await invoke('get_settings'); } catch (_) {}
  await invoke('save_settings', { s: { ...latest, ...collectFormValues() } });
  await emit('settings_updated');
  await getCurrentWebviewWindow().close();
});

document.getElementById('cancel-btn').addEventListener('click', async () => {
  if (settings) await initI18n(settings.language || '');
  await getCurrentWebviewWindow().close();
});

init().catch(console.error);
