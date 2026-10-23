#!/usr/bin/env node
// Runs the chanto binary from the platform package npm installed alongside
// this one (an optional dependency picked by os and cpu).
'use strict';
const { spawnSync } = require('node:child_process');

const platforms = {
  'darwin-arm64': '@chanto-sh/chanto-darwin-arm64',
  'darwin-x64': '@chanto-sh/chanto-darwin-x64',
  'linux-arm64': '@chanto-sh/chanto-linux-arm64',
  'linux-x64': '@chanto-sh/chanto-linux-x64',
  'win32-x64': '@chanto-sh/chanto-win32-x64'
};
const key = `${process.platform}-${process.arch}`;
const pkg = platforms[key];
if (!pkg) {
  console.error(`chanto: no prebuilt binary for ${key}. Install from source: cargo install chanto`);
  process.exit(1);
}
let bin;
try {
  bin = require.resolve(`${pkg}/bin/chanto${process.platform === 'win32' ? '.exe' : ''}`);
} catch {
  console.error(`chanto: ${pkg} isn't installed. Reinstall without --omit=optional or --no-optional.`);
  process.exit(1);
}
const result = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
if (result.error) {
  console.error(`chanto: ${result.error.message}`);
  process.exit(1);
}
if (result.signal) process.kill(process.pid, result.signal);
process.exit(result.status ?? 1);
