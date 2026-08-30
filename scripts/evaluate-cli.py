#!/usr/bin/env python3
"""Run deterministic Kernary product gates and emit an auditable scorecard."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[1]


def executable(name: str) -> str:
    candidates = [name]
    if os.name == "nt":
        candidates.insert(0, f"{name}.cmd")
    resolved = next((shutil.which(candidate) for candidate in candidates if shutil.which(candidate)), None)
    if resolved:
        return resolved
    if name == "cargo":
        fallback = Path.home() / ".cargo" / "bin" / ("cargo.exe" if os.name == "nt" else "cargo")
        if fallback.is_file():
            return str(fallback)
    raise SystemExit(f"required executable not found: {name}")


def default_binary() -> Path:
    name = "kernary.exe" if os.name == "nt" else "kernary"
    return ROOT / "target" / "release" / name


def render_command(parts: list[str], values: dict[str, str]) -> list[str]:
    return [values.get(part[1:-1], part) if part.startswith("{") and part.endswith("}") else part for part in parts]


def tail(text: str, limit: int = 4000) -> str:
    return text if len(text) <= limit else f"[truncated]\n{text[-limit:]}"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", choices=["quick", "full"], default="quick")
    parser.add_argument("--matrix", type=Path, default=ROOT / "evals" / "matrix.json")
    parser.add_argument("--binary", type=Path, default=default_binary())
    parser.add_argument("--output", type=Path, default=ROOT / "output" / "evals" / "local-scorecard.json")
    parser.add_argument("--timeout", type=int, default=900)
    args = parser.parse_args()

    matrix = json.loads(args.matrix.read_text(encoding="utf-8"))
    values = {
        "cargo": executable("cargo"),
        "npm": executable("npm"),
        "python": sys.executable,
        "binary": str(args.binary.resolve()),
    }
    environment = os.environ.copy()
    environment["CARGO_INCREMENTAL"] = "0"
    results = []
    earned = 0
    available = 0
    for gate in matrix["localGates"]:
        if args.profile not in gate["profiles"]:
            continue
        available += int(gate["weight"])
        command = render_command(gate["command"], values)
        started = time.perf_counter()
        try:
            completed = subprocess.run(
                command,
                cwd=ROOT,
                env=environment,
                text=True,
                encoding="utf-8",
                errors="replace",
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=args.timeout,
                check=False,
            )
            passed = completed.returncode == 0
            output = completed.stdout
            return_code = completed.returncode
        except subprocess.TimeoutExpired as error:
            passed = False
            output = (error.stdout or "") + "\nTIMEOUT"
            return_code = 124
        duration = round(time.perf_counter() - started, 3)
        if passed:
            earned += int(gate["weight"])
        results.append(
            {
                "id": gate["id"],
                "category": gate["category"],
                "weight": gate["weight"],
                "passed": passed,
                "returnCode": return_code,
                "durationSeconds": duration,
                "command": command,
                "outputTail": tail(output),
            }
        )
        print(f"{'PASS' if passed else 'FAIL'} {gate['id']} ({duration:.2f}s)")

    percent = round(100 * earned / available, 2) if available else 0.0
    report = {
        "schemaVersion": 1,
        "profile": args.profile,
        "localProductScore": {
            "earned": earned,
            "available": available,
            "percent": percent,
            "passed": earned == available,
        },
        "gates": results,
        "externalBenchmarks": matrix["externalBenchmarks"],
        "comparisonClaim": "unproven until paired external benchmark runs complete",
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"Local Product Score: {earned}/{available} ({percent:.2f}%)")
    print(f"Report: {args.output}")
    return 0 if earned == available else 1


if __name__ == "__main__":
    raise SystemExit(main())
