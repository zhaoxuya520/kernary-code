import { mkdirSync, readdirSync, rmSync } from "node:fs";
import { resolve, sep } from "node:path";
import { spawnSync } from "node:child_process";

const workspaceRoot = resolve(process.cwd());
const testTempRoot = resolve(workspaceRoot, "output", "test-temp");

// 删除目标必须严格位于当前工作区 output/test-temp，避免测试清理越界。
if (!testTempRoot.startsWith(`${workspaceRoot}${sep}`)) {
  throw new Error(`unsafe-test-temp-root: ${testTempRoot}`);
}

rmSync(testTempRoot, { recursive: true, force: true });
mkdirSync(testTempRoot, { recursive: true });

const testFiles = readdirSync(resolve(workspaceRoot, "tests"))
  .filter((name) => name.endsWith(".test.ts"))
  .sort()
  .map((name) => `tests/${name}`);

const result = spawnSync(process.execPath, ["--test", ...testFiles], {
  cwd: workspaceRoot,
  stdio: "inherit",
});

// 子测试进程退出后，Windows 的 SQLite 原生文件句柄已经彻底释放。
rmSync(testTempRoot, { recursive: true, force: true, maxRetries: 5, retryDelay: 50 });

process.exitCode = result.status ?? 1;
