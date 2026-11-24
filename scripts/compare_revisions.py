#!/usr/bin/env python3
"""Compare two zrun revisions with adjacent, alternating fresh processes.

This reuses benchmark.py's workloads, sample validation, process bounds and
paired bootstrap. Both variants remain engine=zrun; neither is called Tokio.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path

import benchmark as bench


def build_receipt(path, binary):
    receipt = json.loads(path.read_text())
    if receipt.get("exit_code") != 0 or not receipt.get("source_head"):
        raise ValueError(f"{path}: missing successful build/source identity")
    if receipt.get("source_status") != "":
        raise ValueError(f"{path}: comparison requires a clean source build")
    actual = hashlib.sha256(binary.read_bytes()).hexdigest()
    if receipt.get("binary_sha256") != actual:
        raise ValueError(f"{path}: binary SHA-256 does not match the build receipt")
    return receipt


def summarize(samples, args):
    cells = []
    for tasks in args.tasks:
        for metric, (scenario, direction, _, _, unit) in bench.METRICS.items():
            if scenario not in args.scenarios:
                continue
            records = [s for s in samples if s["tasks"] == tasks and s["scenario"] == scenario]
            values = {}
            for variant in ("baseline", "candidate"):
                values[variant] = {
                    s["round"]: s["sample"]["metrics"].get(metric)
                    for s in records if s["variant"] == variant and s["status"] == "ok"
                }
            pairs = [(values["candidate"].get(i), values["baseline"].get(i))
                     for i in range(args.rounds)]
            release = all(s.get("sample", {}).get("build", {}).get("profile") == "release"
                          for s in records)
            comparison = bench.compare_pairs(
                pairs, direction=direction, normal=1.0, challenge=1.0,
                expected=args.rounds, iterations=args.bootstrap,
                seed=bench.stable_seed(args.seed, tasks, metric), release=release,
            )
            interval = comparison["ratio_bootstrap_95_ci"]
            improvement = (comparison["status"] == "evaluated" and
                           (interval[0] > 1.0 if direction == "higher" else interval[1] < 1.0))
            cells.append({
                "tasks": tasks, "scenario": scenario, "metric": metric, "unit": unit,
                "direction": direction, "status": comparison["status"],
                "pairs": comparison["pairs"], "expected_pairs": args.rounds,
                "variants": {variant: bench.describe(
                    [values[variant].get(i) for i in range(args.rounds)], args.bootstrap,
                    bench.stable_seed(args.seed, tasks, metric, variant))
                    for variant in values},
                "ratio_definition": "median(candidate) / median(baseline)",
                "ratio": comparison["ratio"], "ratio_bootstrap_95_ci": interval,
                "evidence_of_improvement": improvement,
            })
    return {"cells": cells, "tokio_targets": "not_evaluated_by_revision_comparison",
            "interpretation": "At least ten complete positive release pairs and a 95% ratio interval strictly beyond 1 establish a measured improvement for this workload. No multiplicity correction or full-project acceptance."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for variant in ("baseline", "candidate"):
        parser.add_argument(f"--{variant}", type=Path, required=True)
        parser.add_argument(f"--{variant}-build", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--tasks", type=int, nargs="+", choices=bench.TASK_SCALES,
                        default=list(bench.TASK_SCALES))
    parser.add_argument("--scenarios", nargs="+", choices=bench.SCENARIOS,
                        default=list(bench.SCENARIOS))
    parser.add_argument("--workers", type=int, choices=bench.WORKERS, default=1)
    parser.add_argument("--rounds", type=int, default=10)
    parser.add_argument("--yields", type=int, default=16)
    parser.add_argument("--bootstrap", type=int, default=5000)
    parser.add_argument("--seed", type=int, default=20261123)
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--affinity")
    args = parser.parse_args()
    if not 1 <= args.rounds <= 1000 or not 100 <= args.bootstrap <= 100000:
        parser.error("rounds must be 1..1000 and bootstrap 100..100000")
    if not math.isfinite(args.timeout) or not 0 < args.timeout <= 86400:
        parser.error("timeout must be finite and in (0,86400]")
    if not 1 <= args.yields <= 10000:
        parser.error("yields must be 1..10000")
    args.tasks = list(dict.fromkeys(args.tasks))
    args.scenarios = list(dict.fromkeys(args.scenarios))
    args.latency_samples, args.hold_ms = 10000, 20
    try:
        if args.affinity:
            if not hasattr(os, "sched_setaffinity"):
                raise ValueError("setting affinity is unsupported on this platform")
            cpus = bench.parse_affinity(args.affinity)
            if not set(cpus).issubset(set(bench.affinity())):
                raise ValueError("requested CPUs are not all in the allowed affinity")
            os.sched_setaffinity(0, cpus)
        builds = {}
        for variant in ("baseline", "candidate"):
            binary = getattr(args, variant).resolve()
            setattr(args, variant, binary)
            builds[variant] = build_receipt(getattr(args, f"{variant}_build"), binary)
        output = args.output.resolve()
        output.mkdir(parents=True, exist_ok=False)
        for variant in builds:
            (output / variant).mkdir()
    except (ValueError, OSError) as error:
        parser.error(str(error))
    bench.install_signals()
    report = {
        "schema_version": 1, "status": "running", "builds": builds,
        "environment": bench.environment(args.candidate),
        "config": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        "samples": [],
    }
    bench.atomic_json(output / "report.json", report)
    for i in range(args.rounds):
        for tasks in args.tasks:
            for scenario in args.scenarios:
                order = ("baseline", "candidate") if i % 2 == 0 else ("candidate", "baseline")
                for variant in order:
                    if bench.STOP_REQUESTED:
                        break
                    args.binary = getattr(args, variant)
                    sample = bench.collect_sample(args, output / variant, i, tasks, scenario, "zrun", workers=args.workers)
                    sample["variant"] = variant
                    sample["stdout"] = f"{variant}/{sample['stdout']}"
                    sample["stderr"] = f"{variant}/{sample['stderr']}"
                    report["samples"].append(sample)
                    bench.atomic_json(output / "report.json", report)
                    print(f"round={i + 1}/{args.rounds} tasks={tasks} scenario={scenario} variant={variant} status={sample['status']}", flush=True)
    report["status"] = "cancelled" if bench.STOP_REQUESTED else "completed"
    if not bench.STOP_REQUESTED and any(s["status"] != "ok" for s in report["samples"]):
        report["status"] = "completed_with_errors"
    report["summary"] = summarize(report["samples"], args)
    bench.atomic_json(output / "report.json", report)
    print(output / "report.json")
    return 130 if bench.STOP_REQUESTED else (0 if report["status"] == "completed" else 1)


if __name__ == "__main__":
    raise SystemExit(main())
