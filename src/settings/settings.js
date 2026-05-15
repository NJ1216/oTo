import { initI18n, t } from '../i18n/index.js';

const { invoke } = window.__TAURI__.core;

let settings = null;
let customPath = null;

async function init() {
  settings = await invoke('get_settings');
  customPath = settings.customOutputPath || null;
  await initI18n(settings.language || '');
  const title = t('window.settings');
  document.title = title;
  window.__TAURI__.webviewWindow.getCurrentWebviewWindow().setTitle(title);
  populateForm(settings);
}

function updatePreserveVisibility() {
  const dest = document.querySelector('input[name="outputDest"]:checked')?.value;
  const row = document.getElementById('preserve-structure-row');
  if (row) row.style.display = dest === 'source_folder' ? 'none' : '';
}

function populateForm(s) {
  // Output destination
  const destVal = snakeCase(s.outputDest);
  const destRadio = document.querySelector(`input[name="outputDest"][value="${destVal}"]`);
  if (destRadio) destRadio.checked = true;
  updateCustomPathDisplay();
  document.getElementById('preserveFolderStructure').checked = !!s.preserveFolderStructure;
  updatePreserveVisibility();

  // Source file action
  const actionVal = snakeCase(s.sourceFileAction);
  const actionRadio = document.querySelector(`input[name="sourceAction"][value="${actionVal}"]`);
  if (actionRadio) actionRadio.checked = true;

  // Name conflict
  const conflictVal = snakeCase(s.nameConflict);
  const conflictRadio = document.querySelector(`input[name="nameConflict"][value="${conflictVal}"]`);
  if (conflictRadio) conflictRadio.checked = true;

  // Quality presets
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

  // Full power toggle
  document.getElementById('fullPower').checked = !!s.fullPower;

  // Enabled formats
  const enabled = s.enabledFormats || ['mp3', 'aac', 'flac'];
  document.querySelectorAll('.fmt-check').forEach((cb) => {
    cb.checked = enabled.includes(cb.dataset.fmt);
  });

  // Open in Finder
  document.getElementById('openInFinder').checked = s.openInFinder;

  // Language
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

// Wire up all preset selects to show/hide custom panels
['mp3Preset', 'aacPreset', 'opusPreset', 'flacPreset', 'alacPreset'].forEach((id) => {
  document.getElementById(id)?.addEventListener('change', (e) => {
    toggleCustomDetail(id, e.target.value === 'custom');
  });
});

// Output dest radios — show/hide preserve structure option
document.querySelectorAll('input[name="outputDest"]').forEach((r) => {
  r.addEventListener('change', updatePreserveVisibility);
});

// CBR/VBR mode radios
['mp3', 'aac'].forEach((fmt) => {
  document.querySelectorAll(`input[name="${fmt}Mode"]`).forEach((r) => {
    r.addEventListener('change', (e) => toggleCbrVbr(fmt, e.target.value));
  });
});

function snakeCase(val) {
  return val.replace(/([A-Z])/g, (m) => '_' + m.toLowerCase());
}

function updateCustomPathDisplay() {
  const el = document.getElementById('custom-path-display');
  el.textContent = customPath || '';
}

// Folder picker
document.getElementById('pick-folder-btn').addEventListener('click', async () => {
  const path = await invoke('pick_folder');
  if (path) {
    customPath = path;
    updateCustomPathDisplay();
    const radio = document.querySelector('input[name="outputDest"][value="custom"]');
    if (radio) radio.checked = true;
  }
});

// Language real-time preview
document.getElementById('language').addEventListener('change', (e) => {
  initI18n(e.target.value);
});

// Save
document.getElementById('save-btn').addEventListener('click', async () => {
  const outputDest = document.querySelector('input[name="outputDest"]:checked')?.value;
  const sourceFileAction = document.querySelector('input[name="sourceAction"]:checked')?.value;
  const nameConflict = document.querySelector('input[name="nameConflict"]:checked')?.value;

  const updated = {
    ...settings,
    outputDest: outputDest || 'source_folder',
    sourceFileAction: sourceFileAction || 'keep',
    nameConflict: nameConflict || 'auto_rename',
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
    fullPower: document.getElementById('fullPower').checked,
    openInFinder: document.getElementById('openInFinder').checked,
    preserveFolderStructure: document.getElementById('preserveFolderStructure').checked,
    customOutputPath: customPath,
    language: document.getElementById('language').value,
    enabledFormats: (() => {
      const checked = [...document.querySelectorAll('.fmt-check:checked')].map((cb) => cb.dataset.fmt);
      return checked.length > 0 ? checked : ['mp3'];
    })(),
  };

  await invoke('save_settings', { s: updated });
  await window.__TAURI__.event.emit('settings_updated', null);
  await window.__TAURI__.webviewWindow.getCurrentWebviewWindow().close();
});

// Cancel — restore saved language before closing
document.getElementById('cancel-btn').addEventListener('click', async () => {
  if (settings) await initI18n(settings.language || '');
  await window.__TAURI__.webviewWindow.getCurrentWebviewWindow().close();
});

init().catch(console.error);
