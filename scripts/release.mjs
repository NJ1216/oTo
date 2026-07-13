#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { spawnSync } from "node:child_process";

const argv = process.argv.slice(2);
const isDryRun = argv.includes("--dry-run");
const requestedTag = argv.find((arg) => !arg.startsWith("--"));

if (!requestedTag) {
  console.error("Usage: pnpm release <vYY.M.D> [--dry-run]");
  process.exit(1);
}

if (!/^v?\d{2}\.\d{1,2}\.\d{1,2}$/.test(requestedTag)) {
  console.error("Tag must be in format vYY.M.D (example: v26.7.12).");
  process.exit(1);
}

const tag = requestedTag.startsWith("v") ? requestedTag : `v${requestedTag}`;
const version = tag.slice(1);
const repoRoot = process.cwd();

function runCommand(command, args = [], options = {}) {
  const result = spawnSync(command, args, {
    cwd: options.cwd || repoRoot,
    encoding: "utf8",
    stdio: options.stdio || ["ignore", "pipe", "pipe"],
    shell: false,
    maxBuffer: options.maxBuffer || 1024 * 1024 * 20,
  });

  if (result.error) {
    throw result.error;
  }

  if (result.status !== 0) {
    const msg = result.stderr?.trim() || result.stdout?.trim() || "command failed";
    throw new Error(`${command} ${args.join(" ")}: ${msg}`);
  }

  return options.capture === false ? "" : (result.stdout || "").trim();
}

function runNoThrow(command, args = [], options = {}) {
  try {
    runCommand(command, args, { ...options, capture: false });
    return true;
  } catch {
    return false;
  }
}

function readJson(filePath) {
  const content = fs.readFileSync(filePath, "utf8");
  return JSON.parse(content);
}

function writeJson(filePath, object) {
  fs.writeFileSync(filePath, `${JSON.stringify(object, null, 2)}\n`, "utf8");
}

function writeLines(filePath, lines) {
  while (lines.at(-1) === "") {
    lines.pop();
  }
  fs.writeFileSync(filePath, `${lines.join("\n")}\n`, "utf8");
}

function updatePackageVersion(versionValue) {
  const packagePath = path.join(repoRoot, "package.json");
  const pkg = readJson(packagePath);
  pkg.version = versionValue;
  writeJson(packagePath, pkg);
}

function updateCargoVersion(versionValue) {
  const cargoPath = path.join(repoRoot, "src-tauri", "Cargo.toml");
  const lines = fs.readFileSync(cargoPath, "utf8").split(/\r?\n/);
  let inPackage = false;
  let changed = false;

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (/^\[package\]/.test(line)) {
      inPackage = true;
      continue;
    }
    if (inPackage && /^\[/.test(line)) {
      inPackage = false;
      continue;
    }
    if (inPackage && /^version\s*=\s*"/.test(line)) {
      lines[i] = `version = "${versionValue}"`;
      changed = true;
      break;
    }
  }

  if (!changed) {
    throw new Error("Could not find version in [package] section of src-tauri/Cargo.toml.");
  }

  writeLines(cargoPath, lines);
}

function updateCargoLockVersion(versionValue) {
  const lockPath = path.join(repoRoot, "src-tauri", "Cargo.lock");
  const lines = fs.readFileSync(lockPath, "utf8").split(/\r?\n/);
  let inOtoPackage = false;

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (line === "[[package]]") {
      inOtoPackage = false;
      continue;
    }
    if (line === 'name = "oto"') {
      inOtoPackage = true;
      continue;
    }
    if (inOtoPackage && /^version\s*=\s*"/.test(line)) {
      lines[i] = `version = "${versionValue}"`;
      writeLines(lockPath, lines);
      return;
    }
  }

  throw new Error('Could not find version for package "oto" in src-tauri/Cargo.lock.');
}

function updateTauriVersion(versionValue) {
  const confPath = path.join(repoRoot, "src-tauri", "tauri.conf.json");
  const conf = readJson(confPath);
  conf.version = versionValue;
  writeJson(confPath, conf);
}

function getPreviousTagFromHead() {
  const checks = ["HEAD^", "HEAD~1"];
  for (const rev of checks) {
    try {
      return runCommand("git", [
        "describe",
        "--match",
        "v*",
        "--abbrev=0",
        rev,
      ]);
    } catch {
      // continue
    }
  }
  return "";
}

function generateReleaseDraft(currentTag, previousTag) {
  const range = previousTag ? `${previousTag}..HEAD` : "HEAD";
  const format = "--pretty=format:- %h %s";
  const log = runCommand("git", ["log", range, format, "--no-merges"]);

  const commitLines = log
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);

  const lines = [
    `# oTo ${currentTag}`,
    "",
    "## 変更履歴（自動生成）",
    "",
  ];

  if (commitLines.length === 0) {
    lines.push("- 該当するコミットが見つかりませんでした。");
  } else {
    lines.push(...commitLines);
  }

  lines.push("");
  lines.push("## 配布ファイル");
  lines.push("");
  lines.push("| ファイル | 用途 |");
  lines.push("| --- | --- |");
  lines.push("| `oto.exe` | Windows ポータブル実行ファイル |");
  lines.push("");
  lines.push("> Windowsで初回起動時に警告が表示される場合は、配布元を確認したうえで実行してください。");
  lines.push("");

  return lines.join("\n");
}

