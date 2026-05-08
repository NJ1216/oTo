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

function populateForm(s) {
  // Output destination
  const destVal = snakeCase(s.outputDest);
  const destRadio = document.querySelector(`input[name="outputDest"][value="${destVal}"]`);
  if (destRadio) destRadio.checked = true;
  updateCustomPathDisplay();

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
  document.getElementById('mp3Bitrate').value = String(s.mp3Bitrate || 192);
  document.getElementById('mp3SampleRate').value = String(s.mp3SampleRate ?? 0);
  document.getElementById('mp3ChannelMode').value = s.mp3ChannelMode || 'joint_stereo';

  setPreset('aacPreset', s.aacPreset || '128');
  document.getElementById('m4aBitrate').value = String(s.m4aBitrate || 128);
  document.getElementById('aacSampleRate').value = String(s.aacSampleRate ?? 0);
  document.getElementById('aacChannels').value = String(s.aacChannels ?? 0);

  setPreset('oggPreset', s.oggPreset || 'q4');
  document.getElementById('oggQuality').value = String(s.oggQuality ?? 4);

  setPreset('opusPreset', s.opusPreset || '128');
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
['mp3Preset', 'aacPreset', 'oggPreset', 'opusPreset', 'flacPreset', 'alacPreset'].forEach((id) => {
  document.getElementById(id)?.addEventListener('change', (e) => {
    toggleCustomDetail(id, e.target.value === 'custom');
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
    mp3Bitrate: parseInt(document.getElementById('mp3Bitrate').value, 10) || 192,
    mp3SampleRate: parseInt(document.getElementById('mp3SampleRate').value, 10),
    mp3ChannelMode: document.getElementById('mp3ChannelMode').value,
    aacPreset: document.getElementById('aacPreset').value,
    m4aBitrate: parseInt(document.getElementById('m4aBitrate').value, 10) || 128,
    aacSampleRate: parseInt(document.getElementById('aacSampleRate').value, 10),
    aacChannels: parseInt(document.getElementById('aacChannels').value, 10),
    oggPreset: document.getElementById('oggPreset').value,
    oggQuality: parseFloat(document.getElementById('oggQuality').value) || 4,
    opusPreset: document.getElementById('opusPreset').value,
    opusBitrate: parseInt(document.getElementById('opusBitrate').value, 10) || 128,
    opusComplexity: parseInt(document.getElementById('opusComplexity').value, 10),
    flacPreset: document.getElementById('flacPreset').value,
    flacCompression: parseInt(document.getElementById('flacCompression').value, 10),
    alacPreset: document.getElementById('alacPreset').value,
    alacBitDepth: parseInt(document.getElementById('alacBitDepth').value, 10) || 16,
    fullPower: document.getElementById('fullPower').checked,
    openInFinder: document.getElementById('openInFinder').checked,
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
