import { readFileSync } from 'fs';
import { resolve } from 'path';

function parseCargoVersions(toml) {
  // [dependencies] / [target.*.dependencies] / [build-dependencies] のみを対象にし、
  // [package] の name/description/edition 等を拾わないようにする。
  const versions = {};
  const sectionRe = /^\[(?<section>[^\]]+)\]\s*$/gm;
  const depSections = [];
  let s;
  while ((s = sectionRe.exec(toml)) !== null) {
    const name = s.groups.section.trim();
    if (name === 'dependencies' || name === 'build-dependencies' || /\.dependencies$/.test(name)) {
      depSections.push({ start: s.index + s[0].length });
    }
  }
  for (let i = 0; i < depSections.length; i++) {
    let end = toml.length;
    const nextSectionRe = /^\[[^\]]+\]\s*$/gm;
    nextSectionRe.lastIndex = depSections[i].start;
    const next = nextSectionRe.exec(toml);
    if (next) end = next.index;
    depSections[i].end = end;
  }

  for (const { start, end } of depSections) {
    const body = toml.slice(start, end);
    const tableRe = /^([\w-]+)\s*=\s*\{[^}]*version\s*=\s*"([^"]+)"/gm;
    let m;
    while ((m = tableRe.exec(body)) !== null) versions[m[1]] = m[2];
    const simpleRe = /^([\w-]+)\s*=\s*"([^"]+)"\s*$/gm;
    while ((m = simpleRe.exec(body)) !== null) if (!versions[m[1]]) versions[m[1]] = m[2];
  }
  return versions;
}

export default function cargoMetaPlugin() {
  const virtualId = 'virtual:cargo-meta';
  const resolvedId = '\0' + virtualId;

  return {
    name: 'cargo-meta',
    resolveId(id) {
      if (id === virtualId) return resolvedId;
    },
    load(id) {
      if (id !== resolvedId) return;
      const toml = readFileSync(resolve(process.cwd(), 'src-tauri/Cargo.toml'), 'utf-8');
      const versions = parseCargoVersions(toml);
      return `export const cargoVersions = ${JSON.stringify(versions)};`;
    },
  };
}
