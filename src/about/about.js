import { initI18n } from '../i18n/index.js';

const { invoke } = window.__TAURI__.core;

const BADGE_CLASS = {
  'MIT':        'badge-mit',
  'LGPL 2.1+':  'badge-lgpl',
  'Apache-2.0': 'badge-apache',
};

const LIBS = [
  { name: 'oTo',             license: 'MIT',       url: 'https://github.com/' },
  { section: 'Rust（バックエンド）' },
  { name: 'tauri',           license: 'MIT',       url: 'https://tauri.app' },
  { name: 'FFmpeg',          license: 'LGPL 2.1+', url: 'https://ffmpeg.org' },
  { name: 'tokio',           license: 'MIT',       url: 'https://tokio.rs' },
  { name: 'serde',           license: 'MIT',       url: 'https://serde.rs' },
  { name: 'walkdir',         license: 'MIT',       url: 'https://github.com/BurntSushi/walkdir' },
  { name: 'uuid',            license: 'MIT',       url: 'https://github.com/uuid-rs/uuid' },
  { name: 'anyhow',          license: 'MIT',       url: 'https://github.com/dtolnay/anyhow' },
  { name: 'dirs',            license: 'MIT',       url: 'https://github.com/dirs-dev/dirs-rs' },
  { section: 'JavaScript（フロントエンド）' },
  { name: '@tauri-apps/api', license: 'MIT',       url: 'https://tauri.app' },
];

function buildLibList() {
  const container = document.getElementById('lib-list');
  for (const item of LIBS) {
    if (item.section) {
      const el = document.createElement('div');
      el.className = 'lib-section-header';
      el.textContent = item.section;
      container.appendChild(el);
    } else {
      const row = document.createElement('div');
      row.className = 'lib-row';

      const name = document.createElement('span');
      name.className = 'lib-name';
      name.textContent = item.name;

      const badge = document.createElement('span');
      badge.className = 'lib-badge ' + (BADGE_CLASS[item.license] ?? 'badge-other');
      badge.textContent = item.license;

      row.appendChild(name);
      row.appendChild(badge);
      container.appendChild(row);
    }
  }
}

async function init() {
  let lang = '';
  try {
    const settings = await invoke('get_settings');
    lang = settings.language || '';
  } catch (_) {}

  await initI18n(lang);
  buildLibList();

  const version = await invoke('get_app_version');
  document.getElementById('version-text').textContent = version;
}

document.getElementById('close-btn').addEventListener('click', () => {
  window.__TAURI__.webviewWindow.getCurrentWebviewWindow().close();
});

init().catch(console.error);
