import { chmod, copyFile, mkdir, readFile } from 'node:fs/promises';
import path from 'node:path';
import process from 'node:process';

const [, , platform, binary] = process.argv;
const definitions = {
  'win32-x64': ['kernary-code-win32-x64', 'kernary.exe'],
  'linux-x64-gnu': ['kernary-code-linux-x64-gnu', 'kernary'],
};
const definition = definitions[platform];
if (!definition || !binary) {
  throw new Error('usage: stage-platform-package.mjs <win32-x64|linux-x64-gnu> <binary>');
}

const root = path.resolve(import.meta.dirname, '..', '..');
const [packageDirectory, binaryName] = definition;
const destinationRoot = path.join(root, 'npm', packageDirectory);
const destinationBin = path.join(destinationRoot, 'bin');
await mkdir(destinationBin, { recursive: true });
await copyFile(path.resolve(binary), path.join(destinationBin, binaryName));
await copyFile(path.join(root, 'LICENSE-APACHE'), path.join(destinationRoot, 'LICENSE-APACHE'));
if (platform === 'linux-x64-gnu') {
  await chmod(path.join(destinationBin, binaryName), 0o755);
}

const packageJson = JSON.parse(
  await readFile(path.join(destinationRoot, 'package.json'), 'utf8'),
);
process.stdout.write(`${packageJson.name}@${packageJson.version}\n`);
