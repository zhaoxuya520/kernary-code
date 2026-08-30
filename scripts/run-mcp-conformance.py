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
    core_scenarios = lock["requiredCoreScenarios"]
    auth_scenarios = lock.get("requiredAuthScenarios", [])
    for scenario in [*core_scenarios, *auth_scenarios]:
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
                "category": "auth" if scenario in auth_scenarios else "core",
                "passed": passed,
                "returnCode": completed.returncode,
                "durationSeconds": duration,
                "outputTail": completed.stdout[-6000:],
            }
        )

    core_results = [result for result in results if result["category"] == "core"]
    auth_results = [result for result in results if result["category"] == "auth"]
    core_passed = sum(1 for result in core_results if result["passed"])
    auth_passed = sum(1 for result in auth_results if result["passed"])
    report = {
        "schemaVersion": 1,
        "referee": lock["referee"],
        "sdkOverride": lock["sdkOverride"],
        "core": {
            "passed": core_passed,
            "total": len(core_results),
            "complete": core_passed == len(core_results),
        },
        "results": results,
        "auth": {
            "passed": auth_passed,
            "required": len(auth_results),
            "total": len(auth_results) + len(lock["pendingAuthScenarios"]) + len(lock.get("knownRefereeDefects", [])),
            "status": "partial",
            "pendingScenarios": lock["pendingAuthScenarios"],
            "knownRefereeDefects": lock.get("knownRefereeDefects", []),
        },
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(
        f"MCP core: {core_passed}/{len(core_results)}; "
        f"auth required: {auth_passed}/{len(auth_results)}; "
        f"pending: {len(lock['pendingAuthScenarios'])}; "
        f"referee defects: {len(lock.get('knownRefereeDefects', []))}"
    )
    print(f"Report: {args.output}")
    return 0 if core_passed == len(core_results) and auth_passed == len(auth_results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
