import { access, readFile } from 'node:fs/promises';
import path from 'node:path';

const root = path.resolve(import.meta.dirname, '..', '..');
const packages = [
  ['kernary-code-win32-x64', 'kernary.exe'],
  ['kernary-code-linux-x64-gnu', 'kernary'],
];
const meta = JSON.parse(await readFile(path.join(root, 'npm/kernary-code/package.json'), 'utf8'));
for (const [directory, binary] of packages) {
  const manifest = JSON.parse(
    await readFile(path.join(root, 'npm', directory, 'package.json'), 'utf8'),
  );
  if (manifest.version !== meta.version || meta.optionalDependencies[manifest.name] !== meta.version) {
    throw new Error(`version mismatch for ${manifest.name}`);
  }
  await access(path.join(root, 'npm', directory, 'bin', binary));
}
process.stdout.write(`npm package set verified: ${meta.version}\n`);
