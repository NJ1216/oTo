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
    btn.classList.toggle('active', enabledDecode.includes(btn.dataset.fmt));
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
  const aacMode = s.aacMode || 'cbr';
  document.querySelectorAll('input[name="aacMode"]').forEach((r) => { r.checked = r.value === aacMode; });
  toggleCbrVbr('aac', aacMode);
  document.getElementById('m4aBitrate').value = String(s.m4aBitrate || 128);
  document.getElementById('aacVbrQuality').value = String(s.aacVbrQuality ?? 4);
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
    btn.classList.toggle('active', enabled.includes(btn.dataset.fmt));
  });

  document.getElementById('openInFinder').checked = s.openInFinder;
  document.getElementById('language').value = s.language || '';
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

['mp3', 'aac'].forEach((fmt) => {
  document.querySelectorAll(`input[name="${fmt}Mode"]`).forEach((r) => {
    r.addEventListener('change', (e) => toggleCbrVbr(fmt, e.target.value));
  });
});

document.querySelectorAll('#encode-fmt-group .fmt-toggle-btn').forEach((btn) => {
  btn.addEventListener('click', () => {
    const activeCount = document.querySelectorAll('#encode-fmt-group .fmt-toggle-btn.active').length;
    if (btn.classList.contains('active') && activeCount <= 1) return;
    btn.classList.toggle('active');
  });
});

document.querySelectorAll('.decode-toggle-btn').forEach((btn) => {
  btn.addEventListener('click', () => {
    const activeCount = document.querySelectorAll('.decode-toggle-btn.active').length;
    if (btn.classList.contains('active') && activeCount <= 1) return;
    btn.classList.toggle('active');
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

document.getElementById('save-btn').addEventListener('click', async () => {
  const updated = {
    ...settings,
    outputDest: document.getElementById('outputDest').value || 'source_folder',
    sourceFileAction: document.getElementById('sourceAction').value || 'keep',
    nameConflict: document.getElementById('nameConflict').value || 'auto_rename',
    mp3Preset: document.getElementById('mp3Preset').value,
    mp3Mode: document.querySelector('input[name="mp3Mode"]:checked')?.value || 'cbr',
    mp3Bitrate: parseInt(document.getElementById('mp3Bitrate').value, 10) || 192,
    mp3VbrQuality: parseInt(document.getElementById('mp3VbrQuality').value, 10) || 4,
    mp3SampleRate: parseInt(document.getElementById('mp3SampleRate').value, 10),
    mp3ChannelMode: document.getElementById('mp3ChannelMode').value,
    aacPreset: document.getElementById('aacPreset').value,
    aacMode: document.querySelector('input[name="aacMode"]:checked')?.value || 'cbr',
    m4aBitrate: parseInt(document.getElementById('m4aBitrate').value, 10) || 128,
    aacVbrQuality: parseInt(document.getElementById('aacVbrQuality').value, 10) || 4,
    aacSampleRate: parseInt(document.getElementById('aacSampleRate').value, 10),
    aacChannels: parseInt(document.getElementById('aacChannels').value, 10),
    opusPreset: document.getElementById('opusPreset').value,
    opusMode: document.querySelector('input[name="opusMode"]:checked')?.value || 'vbr',
    opusBitrate: parseInt(document.getElementById('opusBitrate').value, 10) || 128,
    opusComplexity: parseInt(document.getElementById('opusComplexity').value, 10),
    flacPreset: document.getElementById('flacPreset').value,
    flacCompression: parseInt(document.getElementById('flacCompression').value, 10),
    alacPreset: document.getElementById('alacPreset').value,
    alacBitDepth: parseInt(document.getElementById('alacBitDepth').value, 10) || 16,
    silenceTrimEnabled: document.getElementById('silenceTrimEnabled').checked,
    openInFinder: document.getElementById('openInFinder').checked,
    preserveFolderStructure: document.getElementById('preserveFolderStructure').checked,
    customOutputPath: customPath,
    language: document.getElementById('language').value,
    enabledFormats: (() => {
      const checked = [...document.querySelectorAll('#encode-fmt-group .fmt-toggle-btn.active')].map((btn) => btn.dataset.fmt);
      return checked.length > 0 ? checked : ['mp3'];
    })(),
    enabledDecodeFormats: (() => {
      const checked = [...document.querySelectorAll('.decode-toggle-btn.active')].map((btn) => btn.dataset.fmt);
      return checked.length > 0 ? checked : ['wav'];
    })(),
  };

  await invoke('save_settings', { s: updated });
  await emit('settings_updated');
  await getCurrentWebviewWindow().close();
});

document.getElementById('cancel-btn').addEventListener('click', async () => {
  if (settings) await initI18n(settings.language || '');
  await getCurrentWebviewWindow().close();
});

init().catch(console.error);
