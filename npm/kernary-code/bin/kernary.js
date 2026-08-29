#!/usr/bin/env node
'use strict';

const path = require('node:path');
const { spawnSync } = require('node:child_process');

const targets = {
  'win32-x64': {
    packageName: 'kernary-code-win32-x64',
    binary: 'kernary.exe',
  },
  'linux-x64': {
    packageName: 'kernary-code-linux-x64-gnu',
    binary: 'kernary',
  },
};

const key = `${process.platform}-${process.arch}`;
const target = targets[key];
if (!target) {
  console.error(`KernaryUnsupportedPlatform: ${key}; supported: win32-x64, linux-x64`);
  process.exit(1);
}

let packageJson;
try {
  packageJson = require.resolve(`${target.packageName}/package.json`);
} catch (error) {
  console.error(
    `KernaryBinaryMissing: optional package ${target.packageName} is not installed. ` +
      'Reinstall without --omit=optional.',
  );
  process.exit(1);
}

const executable = path.join(path.dirname(packageJson), 'bin', target.binary);
const result = spawnSync(executable, process.argv.slice(2), {
  stdio: 'inherit',
  windowsHide: true,
});
if (result.error) {
  console.error(`KernaryLaunchError: ${result.error.message}`);
  process.exit(1);
}
if (result.signal) {
  process.kill(process.pid, result.signal);
}
process.exit(result.status === null ? 1 : result.status);
