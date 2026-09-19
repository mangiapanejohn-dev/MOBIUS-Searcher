#!/usr/bin/env node
// Launcher: runs the prebuilt binary with the same arguments, terminal and
// exit code. Installs it first if postinstall did not run.
"use strict";

const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const { install, binaryPath } = require("../install.js");

async function main() {
  let bin = binaryPath();
  if (!fs.existsSync(bin)) bin = await install();
  const r = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
  if (r.error) throw r.error;
  process.exit(r.status === null ? 1 : r.status);
}

main().catch((e) => {
  console.error(`mobius-searcher: ${e.message}`);
  process.exit(1);
});
