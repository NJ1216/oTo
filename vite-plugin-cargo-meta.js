import { readFileSync } from 'fs';
import { resolve } from 'path';

function parseCargoVersions(toml) {
  const versions = {};
  // name = { version = "x.y", ... }
  const tableRe = /^([\w-]+)\s*=\s*\{[^}]*version\s*=\s*"([^"]+)"/gm;
  let m;
  while ((m = tableRe.exec(toml)) !== null) versions[m[1]] = m[2];
  // name = "x.y"
  const simpleRe = /^([\w-]+)\s*=\s*"([^"]+)"/gm;
  while ((m = simpleRe.exec(toml)) !== null) if (!versions[m[1]]) versions[m[1]] = m[2];
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
