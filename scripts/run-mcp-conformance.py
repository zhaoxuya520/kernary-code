#!/usr/bin/env python3
"""Run the pinned MCP client conformance suite and emit a secret-safe report."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[1]
SENSITIVE_CONTEXT_VALUE = re.compile(
    r'("(?:private_key_pem|client_secret|idp_id_token)":")((?:\\.|[^"])*)(")'
)
CHECK_RESULT = re.compile(r"Passed:\s*(\d+)/(\d+)")


def default_client() -> Path:
    name = "kernary-mcp-conformance-client.exe" if os.name == "nt" else "kernary-mcp-conformance-client"
    return ROOT / "target" / "debug" / name


def default_node() -> str:
    configured = os.environ.get("NODE")
    if configured:
        return configured
    discovered = shutil.which("node")
    if discovered:
        return discovered
    if os.name == "nt":
        candidate = (
            Path(os.environ.get("ProgramFiles", r"C:\Program Files"))
            / "nodejs"
            / "node.exe"
        )
        if candidate.is_file():
            return str(candidate)
    return "node"


def redact_sensitive_output(value: str) -> str:
    return SENSITIVE_CONTEXT_VALUE.sub(r"\1[REDACTED]\3", value)


def verify_referee_lock(referee_dir: Path, lock: dict[str, object]) -> None:
    package_lock_path = referee_dir / "package-lock.json"
    package_lock = json.loads(package_lock_path.read_text(encoding="utf-8"))
    packages = package_lock.get("packages", {})
    for lock_key in ("referee", "sdkOverride"):
        expected = lock[lock_key]
        package = expected["package"]
        installed = packages.get(f"node_modules/{package}")
        if not installed:
            raise RuntimeError(f"pinned referee package missing: {package}")
        for field in ("version", "integrity"):
            if installed.get(field) != expected[field]:
                raise RuntimeError(
                    f"pinned referee {package} {field} mismatch: "
                    f"expected {expected[field]}, got {installed.get(field)}"
                )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--referee-dir",
        type=Path,
        default=ROOT / "output" / "mcp-conformance-run",
    )
    parser.add_argument("--client", type=Path, default=default_client())
    parser.add_argument("--node", default=default_node())
    parser.add_argument(
        "--lock",
        type=Path,
        default=ROOT / "evals" / "mcp-conformance-lock.json",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=ROOT / "output" / "evals" / "mcp-conformance-0.1.16.json",
    )
    parser.add_argument("--timeout", type=int, default=30_000)
    args = parser.parse_args()

    lock = json.loads(args.lock.read_text(encoding="utf-8"))
    verify_referee_lock(args.referee_dir, lock)
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
            args.node,
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
        safe_output = redact_sensitive_output(completed.stdout)
        check_matches = CHECK_RESULT.findall(safe_output)
        checks_passed, checks_total = (
            tuple(map(int, check_matches[-1])) if check_matches else (0, 0)
        )
        passed = (
            completed.returncode == 0
            and bool(check_matches)
            and checks_passed == checks_total
        )
        print(f"{'PASS' if passed else 'FAIL'} {scenario} ({duration:.2f}s)")
        results.append(
            {
                "scenario": scenario,
                "category": "auth" if scenario in auth_scenarios else "core",
                "passed": passed,
                "returnCode": completed.returncode,
                "durationSeconds": duration,
                "checksPassed": checks_passed,
                "checksTotal": checks_total,
                "outputTail": safe_output[-6000:],
            }
        )

    core_results = [result for result in results if result["category"] == "core"]
    auth_results = [result for result in results if result["category"] == "auth"]
    core_passed = sum(1 for result in core_results if result["passed"])
    auth_passed = sum(1 for result in auth_results if result["passed"])
    checks_passed = sum(result["checksPassed"] for result in results)
    checks_total = sum(result["checksTotal"] for result in results)
    pending = lock.get("pendingAuthScenarios", [])
    referee_defects = lock.get("knownRefereeDefects", [])
    auth_complete = auth_passed == len(auth_results) and not pending and not referee_defects
    report = {
        "schemaVersion": 2,
        "referee": lock["referee"],
        "sdkOverride": lock["sdkOverride"],
        "secretsRedacted": True,
        "checks": {
            "passed": checks_passed,
            "total": checks_total,
            "complete": checks_passed == checks_total,
        },
        "core": {
            "passed": core_passed,
            "total": len(core_results),
            "complete": core_passed == len(core_results),
        },
        "results": results,
        "auth": {
            "passed": auth_passed,
            "required": len(auth_results),
            "total": len(auth_results) + len(pending) + len(referee_defects),
            "status": "complete" if auth_complete else "partial",
            "pendingScenarios": pending,
            "knownRefereeDefects": referee_defects,
        },
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(
        f"MCP core: {core_passed}/{len(core_results)}; "
        f"auth required: {auth_passed}/{len(auth_results)}; "
        f"checks: {checks_passed}/{checks_total}; "
        f"pending: {len(pending)}; "
        f"referee defects: {len(referee_defects)}"
    )
    print(f"Report: {args.output}")
    return 0 if core_passed == len(core_results) and auth_complete else 1


if __name__ == "__main__":
    raise SystemExit(main())
