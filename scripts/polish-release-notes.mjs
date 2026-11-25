#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { spawnSync } from "node:child_process";

const args = process.argv.slice(2);

if (args.length === 0) {
  console.error(
    "Usage: node scripts/polish-release-notes.mjs <vYY.M.D> [--output <path>] [--apply]"
  );
  process.exit(1);
}

const applyIndex = args.indexOf("--apply");
const outputIndex = args.indexOf("--output");
const tagArg = args.find((arg, index) =>
  !arg.startsWith("--") && index !== outputIndex + 1
);

if (!tagArg) {
  console.error(
    "Usage: node scripts/polish-release-notes.mjs <vYY.M.D> [--output <path>] [--apply]"
  );
  process.exit(1);
}

if (!/^v?\d{2}\.\d{1,2}\.\d{1,2}$/.test(tagArg)) {
  console.error("Tag must be in format vYY.M.D");
  process.exit(1);
}

const tag = tagArg.startsWith("v") ? tagArg : `v${tagArg}`;
const applyNotes = applyIndex >= 0;
const outputPath =
  outputIndex >= 0 && args[outputIndex + 1] ? args[outputIndex + 1] : "";

function run(cmd, args = [], options = {}) {
  const result = spawnSync(cmd, args, {
    cwd: options.cwd || process.cwd(),
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    shell: false,
  });

  if (result.error) {
    throw result.error;
  }

  if (result.status !== 0) {
    const msg = (result.stderr || result.stdout || "command failed").toString().trim();
    throw new Error(`${cmd} ${args.join(" ")} failed: ${msg}`);
  }

  return (result.stdout || "").toString().trim();
}

function detectPreviousTag() {
  try {
    const reference = (() => {
      try {
        run("git", ["rev-parse", tag]);
        return tag;
      } catch {
        return "HEAD";
      }
    })();
    const tagList = run("git", ["tag", "--sort=creatordate", "--merged", reference, "--list", "v*"])
      .split("\n")
      .map((s) => s.trim())
      .filter(Boolean);
    const index = tagList.indexOf(tag);
    if (index > 0) return tagList[index - 1];
    if (index === -1 && tagList.length >= 2) return tagList[tagList.length - 2];
    if (index === 0) return "";
    return "";
  } catch {
    try {
      return run("git", ["describe", "--match", "v*", "--abbrev=0", "HEAD~1"]);
    } catch {
      return "";
    }
  }
}

function categorize(subject) {
  const match = subject.match(/^(\w+)(?:\([^)]*\))?:\s*/);
  const raw = match ? match[1].toLowerCase() : "other";

  if (["feat", "feature"].includes(raw)) return "機能追加";
  if (["fix", "bug", "bugfix"].includes(raw)) return "不具合修正";
  if (raw === "refactor") return "内部改善";
  if (raw === "perf") return "パフォーマンス改善";
  if (["chore", "ci", "build", "docs"].includes(raw)) return "運用・保守";
  if (raw === "test") return "テスト";
  if (raw === "revert") return "取り消し";
  return "その他";
}

function buildSummary(commitsByCategory) {
  const sectionOrder = [
    "機能追加",
    "不具合修正",
    "内部改善",
    "パフォーマンス改善",
    "運用・保守",
    "テスト",
    "取り消し",
    "その他",
  ];

  const entries = [];
  for (const section of sectionOrder) {
    const count = commitsByCategory[section]?.length || 0;
    if (count > 0) entries.push(`- ${section}: ${count}件`);
  }
  if (entries.length === 0) {
    return ["- 変更対象のコミットが見つかりませんでした。"];
  }
  return entries;
}

function buildReleaseDraft(previousTag, commitsByCategory, rangeLabel) {
  const sectionOrder = [
    "機能追加",
    "不具合修正",
    "内部改善",
    "パフォーマンス改善",
    "運用・保守",
    "テスト",
    "取り消し",
    "その他",
  ];

  const lines = [
    `# oTo ${tag}`,
    "",
    "## 概要",
  ];

  if (previousTag) {
    lines.push(`- 前回リリース: ${previousTag}`);
  }
  lines.push(`- 対象範囲: ${rangeLabel}`);
  lines.push(...buildSummary(commitsByCategory));
  lines.push("");

  lines.push("## 変更履歴（自動生成）");
  for (const section of sectionOrder) {
    const entries = commitsByCategory[section];
    if (!entries || entries.length === 0) continue;
    lines.push("");
    lines.push(`### ${section}`);
    lines.push(...entries);
  }

  lines.push("");
  lines.push("## 配布ファイル");
  lines.push("");
  lines.push("| ファイル | 用途 |");
  lines.push("| --- | --- |");
  lines.push("| `oto.exe` | Windows ポータブル実行ファイル |");
  lines.push("");
  lines.push(
    "> Windowsで初回起動時に警告が表示される場合は、配布元を確認したうえで実行してください。"
  );
  lines.push("");

  return lines.join("\n");
}

const previousTag = detectPreviousTag();
const range = previousTag ? `${previousTag}..${tag}` : tag;
const rangeLabel = previousTag ? `${previousTag}..${tag}` : "全履歴（初回リリースベース）";
const rawLog = run("git", ["log", range, "--pretty=format:%h%x1f%s", "--no-merges"]);

const entries = rawLog
  .split("\n")
  .map((line) => line.trim())
  .filter(Boolean)
  .map((line) => {
    const [shortSha, subject = "(message unavailable)"] = line.split("\x1f");
    return { shortSha, subject, section: categorize(subject) };
  });

const commitsByCategory = {
  "機能追加": [],
  "不具合修正": [],
  "内部改善": [],
  "パフォーマンス改善": [],
  "運用・保守": [],
  "テスト": [],
  "取り消し": [],
  その他: [],
};

if (entries.length === 0) {
  commitsByCategory["その他"].push("- 変更点が見つかりませんでした。");
} else {
  for (const entry of entries) {
    commitsByCategory[entry.section].push(`- ${entry.shortSha} ${entry.subject}`);
  }
}

const draft = buildReleaseDraft(previousTag, commitsByCategory, rangeLabel);

const outFile = outputPath || path.join(os.tmpdir(), `oTo-release-notes-${tag}.md`);
fs.writeFileSync(outFile, `${draft}\n`, "utf8");

if (applyNotes) {
  const canUseGh = (() => {
    try {
      run("gh", ["--version"]);
      return true;
    } catch {
      return false;
    }
  })();

  if (!canUseGh) {
    console.log(`[release-notes] gh CLI not found. Draft saved: ${outFile}`);
    process.exit(0);
  }

  try {
    run("gh", ["release", "edit", tag, "--notes-file", outFile]);
    const release = run("gh", ["release", "view", tag, "--json", "url"]);
    const releaseUrl = JSON.parse(release || "{}").url || "";
    console.log(`[release-notes] Updated release notes for ${tag}`);
    if (releaseUrl) {
      console.log(`[release-notes] release page: ${releaseUrl}`);
    }
  } catch (error) {
    console.log(
      `[release-notes] Failed to update GitHub release directly. Draft remains: ${outFile}`
    );
    console.log(`[release-notes] ${error.message}`);
  }
} else if (outputPath) {
  console.log(outFile);
} else {
  console.log(draft);
}
