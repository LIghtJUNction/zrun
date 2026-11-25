#!/usr/bin/env python3
"""Run paired cold/hot shell batches with an already built zrun-shell-compare binary.

Build on Linux: cargo build --locked --release --manifest-path scripts/bench-shell/Cargo.toml
Run: python3 scripts/bench-shell/run.py /path/to/zrun-shell-compare --out /tmp/shell.json
Cold includes runtime creation and the first batch in a fresh process. Hot uses the same
runtime for a second identical batch. Both exclude OS launch of the benchmark binary.
Filesystem caches remain in their natural state; no page cache is flushed.
"""
import argparse
import hashlib
import json
import os
import platform
import random
import statistics
import subprocess
import time
from pathlib import Path


def spread(values):
    q1, _, q3 = statistics.quantiles(values, n=4, method="inclusive")
    return {"median": statistics.median(values), "q1": q1, "q3": q3}


def paired_interval(ratios):
    rng = random.Random(62865)
    boot = sorted(statistics.median(rng.choices(ratios, k=len(ratios))) for _ in range(10000))
    return [boot[249], boot[9749]]


def measure(binary, engine, workload, jobs, lanes):
    run = subprocess.run(
        [binary, engine, workload, str(jobs), str(lanes)],
        check=True, capture_output=True, text=True, timeout=90,
        env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"}, cwd="/",
    )
    cold, hot = map(int, run.stdout.split())
    return {"cold": cold / 1e6, "hot": hot / 1e6}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--pairs", type=int, default=16)
    parser.add_argument("--jobs", type=int, default=64)
    parser.add_argument("--lanes", type=int, default=8)
    args = parser.parse_args()
    if args.pairs < 4 or args.pairs % 2 or not 1 <= args.lanes <= min(args.jobs, 256):
        parser.error("use an even number of pairs >=4 and 1..min(jobs,256) lanes")
    binary = args.binary.resolve(strict=True)
    root = Path(__file__).resolve().parents[2]
    data = {
        "source_head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "rustc": subprocess.check_output(["rustc", "-Vv"], cwd=root, text=True).strip(),
        "kernel": platform.release(), "machine": platform.machine(),
        "cpu": next((l.split(":", 1)[1].strip() for l in Path("/proc/cpuinfo").read_text().splitlines()
                     if l.startswith("model name")), None),
        "affinity": sorted(os.sched_getaffinity(0)), "shell": os.path.realpath("/bin/sh"),
        "start_unix": time.time(), "load_before": os.getloadavg(), "pairs": args.pairs,
        "jobs": args.jobs, "lanes": args.lanes, "unit": "ms/batch", "samples": [],
    }
    for i in range(args.pairs):
        for workload in (("short", "output") if i % 2 == 0 else ("output", "short")):
            engines = ("zrun", "tokio")
            # Balance AB and BA per workload, independently of workload order.
            if (i + (workload == "output")) % 2:
                engines = engines[::-1]
            pair = {"pair": i + 1, "workload": workload, "order": engines}
            for engine in engines:
                pair[engine] = measure(str(binary), engine, workload, args.jobs, args.lanes)
            data["samples"].append(pair)
    data["load_after"] = os.getloadavg()
    data["end_unix"] = time.time()
    summary = {}
    for work in ("short", "output"):
        pairs = [pair for pair in data["samples"] if pair["workload"] == work]
        for phase in ("cold", "hot"):
            cell = {engine: spread([pair[engine][phase] for pair in pairs]) for engine in ("zrun", "tokio")}
            ratios = [pair["tokio"][phase] / pair["zrun"][phase] for pair in pairs]
            cell["tokio_ms_over_zrun_ms"] = spread(ratios)
            cell["paired_bootstrap_95pct"] = paired_interval(ratios)
            summary[f"{work}_{phase}"] = cell
    data["summary"] = summary
    args.out.write_text(json.dumps(data, indent=2) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
