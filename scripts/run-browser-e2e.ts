import { mkdirSync, rmSync } from "node:fs";
import { resolve, sep } from "node:path";
import { spawnSync } from "node:child_process";

const workspaceRoot = resolve(process.cwd());
const testTempRoot = resolve(workspaceRoot, "output", "test-temp");
if (!testTempRoot.startsWith(`${workspaceRoot}${sep}`)) throw new Error(`unsafe-test-temp-root: ${testTempRoot}`);
rmSync(testTempRoot, { recursive: true, force: true });
mkdirSync(testTempRoot, { recursive: true });

const result = spawnSync(process.execPath, ["--test", "tests/browser-runtime.test.ts"], {
  cwd: workspaceRoot,
  stdio: "inherit",
  env: { ...process.env, HARNESS_BROWSER_E2E: "1" },
});

rmSync(testTempRoot, { recursive: true, force: true, maxRetries: 5, retryDelay: 50 });
process.exitCode = result.status ?? 1;
