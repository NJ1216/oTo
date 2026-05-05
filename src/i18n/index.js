const SUPPORTED = ['ja', 'en'];
let _dict = {};

function resolve(lang) {
  if (lang && SUPPORTED.includes(lang)) return lang;
  const sys = (navigator.language || '').split('-')[0].toLowerCase();
  return SUPPORTED.includes(sys) ? sys : 'en';
}

export function t(key, vars = {}) {
  const parts = key.split('.');
  let node = _dict;
  for (const p of parts) {
    if (node == null || typeof node !== 'object') return key;
    node = node[p];
  }
  if (typeof node !== 'string') return key;
  return node.replace(/\{(\w+)\}/g, (_, k) => (vars[k] ?? _));
}

function applyDOM() {
  document.querySelectorAll('[data-i18n]').forEach((el) => {
    const key = el.dataset.i18n;
    const translated = t(key);
    if (translated !== key) el.textContent = translated;
  });
}

export async function initI18n(lang) {
  const resolved = resolve(lang);
  const mod = await import(`./${resolved}.js`);
  _dict = mod.default;
  applyDOM();
}