function writeDraftFile(tag, draft) {
  const draftPath = path.join(os.tmpdir(), `oTo-release-notes-${tag}.md`);
  fs.writeFileSync(draftPath, `${draft}\n`, "utf8");
  return draftPath;
}

function sleepMs(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

function waitForWorkflow(tagSha, timeoutMs = 20 * 60 * 1000) {
  const started = Date.now();
  const pollMs = 10 * 1000;

  while (Date.now() - started < timeoutMs) {
    const raw = runCommand("gh", [
      "run",
      "list",
      "--workflow",
      "build.yml",
      "--limit",
      "40",
      "--json",
      "databaseId,headSha,event,status,conclusion,url",
    ]);

    const runs = JSON.parse(raw || "[]");
    const target = runs.find((run) => run.headSha === tagSha && run.event === "push");

    if (target) {
      if (target.status === "completed") {
        if (target.conclusion !== "success") {
          throw new Error(
            `GitHub workflow finished with status: ${target.conclusion}. Check run: ${target.url}`
          );
        }
        return target;
      }

      runCommand("gh", ["run", "watch", String(target.databaseId), "--exit-status"]);
      const confirmed = runCommand("gh", [
        "run",
        "view",
        String(target.databaseId),
        "--json",
        "conclusion,url",
      ]);
      const parsed = JSON.parse(confirmed);
      if (parsed.conclusion !== "success") {
        throw new Error(`GitHub workflow failed. Check run: ${parsed.url || target.url}`);
      }
      return { ...target, ...parsed };
    }

    sleepMs(pollMs);
  }

  throw new Error("Timeout while waiting for GitHub workflow to complete.");
}

async function main() {
  const state = {
    commitCreated: false,
    tagCreated: false,
    tagPushed: false,
    commitPushed: false,
    releaseCommit: "",
    originalHead: runCommand("git", ["rev-parse", "HEAD"]),
    previousTag: "",
  };

  try {
    const status = runCommand("git", ["status", "--porcelain"]);
    if (status) {
      throw new Error("Working tree is not clean. Commit or stash changes first.");
    }

    const exists = runCommand("git", ["tag", "-l", tag]);
    if (exists) {
      throw new Error(`Tag ${tag} already exists.`);
    }

    state.previousTag = getPreviousTagFromHead();
    const draft = generateReleaseDraft(tag, state.previousTag);
    const draftPath = writeDraftFile(tag, draft);
    console.log(`[release] draft notes (temp): ${draftPath}`);

    if (isDryRun) {
      console.log("[release] Dry run complete. No commit/tag/push executed.");
      return;
    }

    updatePackageVersion(version);
    updateCargoVersion(version);
    updateCargoLockVersion(version);
    updateTauriVersion(version);

    const pkgPath = path.join(repoRoot, "package.json");
    const cargoPath = path.join(repoRoot, "src-tauri", "Cargo.toml");
    const cargoLockPath = path.join(repoRoot, "src-tauri", "Cargo.lock");
    const confPath = path.join(repoRoot, "src-tauri", "tauri.conf.json");

    runCommand("git", ["add", pkgPath, cargoPath, cargoLockPath, confPath]);
    if (runNoThrow("git", ["diff", "--cached", "--quiet"])) {
      state.releaseCommit = runCommand("git", ["rev-parse", "HEAD"]);
      console.log(`[release] version is already ${version}; tagging current HEAD`);
    } else {
      runCommand("git", ["commit", "-m", `chore: release ${tag}`]);
      state.commitCreated = true;
      state.releaseCommit = runCommand("git", ["rev-parse", "HEAD"]);
    }

    runCommand("git", ["tag", "-a", tag, "-m", `Release ${tag}`]);
    state.tagCreated = true;

    runCommand("git", ["push"]);
    state.commitPushed = true;
    runCommand("git", ["push", "origin", tag]);
    state.tagPushed = true;

    const tagSha = runCommand("git", ["rev-list", "-n", "1", tag]);
    const workflow = waitForWorkflow(tagSha);

    let releaseUrl = "(not yet published)";
    try {
      const release = runCommand("gh", ["release", "view", tag, "--json", "url"]);
      releaseUrl = JSON.parse(release).url || releaseUrl;
    } catch {
      // release may still be generating
    }

    console.log(`[release] tag and commit created: ${tag} / ${state.releaseCommit}`);
    console.log(`[release] workflow run: ${workflow.url}`);
    console.log(`[release] release page: ${releaseUrl}`);
    console.log("[release] Use the github-release skill to review and polish the release notes in Japanese.");
  } catch (error) {
    console.error(`[release] ERROR: ${error.message}`);

    if (!isDryRun) {
      if (state.tagCreated) {
        runNoThrow("git", ["tag", "-d", tag]);
        runNoThrow("git", ["push", "origin", "--delete", tag]);
      }

      if (state.commitPushed && state.commitCreated) {
        runNoThrow("git", ["revert", "--no-edit", state.releaseCommit]);
        runNoThrow("git", ["push"]);
      } else if (state.commitCreated) {
        runNoThrow("git", ["reset", "--hard", state.originalHead]);
      }
    }

    process.exit(1);
  }
}

await main();
