import { invoke } from '@tauri-apps/api/core';
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';
import { initI18n, t } from '../i18n/index.js';
import { cargoVersions } from 'virtual:cargo-meta';

const GITHUB_URL = 'https://github.com/NJ1216/oTo';

const BADGE_CLASS = {
  'MIT':        'badge-mit',
  'External':    'badge-external',
  'Apache-2.0': 'badge-apache',
};

const LIBS = [
  { name: 'oTo',             license: 'MIT',       url: GITHUB_URL },
  { sectionKey: 'about.libsSection.rust' },
  { name: 'tauri',           version: cargoVersions.tauri,   license: 'MIT',       url: 'https://tauri.app' },
  { name: 'tauri-plugin-dialog', version: cargoVersions['tauri-plugin-dialog'], license: 'MIT', url: 'https://v2.tauri.app/plugin/dialog/' },
  { name: 'tokio',           version: cargoVersions.tokio,   license: 'MIT',       url: 'https://tokio.rs' },
  { name: 'serde',           version: cargoVersions.serde,   license: 'MIT',       url: 'https://serde.rs' },
  { name: 'serde_json',      version: cargoVersions['serde_json'], license: 'MIT',  url: 'https://github.com/serde-rs/json' },
  { name: 'walkdir',         version: cargoVersions.walkdir, license: 'MIT',       url: 'https://github.com/BurntSushi/walkdir' },
  { name: 'uuid',            version: cargoVersions.uuid,    license: 'MIT',       url: 'https://github.com/uuid-rs/uuid' },
  { name: 'anyhow',          version: cargoVersions.anyhow,  license: 'MIT',       url: 'https://github.com/dtolnay/anyhow' },
  { name: 'dirs',            version: cargoVersions.dirs,    license: 'MIT',       url: 'https://github.com/dirs-dev/dirs-rs' },
  { name: 'base64',          version: cargoVersions.base64,  license: 'MIT',       url: 'https://github.com/marshallpierce/rust-base64' },
  { name: 'sysinfo',         version: cargoVersions.sysinfo, license: 'MIT',       url: 'https://github.com/GuillaumeGomez/sysinfo' },
  { name: 'FFmpeg',          license: 'External', detail: 'LGPL 2.1+ / GPL (depends on distribution)', url: 'https://ffmpeg.org/legal.html' },
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
      name.textContent = item.version ? `${item.name} ${item.version}` : item.name;

      const text = document.createElement('div');
      text.className = 'lib-text';
      text.appendChild(name);
      if (item.detail) {
        const detail = document.createElement('span');
        detail.className = 'lib-detail';
        detail.textContent = item.detail;
        text.appendChild(detail);
      }

      const badge = document.createElement('span');
      badge.className = 'lib-badge ' + (BADGE_CLASS[item.license] ?? 'badge-other');
      badge.textContent = item.license;

      row.appendChild(text);
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

document.getElementById('licenses-btn').addEventListener('click', () => {
  invoke('open_licenses_window').catch(console.error);
});

init().catch(console.error);
