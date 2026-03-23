#!/usr/bin/env node
import { execSync, spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

function run(cmd) {
  execSync(cmd, { stdio: "inherit" });
}

function runQuiet(cmd) {
  return execSync(cmd, { stdio: ["ignore", "pipe", "pipe"], encoding: "utf8" }).trim();
}

function fail(message) {
  console.error(`\n[release] ${message}`);
  process.exit(1);
}

function normalizeVersion(input) {
  const raw = `${input ?? ""}`.trim();
  const version = raw.startsWith("v") ? raw.slice(1) : raw;
  if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version)) {
    fail("版本号格式不正确，请使用 x.y.z 或 vx.y.z（例如 1.2.0）");
  }
  return version;
}

function ensureCleanWorktree() {
  const status = runQuiet("git status --porcelain");
  if (status) {
    fail("git 工作区不干净，请先提交或清理改动后再发布。");
  }
}

function ensureTagNotExists(tag) {
  const local = spawnSync("git", ["rev-parse", "-q", "--verify", `refs/tags/${tag}`], {
    stdio: "ignore",
  });
  if (local.status === 0) {
    fail(`本地 tag 已存在：${tag}`);
  }

  const remote = spawnSync("git", ["ls-remote", "--tags", "origin", tag], {
    stdio: ["ignore", "pipe", "ignore"],
    encoding: "utf8",
  });
  if ((remote.stdout ?? "").trim()) {
    fail(`远程 tag 已存在：${tag}`);
  }
}

function writeJson(path, updater) {
  const abs = resolve(path);
  const data = JSON.parse(readFileSync(abs, "utf8"));
  updater(data);
  writeFileSync(abs, `${JSON.stringify(data, null, 2)}\n`, "utf8");
}

function updateCargoTomlVersion(path, nextVersion) {
  const abs = resolve(path);
  const lines = readFileSync(abs, "utf8").split("\n");
  let inPackage = false;
  let replaced = false;

  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    const section = line.match(/^\s*\[(.+)\]\s*$/);
    if (section) {
      inPackage = section[1] === "package";
      continue;
    }
    if (inPackage && /^\s*version\s*=\s*".*"\s*$/.test(line)) {
      lines[i] = `version = "${nextVersion}"`;
      replaced = true;
      break;
    }
  }

  if (!replaced) {
    fail("未找到 src-tauri/Cargo.toml 的 [package] version 字段。");
  }

  writeFileSync(abs, `${lines.join("\n")}\n`, "utf8");
}

function main() {
  const version = normalizeVersion(process.argv[2]);
  const tag = `v${version}`;

  ensureCleanWorktree();
  ensureTagNotExists(tag);

  const currentVersion = runQuiet("node -p \"require('./package.json').version\"");
  if (currentVersion === version) {
    fail(`版本号已经是 ${version}，无需重复发布。`);
  }

  console.log(`[release] 准备发布版本 ${version}`);

  writeJson("package.json", (data) => {
    data.version = version;
  });
  writeJson("package-lock.json", (data) => {
    data.version = version;
    if (data.packages && data.packages[""]) {
      data.packages[""].version = version;
    }
  });
  writeJson("src-tauri/tauri.conf.json", (data) => {
    data.version = version;
  });
  updateCargoTomlVersion("src-tauri/Cargo.toml", version);

  console.log("[release] 版本号更新完成，开始执行构建检查");
  run("npm run build");
  run("cargo check --manifest-path src-tauri/Cargo.toml");

  console.log("[release] 提交、打 tag、推送");
  run("git add package.json package-lock.json src-tauri/Cargo.toml src-tauri/tauri.conf.json");
  run(`git commit -m "chore(release): ${tag}"`);
  run(`git tag ${tag}`);
  run("git push origin HEAD");
  run(`git push origin ${tag}`);

  console.log(`[release] 发布流程完成：${tag}`);
}

main();
