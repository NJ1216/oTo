#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";

const repoRoot = process.cwd();
const outputPath = path.join(repoRoot, "src", "licenses.html");
const cargoArgs = [
  "about",
  "generate",
  "--locked",
  "--offline",
  "--fail",
  "--format",
  "json",
  "--config",
  "licenses/about.toml",
  "--manifest-path",
  "src-tauri/Cargo.toml",
];

function runCargoAbout() {
  const result = spawnSync("cargo", cargoArgs, {
    cwd: repoRoot,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    maxBuffer: 50 * 1024 * 1024,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error((result.stderr || result.stdout || "cargo about failed").trim());
  }
  return JSON.parse(result.stdout);
}

function escapeHtml(value) {
  return String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

function licenseAnchor(id) {
  return id.replaceAll(/[^A-Za-z0-9_-]/g, "-");
}

function copyrightLines(text) {
  return [...new Set(
    text
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => /(?:copyright|©).*\d{4}/i.test(line))
  )];
}

function trimTrailingWhitespace(text) {
  return String(text)
    .split("\n")
    .map((line) => line.trimEnd())
    .join("\n");
}

function crateLink(krate) {
  const url = krate.repository || `https://crates.io/crates/${krate.name}`;
  return `<a href="${escapeHtml(url)}">${escapeHtml(krate.name)} ${escapeHtml(krate.version)}</a>`;
}

function renderLicenseGroup(overview, licenses) {
  const entries = overview.indices.map((index) => licenses[index]).filter(Boolean);
  const packages = new Map();

  for (const entry of entries) {
    for (const used of entry.used_by || []) {
      const krate = used.crate;
      const key = krate.id || `${krate.name}@${krate.version}`;
      const existing = packages.get(key) || { krate, notices: new Set() };
      for (const line of copyrightLines(entry.text || "")) existing.notices.add(line);
      packages.set(key, existing);
    }
  }

  const sorted = [...packages.values()].sort((a, b) =>
    a.krate.name.localeCompare(b.krate.name) || a.krate.version.localeCompare(b.krate.version)
  );
  const packageItems = sorted.map(({ krate }) => `<li>${crateLink(krate)}</li>`).join("");
  const noticeItems = sorted
    .filter(({ notices }) => notices.size > 0)
    .map(({ krate, notices }) =>
      `<li><strong>${escapeHtml(krate.name)} ${escapeHtml(krate.version)}</strong><br>${[...notices].map(escapeHtml).join("<br>")}</li>`
    )
    .join("");

  return `<details class="license-group" id="${licenseAnchor(overview.id)}">
    <summary>${escapeHtml(overview.name)} <span data-package-count="${sorted.length}"></span></summary>
    <div class="license-content">
      <h3 data-i18n="packages"></h3>
      <ul class="package-list">${packageItems}</ul>
${noticeItems ? `      <details class="subsection"><summary data-i18n="copyrights"></summary><ul class="copyright-list">${noticeItems}</ul></details>` : ""}
      <details class="subsection"><summary data-i18n="licenseText"></summary><pre>${escapeHtml(trimTrailingWhitespace(overview.text || ""))}</pre></details>
    </div>
  </details>`;
}

function buildHtml(data) {
  const groups = data.overview
    .map((overview) => renderLicenseGroup(overview, data.licenses))
    .join("\n");
  const overview = data.overview
    .map((item) => `<li><a href="#${licenseAnchor(item.id)}">${escapeHtml(item.name)} (${item.count})</a></li>`)
    .join("");

  return `<!doctype html>
<html lang="ja">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>oTo — Third-Party Licenses</title>
  <style>
    :root { color-scheme: dark; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; background: #171722; color: #ebebf5; }
    body { margin: 0; background: #171722; }
    main { max-width: 860px; margin: 0 auto; padding: 32px 24px 56px; }
    header { padding: 20px; border: 1px solid #343447; border-radius: 14px; background: linear-gradient(135deg, #22223a, #1b2637); }
    h1 { margin: 0 0 8px; font-size: 24px; }
    h2 { margin-top: 34px; font-size: 17px; }
    h3 { font-size: 14px; margin: 18px 0 9px; }
    p { color: #c7c7d5; line-height: 1.65; }
    a { color: #7dd3fc; }
    .overview { display: flex; flex-wrap: wrap; gap: 8px; padding: 0; list-style: none; }
    .overview a { display: block; padding: 7px 10px; border-radius: 999px; background: #27273a; text-decoration: none; font-size: 13px; }
    .license-group { margin: 12px 0; border: 1px solid #343447; border-radius: 10px; background: #20202e; overflow: hidden; }
    .license-group > summary { cursor: pointer; padding: 13px 15px; font-weight: 600; }
    summary span { color: #a7a7b8; font-size: 12px; font-weight: 400; }
    .license-content { padding: 0 15px 16px; }
    .package-list { columns: 3 170px; padding-left: 20px; line-height: 1.65; }
    .copyright-list { padding-left: 20px; color: #c7c7d5; line-height: 1.55; }
    .copyright-list li { margin: 8px 0; }
    .subsection { margin-top: 14px; border-top: 1px solid #343447; padding-top: 12px; }
    .subsection summary { cursor: pointer; color: #bae6fd; }
    pre { max-height: 260px; overflow: auto; padding: 13px; border-radius: 8px; background: #15151f; color: #d9d9e5; white-space: pre-wrap; font: 12px/1.45 ui-monospace, SFMono-Regular, Menlo, monospace; }
    .muted { color: #a7a7b8; font-size: 13px; }
  </style>
</head>
<body>
  <main>
    <header>
      <h1 data-i18n="title"></h1>
      <p data-i18n="intro"></p>
    </header>
    <h2 data-i18n="javascript"></h2>
    <details class="license-group" open>
      <summary>MIT License <span data-package-count="1"></span></summary>
      <div class="license-content">
        <p><a href="https://www.npmjs.com/package/@tauri-apps/api">@tauri-apps/api</a> — <span data-i18n="tauriApiDescription"></span></p>
        <details class="subsection"><summary data-i18n="licenseText"></summary><pre>MIT License

Copyright (c) 2017 - Present Tauri Apps Contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.</pre></details>
      </div>
    </details>
    <h2 data-i18n="rust"></h2>
    <p class="muted" data-i18n="rustIntro"></p>
    <ul class="overview">${overview}</ul>
    ${groups}
  </main>
  <script>
    const translations = {
      ja: {
        title: "第三者ライセンス",
        intro: "oToに含まれる依存ソフトウェアに関する通知です。複数ライセンスを選べる依存はMITを選択しています。",
        javascript: "JavaScript",
        rust: "Rust",
        rustIntro: "Cargo.lockから生成されています。ライセンスの種類ごとにまとめ、各パッケージの著作権表示を保持しています。",
        packages: "対象パッケージ",
        copyrights: "著作権表示",
        licenseText: "ライセンス本文",
        packageCount: "パッケージ",
        tauriApiDescription: "MITを選択（Apache-2.0 OR MIT）",
      },
      en: {
        title: "Third-Party Licenses",
        intro: "Notices for software dependencies included in oTo. Where a dependency offers multiple licenses, MIT is selected.",
        javascript: "JavaScript",
        rust: "Rust",
        rustIntro: "Generated from Cargo.lock. Packages are grouped by license type while retaining copyright notices.",
        packages: "Packages",
        copyrights: "Copyright notices",
        licenseText: "License text",
        packageCount: "packages",
        tauriApiDescription: "MIT selected (Apache-2.0 OR MIT)",
      },
    };
    let lang = "ja";
    try {
      if (localStorage.getItem("oto_lang") === "en") lang = "en";
    } catch (_) {}
    const text = translations[lang];
    document.documentElement.lang = lang;
    document.title = "oTo — " + text.title;
    document.querySelectorAll("[data-i18n]").forEach((element) => {
      element.textContent = text[element.dataset.i18n];
    });
    document.querySelectorAll("[data-package-count]").forEach((element) => {
      element.textContent = element.dataset.packageCount + " " + text.packageCount;
    });
  </script>
</body>
</html>
`;
}

try {
  const data = runCargoAbout();
  fs.writeFileSync(outputPath, buildHtml(data), "utf8");
  console.log(`[licenses] generated ${path.relative(repoRoot, outputPath)}`);
} catch (error) {
  console.error(`[licenses] ${error.message}`);
  process.exit(1);
}
