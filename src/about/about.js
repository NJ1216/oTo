const { invoke } = window.__TAURI__.core;

const LICENSES = `\
Tauri
  Version: 2.x
  License: MIT / Apache-2.0
  https://tauri.app

FFmpeg
  Version: 8.x (system)
  License: LGPL 2.1+
  https://ffmpeg.org

tokio
  Version: 1.x
  License: MIT
  https://tokio.rs

serde / serde_json
  Version: 1.x
  License: MIT / Apache-2.0
  https://serde.rs

walkdir
  Version: 2.x
  License: MIT / Unlicense
  https://github.com/BurntSushi/walkdir

uuid
  Version: 1.x
  License: MIT / Apache-2.0
  https://github.com/uuid-rs/uuid

anyhow
  Version: 1.x
  License: MIT / Apache-2.0
  https://github.com/dtolnay/anyhow
`;

async function init() {
  const version = await invoke('get_app_version');
  document.getElementById('version-text').textContent = version;
  document.getElementById('licenses').value = LICENSES;
}

document.getElementById('close-btn').addEventListener('click', () => {
  window.__TAURI__.webviewWindow.getCurrentWebviewWindow().close();
});

init().catch(console.error);
