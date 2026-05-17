import { invoke } from '@tauri-apps/api/core';
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';
import { initI18n, t } from '../i18n/index.js';

const GITHUB_URL = 'https://github.com/NJ1216/oTo';

const BADGE_CLASS = {
  'MIT':        'badge-mit',
  'LGPL 2.1+':  'badge-lgpl',
  'Apache-2.0': 'badge-apache',
};

const LIBS = [
  { name: 'oTo',             license: 'MIT',       url: GITHUB_URL },
  { sectionKey: 'about.libsSection.rust' },
  { name: 'tauri',           license: 'MIT',       url: 'https://tauri.app' },
  { name: 'FFmpeg',          license: 'LGPL 2.1+', url: 'https://ffmpeg.org' },
  { name: 'tokio',           license: 'MIT',       url: 'https://tokio.rs' },
  { name: 'serde',           license: 'MIT',       url: 'https://serde.rs' },
  { name: 'walkdir',         license: 'MIT',       url: 'https://github.com/BurntSushi/walkdir' },
  { name: 'uuid',            license: 'MIT',       url: 'https://github.com/uuid-rs/uuid' },
  { name: 'anyhow',          license: 'MIT',       url: 'https://github.com/dtolnay/anyhow' },
  { name: 'dirs',            license: 'MIT',       url: 'https://github.com/dirs-dev/dirs-rs' },
  { sectionKey: 'about.libsSection.js' },
  { name: '@tauri-apps/api', license: 'MIT',       url: 'https://tauri.app' },
];

function openUrl(url) {
  invoke('open_url', { url }).catch(console.error);
}

function buildLibList() {
  const container = document.getElementById('lib-list');
  for (const item of LIBS) {
    if (item.sectionKey) {
      const el = document.createElement('div');
      el.className = 'lib-section-header';
      el.textContent = t(item.sectionKey);
      container.appendChild(el);
    } else {
      const row = document.createElement('div');
      row.className = 'lib-row' + (item.url ? ' lib-row-link' : '');

      const name = document.createElement('span');
      name.className = 'lib-name';
      name.textContent = item.name;

      const badge = document.createElement('span');
      badge.className = 'lib-badge ' + (BADGE_CLASS[item.license] ?? 'badge-other');
      badge.textContent = item.license;

      row.appendChild(name);
      row.appendChild(badge);

      if (item.url) {
        row.addEventListener('click', () => openUrl(item.url));
      }

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
  const title = t('window.about');
  document.title = title;
  getCurrentWebviewWindow().setTitle(title);
  buildLibList();

  const version = await invoke('get_app_version');
  document.getElementById('version-text').textContent = version;

  document.getElementById('github-link').addEventListener('click', (e) => {
    e.preventDefault();
    openUrl(GITHUB_URL);
  });
}

document.getElementById('close-btn').addEventListener('click', () => {
  getCurrentWebviewWindow().close();
});

init().catch(console.error);
