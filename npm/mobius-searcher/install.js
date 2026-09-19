#!/usr/bin/env node
// Downloads the prebuilt mobius-searcher binary matching this package's
// version from the GitHub release, verifies it against SHA256SUMS and unpacks
// it into ./vendor. Runs as postinstall and again on first use if install
// scripts were skipped. Uses curl when present (it honours HTTPS_PROXY), else
// Node's fetch.
"use strict";

const { execFileSync } = require("node:child_process");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const REPO = "mangiapanejohn-dev/MOBIUS-Searcher";
const VERSION = require("./package.json").version;
const VENDOR = path.join(__dirname, "vendor");
const EXE = process.platform === "win32" ? "mobius-searcher.exe" : "mobius-searcher";

const TARGETS = {
  "darwin-arm64": "aarch64-apple-darwin",
  "darwin-x64": "x86_64-apple-darwin",
  "linux-x64": "x86_64-unknown-linux-gnu",
  "linux-arm64": "aarch64-unknown-linux-gnu",
  "win32-x64": "x86_64-pc-windows-msvc",
};

function target() {
  const key = `${process.platform}-${process.arch}`;
  const t = TARGETS[key];
  if (!t) {
    throw new Error(
      `no prebuilt mobius-searcher for ${key}; build from source: ` +
        `cargo install --git https://github.com/${REPO} mobius-searcher --locked`
    );
  }
  return t;
}

function binaryPath() {
  return path.join(VENDOR, `mobius-searcher-${target()}`, EXE);
}

function hasCurl() {
  try {
    execFileSync("curl", ["--version"], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

async function download(url, dest) {
  if (hasCurl()) {
    execFileSync("curl", ["-fsSL", "--retry", "3", "-o", dest, url], { stdio: "inherit" });
    return;
  }
  const res = await fetch(url, { redirect: "follow" });
  if (!res.ok) throw new Error(`GET ${url}: HTTP ${res.status}`);
  fs.writeFileSync(dest, Buffer.from(await res.arrayBuffer()));
}

async function install() {
  const bin = binaryPath();
  if (fs.existsSync(bin)) return bin;
  const t = target();
  const asset = `mobius-searcher-${t}.${process.platform === "win32" ? "zip" : "tar.gz"}`;
  // MOBIUS_DOWNLOAD_BASE: a mirror holding the same assets (or a local test server)
  const base = process.env.MOBIUS_DOWNLOAD_BASE || `https://github.com/${REPO}/releases/download/v${VERSION}`;
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "mobius-"));
  try {
    const archive = path.join(tmp, asset);
    const sums = path.join(tmp, "SHA256SUMS");
    console.error(`mobius-searcher: downloading ${asset} (v${VERSION})`);
    await download(`${base}/${asset}`, archive);
    await download(`${base}/SHA256SUMS`, sums);
    const want = fs
      .readFileSync(sums, "utf8")
      .split("\n")
      .map((l) => l.trim().split(/\s+/))
      .find((p) => p[1] === asset)?.[0];
    const got = crypto.createHash("sha256").update(fs.readFileSync(archive)).digest("hex");
    if (!want || want !== got) {
      throw new Error(`checksum mismatch for ${asset} (expected ${want || "none"}, got ${got})`);
    }
    fs.mkdirSync(VENDOR, { recursive: true });
    // bsdtar (macOS, Windows 10+) and GNU tar both read .tar.gz; bsdtar also reads .zip
    execFileSync("tar", ["-xf", archive, "-C", VENDOR], { stdio: "inherit" });
    if (process.platform !== "win32") fs.chmodSync(bin, 0o755);
    return bin;
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

module.exports = { install, binaryPath };

if (require.main === module) {
  install().catch((e) => {
    // do not fail `npm install`: the launcher retries on first use
    console.error(`mobius-searcher: ${e.message}`);
    console.error("mobius-searcher: the download will be retried when you first run it.");
  });
}
