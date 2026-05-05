import { initI18n } from '../i18n/index.js';

const { invoke } = window.__TAURI__.core;

let settings = null;
let customPath = null;

async function init() {
  settings = await invoke('get_settings');
  customPath = settings.customOutputPath || null;
  await initI18n(settings.language || '');
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

  // Quality
  document.getElementById('mp3Bitrate').value = String(s.mp3Bitrate);
  document.getElementById('m4aBitrate').value = String(s.m4aBitrate);
  document.getElementById('flacCompression').value = String(s.flacCompression);

  // Parallel count
  document.getElementById('parallelCount').value = String(s.parallelCount);

  // Open in Finder
  document.getElementById('openInFinder').checked = s.openInFinder;

  // Language
  document.getElementById('language').value = s.language || '';
}

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
    mp3Bitrate: parseInt(document.getElementById('mp3Bitrate').value, 10),
    m4aBitrate: parseInt(document.getElementById('m4aBitrate').value, 10),
    flacCompression: parseInt(document.getElementById('flacCompression').value, 10),
    parallelCount: Math.max(1, parseInt(document.getElementById('parallelCount').value, 10) || 1),
    openInFinder: document.getElementById('openInFinder').checked,
    customOutputPath: customPath,
    language: document.getElementById('language').value,
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
