#!/usr/bin/env python3
"""跨平台发布基准：测量单二进制体积与 warm `--help` 延迟。"""

import argparse
import json
import pathlib
import statistics
import subprocess
import time


def percentile(values, fraction):
    ordered = sorted(values)
    return ordered[int(fraction * (len(ordered) - 1))]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=pathlib.Path)
    parser.add_argument(
        "--budget",
        type=pathlib.Path,
        default=pathlib.Path("release/performance-budget.json"),
    )
    parser.add_argument("--iterations", type=int)
    parser.add_argument("--output", type=pathlib.Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    budget = json.loads(args.budget.read_text(encoding="utf-8"))
    iterations = args.iterations or int(budget["defaultIterations"])
    if iterations < 10 or iterations > 2000:
        raise SystemExit("iterations must be between 10 and 2000")

    subprocess.run([str(binary), "--help"], check=True, stdout=subprocess.DEVNULL)
    samples = []
    for _ in range(iterations):
        started = time.perf_counter_ns()
        subprocess.run(
            [str(binary), "--help"],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        samples.append((time.perf_counter_ns() - started) / 1_000_000)
    report = {
        "schemaVersion": 1,
        "binary": binary.name,
        "binaryBytes": binary.stat().st_size,
        "iterations": iterations,
        "warmHelpMillis": {
            "p50": round(percentile(samples, 0.50), 2),
            "p95": round(percentile(samples, 0.95), 2),
            "p99": round(percentile(samples, 0.99), 2),
            "mean": round(statistics.fmean(samples), 2),
        },
        "budget": {
            "maxBinaryBytes": int(budget["maxBinaryBytes"]),
            "maxWarmHelpP95Millis": float(budget["maxWarmHelpP95Millis"]),
        },
    }
    report["passed"] = (
        report["binaryBytes"] <= report["budget"]["maxBinaryBytes"]
        and report["warmHelpMillis"]["p95"]
        <= report["budget"]["maxWarmHelpP95Millis"]
    )
    encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.output:
        args.output.write_text(encoded, encoding="utf-8")
    print(encoded, end="")
    if not report["passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
