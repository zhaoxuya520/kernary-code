#!/usr/bin/env python3
"""Run the pinned MCP client conformance core and emit a machine-readable report."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[1]


def default_client() -> Path:
    name = "kernary-mcp-conformance-client.exe" if os.name == "nt" else "kernary-mcp-conformance-client"
    return ROOT / "target" / "debug" / name


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--referee-dir",
        type=Path,
        default=ROOT / "output" / "mcp-conformance-run",
    )
    parser.add_argument("--client", type=Path, default=default_client())
    parser.add_argument(
        "--lock",
        type=Path,
        default=ROOT / "evals" / "mcp-conformance-lock.json",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=ROOT / "output" / "evals" / "mcp-conformance-core.json",
    )
    parser.add_argument("--timeout", type=int, default=30_000)
    args = parser.parse_args()

    lock = json.loads(args.lock.read_text(encoding="utf-8"))
    referee = (
        args.referee_dir
        / "node_modules"
        / "@modelcontextprotocol"
        / "conformance"
        / "dist"
        / "index.js"
    ).resolve(strict=True)
    client = args.client.resolve(strict=True)
    results = []
    for scenario in lock["requiredCoreScenarios"]:
        command = [
            "node",
            str(referee),
            "client",
            "--command",
            str(client),
            "--scenario",
            scenario,
            "--timeout",
            str(args.timeout),
        ]
        started = time.perf_counter()
        completed = subprocess.run(
            command,
            cwd=ROOT,
            text=True,
            encoding="utf-8",
            errors="replace",
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
            timeout=max(60, args.timeout // 1000 + 15),
        )
        duration = round(time.perf_counter() - started, 3)
        passed = completed.returncode == 0
        print(f"{'PASS' if passed else 'FAIL'} {scenario} ({duration:.2f}s)")
        results.append(
            {
                "scenario": scenario,
                "passed": passed,
                "returnCode": completed.returncode,
                "durationSeconds": duration,
                "outputTail": completed.stdout[-6000:],
            }
        )

    passed = sum(1 for result in results if result["passed"])
    report = {
        "schemaVersion": 1,
        "referee": lock["referee"],
        "sdkOverride": lock["sdkOverride"],
        "core": {
            "passed": passed,
            "total": len(results),
            "complete": passed == len(results),
        },
        "results": results,
        "auth": {
            "passed": 0,
            "total": len(lock["pendingAuthScenarios"]),
            "status": "pending",
            "scenarios": lock["pendingAuthScenarios"],
        },
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"MCP core: {passed}/{len(results)}; auth: 0/{len(lock['pendingAuthScenarios'])} pending")
    print(f"Report: {args.output}")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
