const SUPPORTED = ['ja', 'en'];
let _dict = {};
const LOCALE_LOADERS = {
  ja: () => import('./ja.js'),
  en: () => import('./en.js'),
};

function resolve(lang) {
  if (lang && SUPPORTED.includes(lang)) return lang;
  const sys = (navigator.language || '').split('-')[0].toLowerCase();
  return SUPPORTED.includes(sys) ? sys : 'ja';
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
  document.querySelectorAll('[data-i18n-label]').forEach((el) => {
    const key = el.dataset.i18nLabel;
    const translated = t(key);
    if (translated !== key) el.setAttribute('aria-label', translated);
  });
}

export async function initI18n(lang) {
  const resolved = resolve(lang);
  const loader = LOCALE_LOADERS[resolved] || LOCALE_LOADERS.ja;
  const mod = await loader();
  _dict = mod.default;
  document.documentElement.lang = resolved;
  try { localStorage.setItem('oto_lang', resolved); } catch (_) {}
  applyDOM();
}
